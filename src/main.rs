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

/// Whether `-cliptrace` was given. A global like [`LOG_LEVEL`] rather than a
/// config lookup because the clipboard bridge spans the X connection threads,
/// the event sink and the Wayland thread, and the latter two hold no config.
pub(crate) static CLIP_TRACE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// One step of the clipboard/selection state machine (shown only with
/// `-cliptrace`). Enough of these to follow a copy end to end: who took a
/// selection, what we asked its owner for, what came back, and what we told
/// the compositor and the other X clients.
macro_rules! cliplog {
    ($($arg:tt)*) => {
        if $crate::CLIP_TRACE.load(::std::sync::atomic::Ordering::Relaxed) {
            eprintln!("xwlrvnc: clip: {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use {cliplog, fixme, log, logat, vlog, warning};

#[macro_use]
mod util;
mod bridge;
mod config;

use std::ffi::CString;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, io, process, ptr, thread};

use crate::bridge::Server;
use crate::bridge::input::Input;
use crate::bridge::x11::conn::Connection;
use crate::bridge::x11::randr::Screen;
use crate::config::Config;

/// The wrapped child, for forwarding signals: 0 before it is spawned, `SPAWNING`
/// while the spawn is in progress, its pid after. One word, so a signal handler
/// never sees a torn state (with the pid and a spawning flag as two atomics, a
/// handler that read pid 0 just before the spawn finished would then read
/// "not spawning" and exit under a live child).
static CHILD: AtomicI32 = AtomicI32::new(0);
const SPAWNING: i32 = -1;
static PENDING_SIG: AtomicI32 = AtomicI32::new(0); // a signal that arrived while spawning, to forward
static SOCKET_PATH: AtomicPtr<libc::c_char> = AtomicPtr::new(ptr::null_mut()); // leaked C string, to unlink on termination, null until display is bound

/// Async-signal-safe (does not allocate) terminating-signal handler. If the
/// child exists, forward the signal to it and return: the main thread is
/// blocked in `child.wait()`, which then returns and runs the normal cleanup
/// (i.e. we wait for the child to exit before exiting ourselves). While the
/// child is being spawned (any thread can take the signal, so no mask closes
/// this window) the signal is parked for main to forward once it has the pid.
/// If the signal arrives before that (nothing to wait for), we clean up and
/// exit here directly.
extern "C" fn handle_term(sig: libc::c_int) {
    let child = CHILD.load(Ordering::SeqCst);
    if child > 0 {
        // SAFETY: does not allocate
        unsafe { libc::kill(child, sig) };
        return;
    }
    if child == SPAWNING {
        PENDING_SIG.store(sig, Ordering::SeqCst);
        // The spawn may have finished between the load and the store, with
        // main having already swapped the (then empty) pending signal out; it
        // won't look again. So whoever swaps a non-zero signal out delivers
        // it, and exactly one of us does.
        let child = CHILD.load(Ordering::SeqCst);
        if child > 0 {
            let sig = PENDING_SIG.swap(0, Ordering::SeqCst);
            if sig != 0 {
                // SAFETY: does not allocate
                unsafe { libc::kill(child, sig) };
            }
        }
        return;
    }
    let path = SOCKET_PATH.load(Ordering::Acquire);
    if !path.is_null() {
        // SAFETY: does not allocate, path is a leaked C string
        unsafe { libc::unlink(path) };
    }
    // SAFETY: does not allocate
    unsafe { libc::_exit(128 + sig) };
}

fn install_cleanup(socket_path: &str) {
    let leaked = CString::new(socket_path)
        .expect("socket path has no NUL")
        .into_raw();
    SOCKET_PATH.store(leaked, Ordering::Release);
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        // This only affects us since the child will reset signal dispositions
        // on exec.
        sa.sa_sigaction = handle_term as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaction(sig, &sa, ptr::null_mut());
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
    CLIP_TRACE.store(config.cliptrace, Ordering::Relaxed);
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
    let dynres = config.dynres;
    let input = Arc::new(Input::new(geom.width, geom.height));
    let server = Arc::new(Server {
        config,
        atoms: Mutex::new(bridge::x11::atom::Atoms::default()),
        screen: Mutex::new(Screen::new(geom.width, geom.height)),
        input,
        events: bridge::event::EventSink::default(),
        clipboard: bridge::clipboard::Clipboard::default(),
        framebuffer: Arc::new(bridge::capture::Framebuffer::default()),
        damage: bridge::damage::DamageSink::default(),
        cursor: bridge::cursor::CursorState::default(),
        dynres: bridge::dynres::DynRes::default(),
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
    let mut cmd = Command::new(program);
    cmd.args(program_args)
        .env("DISPLAY", format!(":{display}"))
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_SESSION_TYPE", "x11");
    if dynres {
        // what the dynres hook (see dynres/) looks for before it patches out
        // RealVNC's "-virtual" check and lets the feature turn on
        cmd.env("FORCE_DYNRES", "1");
    }
    CHILD.store(SPAWNING, Ordering::SeqCst);
    let spawned = cmd.spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            CHILD.store(0, Ordering::SeqCst);
            crate::warning!("failed to spawn {program:?}: {e}");
            let _ = fs::remove_file(&socket_path);
            process::exit(1);
        }
    };
    CHILD.store(child.id() as i32, Ordering::SeqCst);
    // a terminating signal that landed during the spawn goes to the child now,
    // as it would have a moment later (see handle_term for why the swap)
    let sig = PENDING_SIG.swap(0, Ordering::SeqCst);
    if sig != 0 {
        unsafe { libc::kill(child.id() as i32, sig) };
    }

    let status = child.wait();
    // the pid is reaped and could be reused; a signal from here on exits us
    // directly, which is all that is left to do
    CHILD.store(0, Ordering::SeqCst);
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
    // SAFETY: getuid cannot fail
    let uid = unsafe { libc::getuid() };
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // There is no X authentication, and the abstract socket has no
                // filesystem permissions either, so the peer's uid is all that
                // keeps other local users off the display (and its screen,
                // input and clipboard). Only our own user, and root, get in.
                match peer_uid(&stream) {
                    Ok(peer) if peer == uid || peer == 0 => {}
                    Ok(peer) => {
                        crate::warning!("rejecting X connection from uid {peer}");
                        continue;
                    }
                    Err(e) => {
                        crate::warning!("rejecting X connection: cannot get peer credentials: {e}");
                        continue;
                    }
                }
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

/// The uid of the process at the other end of a unix socket.
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED fills in a ucred, and we pass its size
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
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
