//! Fake X11 server that implements enough for RealVNC's vncagent-x11 (and most
//! other VNC server implementations) to work on wayland with screen capture,
//! virtual input, and clipboard synchronization.

/// Logging verbosity: 0 = quiet (warnings only), 1 = normal, 2 = verbose. Set
/// once at startup from `-quiet`/`-verbose`.
pub(crate) static LOG_LEVEL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Emits one `xwlrvnc:`-prefixed line to stderr if the current level is at least
/// `$min`. Same args as `format!`.
macro_rules! logat {
    ($min:expr, $($arg:tt)*) => {
        if $crate::LOG_LEVEL.load(::std::sync::atomic::Ordering::Relaxed) >= $min {
            eprintln!("xwlrvnc: {}", format_args!($($arg)*));
        }
    };
}
/// Informational log (shown unless `-quiet`).
macro_rules! log { ($($arg:tt)*) => { $crate::logat!(1, $($arg)*) }; }
/// Warning/error log (always shown).
macro_rules! warning {
    ($($arg:tt)*) => { eprintln!("xwlrvnc: {}", format_args!($($arg)*)) };
}
/// Unimplemented (and not explicitly stubbed) request/feature (always shown).
macro_rules! fixme {
    ($($arg:tt)*) => { eprintln!("xwlrvnc: fixme: {}", format_args!($($arg)*)) };
}
/// Verbose log (shown only with `-verbose`).
macro_rules! vlog { ($($arg:tt)*) => { $crate::logat!(2, $($arg)*) }; }
pub(crate) use {fixme, log, logat, vlog, warning};

#[macro_use]
mod util;
mod bridge;
mod config;

use std::ffi::CString;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, io, process, ptr, thread};

use crate::bridge::Server;
use crate::bridge::input::Input;
use crate::bridge::x11::conn::Connection;
use crate::bridge::x11::randr::Screen;
use crate::config::Config;

static CHILD_PID: AtomicI32 = AtomicI32::new(0); // for forwarding signals
static SPAWNING: AtomicBool = AtomicBool::new(false); // between starting the spawn and storing its pid
static PENDING_SIG: AtomicI32 = AtomicI32::new(0); // a signal that arrived while spawning, to forward
static SOCKET_PATH: AtomicPtr<nix::libc::c_char> = AtomicPtr::new(ptr::null_mut()); // leaked C string, to unlink on termination, null until display is bound

/// Async-signal-safe (does not allocate) terminating-signal handler. If the
/// child exists, forward the signal to it and return: the main thread is
/// blocked in `child.wait()`, which then returns and runs the normal cleanup
/// (i.e. we wait for the child to exit before exiting ourselves). While the
/// child is being spawned (any thread can take the signal, so no mask closes
/// this window) the signal is parked for main to forward once it has the pid.
/// If the signal arrives before that (nothing to wait for), we clean up and
/// exit here directly.
extern "C" fn handle_term(sig: nix::libc::c_int) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: does not allocate
        unsafe { nix::libc::kill(pid, sig) };
        return;
    }
    if SPAWNING.load(Ordering::SeqCst) {
        PENDING_SIG.store(sig, Ordering::SeqCst);
        return;
    }
    let path = SOCKET_PATH.load(Ordering::Acquire);
    if !path.is_null() {
        // SAFETY: does not allocate, path is a leaked C string
        unsafe { nix::libc::unlink(path) };
    }
    // SAFETY: does not allocate
    unsafe { nix::libc::_exit(128 + sig) };
}

fn install_cleanup(socket_path: &str) {
    let leaked = CString::new(socket_path)
        .expect("socket path has no NUL")
        .into_raw();
    SOCKET_PATH.store(leaked, Ordering::Release);
    unsafe {
        let mut sa: nix::libc::sigaction = std::mem::zeroed();
        // This only affects us since the child will reset signal dispositions
        // on exec.
        sa.sa_sigaction = handle_term as extern "C" fn(nix::libc::c_int) as usize;
        nix::libc::sigemptyset(&mut sa.sa_mask);
        for sig in [nix::libc::SIGINT, nix::libc::SIGTERM, nix::libc::SIGHUP] {
            nix::libc::sigaction(sig, &sa, ptr::null_mut());
        }
    }
}

fn main() {
    let mut config = Config::parse();
    LOG_LEVEL.store(
        if config.quiet {
            0
        } else if config.verbose {
            2
        } else {
            1
        },
        Ordering::Relaxed,
    );
    // Take the command out before `config` moves into the Server (which doesn't
    // need it). Without `-nowrap` a command is required.
    let command = std::mem::take(&mut config.command);
    if command.is_empty() && !config.nowrap {
        crate::warning!("no command given (use -nowrap to run without one); try -help");
        process::exit(2);
    }

    bridge::profile::start(config.profile);

    let (display, listeners) = match bind_display(config.display) {
        Ok(v) => v,
        Err(e) => {
            crate::warning!("failed to bind an X display: {e}");
            process::exit(1);
        }
    };
    let socket_path = format!("/tmp/.X11-unix/X{display}");
    // Clean up the filesystem socket on a terminating signal too, not just on the
    // normal child-exit path below.
    install_cleanup(&socket_path);

    let geom = config.geometry.unwrap_or_default();
    let displayfd = config.displayfd;
    let nowrap = config.nowrap;
    let input = Arc::new(Input::new(geom.width, geom.height));
    let server = Arc::new(Server {
        config,
        screen: Mutex::new(Screen::new(geom.width, geom.height)),
        input,
        events: bridge::event::EventSink::default(),
        clipboard: bridge::clipboard::Clipboard::default(),
        framebuffer: Arc::new(bridge::capture::Framebuffer::default()),
        damage: bridge::damage::DamageSink::default(),
        cursor: bridge::cursor::CursorState::default(),
        keymap: Mutex::new(None),
    });

    bridge::wayland::spawn(server.clone());

    for listener in listeners {
        let server = server.clone();
        thread::spawn(move || accept_loop(listener, server));
    }

    if let Some(fd) = displayfd {
        use std::io::Write;
        use std::os::fd::FromRawFd;
        let mut f = unsafe { fs::File::from_raw_fd(fd as i32) };
        if writeln!(f, "{display}").is_err() {
            crate::warning!("failed to write display number to fd {fd}");
        }
    }

    if nowrap {
        crate::log!("running without a command (-nowrap); waiting for a signal");
        loop {
            thread::park();
        }
    }

    // Audio needs no special handling: vncserver-x11-core locates the
    // PulseAudio/PipeWire server itself via the runtime path (/run/user/$UID/pulse
    // etc.) and reads the cookie from disk, so we don't advertise the X11
    // PULSE_SERVER/PULSE_COOKIE root-window properties (module-x11-publish style).
    //
    // Point the child at our X display, and make sure it sees an X11 session and
    // no Wayland: unset WAYLAND_DISPLAY so it can't connect to the compositor
    // directly, and set XDG_SESSION_TYPE=x11 so toolkits pick the X backend.
    let (program, program_args) = command.split_first().expect("checked non-empty above");
    SPAWNING.store(true, Ordering::SeqCst);
    let spawned = Command::new(program)
        .args(program_args)
        .env("DISPLAY", format!(":{display}"))
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_SESSION_TYPE", "x11")
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            crate::warning!("failed to spawn {program:?}: {e}");
            let _ = fs::remove_file(&socket_path);
            process::exit(1);
        }
    };
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    SPAWNING.store(false, Ordering::SeqCst);
    // a terminating signal that landed during the spawn goes to the child now,
    // as it would have a moment later
    let sig = PENDING_SIG.swap(0, Ordering::SeqCst);
    if sig != 0 {
        unsafe { nix::libc::kill(child.id() as i32, sig) };
    }

    let status = child.wait();
    let _ = fs::remove_file(&socket_path);
    match status {
        Ok(status) => process::exit(status.code().unwrap_or(1)),
        Err(e) => {
            crate::warning!("failed to wait for child: {e}");
            process::exit(1);
        }
    }
}

fn accept_loop(listener: UnixListener, server: Arc<Server>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = server.clone();
                thread::spawn(move || {
                    if let Err(e) = Connection::new(stream, server).and_then(Connection::run) {
                        crate::vlog!("connection closed: {e}");
                    }
                });
            }
            Err(e) => crate::warning!("accept failed: {e}"),
        }
    }
}

fn bind_display(forced: Option<u32>) -> io::Result<(u32, Vec<UnixListener>)> {
    let _ = fs::create_dir_all("/tmp/.X11-unix");
    let candidates: Vec<u32> = match forced {
        Some(n) => vec![n],
        None => (20..100).collect(),
    };
    for n in candidates {
        let path = format!("/tmp/.X11-unix/X{n}");
        match bind_sockets(&path) {
            Ok(listeners) => {
                crate::log!("serving X display :{n}");
                return Ok((n, listeners));
            }
            Err(e) if forced.is_some() => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("display :{n} is unavailable: {e}"),
                ));
            }
            Err(_) => continue,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "no free X display",
    ))
}

/// Binds one display's filesystem socket and its abstract twin. A socket file
/// nothing answers on (left by a killed server) is reclaimed; anything else in
/// the way, or another server holding the abstract name, means the display is
/// in use.
fn bind_sockets(path: &str) -> io::Result<Vec<UnixListener>> {
    match fs::symlink_metadata(path) {
        Err(_) => {}
        Ok(m) if !m.file_type().is_socket() => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{path} exists and is not a socket"),
            ));
        }
        Ok(_) => match UnixStream::connect(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another server is listening",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                crate::log!("removing stale socket {path}");
                fs::remove_file(path)?;
            }
            // permission denied and the like: someone else's socket
            Err(e) => return Err(e),
        },
    }
    let mut listeners = vec![UnixListener::bind(path)?];
    // libxcb tries the abstract name first, so another server holding it owns
    // the display whatever the filesystem says (abstract names die with their
    // process, so this one is never stale)
    match SocketAddr::from_abstract_name(path.as_bytes())
        .and_then(|addr| UnixListener::bind_addr(&addr))
    {
        Ok(l) => listeners.push(l),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            let _ = fs::remove_file(path);
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "another server holds the abstract socket",
            ));
        }
        Err(_) => {} // no abstract namespace; the filesystem socket suffices
    }
    Ok(listeners)
}
