//! A fake X11 server that backs `vncagent-x11` (RealVNC's Wayland-less agent)
//! with wlr-screencopy for the screen, the wlr virtual-pointer / zwp
//! virtual-keyboard protocols for input, and the Wayland data-control protocol
//! for the clipboard.
//!
//! Usage: `xwlrvnc vncagent-x11 [args...]` — we pick a free X display, point the
//! wrapped binary's `DISPLAY` at ourselves, and translate its X protocol into
//! Wayland.

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
/// Verbose detail (shown only with `-verbose`).
macro_rules! vlog { ($($arg:tt)*) => { $crate::logat!(2, $($arg)*) }; }
pub(crate) use {log, logat, vlog, warning};

#[macro_use]
mod x11arg;
mod capture;
mod clipboard;
mod config;
mod cursor;
mod damage;
mod event;
mod input;
mod keymap;
mod prof;
mod wayland;
mod x11;

use std::ffi::CString;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::{fs, io, process, ptr, thread};

use crate::config::Config;
use crate::input::Input;
use crate::x11::conn::{Connection, Server};
use crate::x11::screen::Screen;

/// The wrapped child's pid (0 until spawned), so the signal handler can forward
/// the terminating signal to it.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);
/// Pointer to a leaked C string of the filesystem X socket path, for the signal
/// handler to `unlink`. Null until we've bound a display.
static SOCKET_PATH: AtomicPtr<nix::libc::c_char> = AtomicPtr::new(ptr::null_mut());

/// Async-signal-safe terminating-signal handler. If the child exists, forward
/// the signal to it and return: the main thread is blocked in `child.wait()`,
/// which then returns and runs the normal cleanup — i.e. we wait for the child
/// to exit before exiting ourselves. Only if the signal arrives before the child
/// is spawned (nothing to wait for) do we clean up and exit here directly.
/// `kill`/`unlink`/`_exit` are all async-signal-safe and the socket path is a
/// preallocated C string, so this allocates nothing.
extern "C" fn handle_term(sig: nix::libc::c_int) {
    let pid = CHILD_PID.load(Ordering::Acquire);
    if pid > 0 {
        unsafe { nix::libc::kill(pid, sig) };
        return;
    }
    let path = SOCKET_PATH.load(Ordering::Acquire);
    if !path.is_null() {
        unsafe { nix::libc::unlink(path) };
    }
    unsafe { nix::libc::_exit(128 + sig) };
}

/// Installs [`handle_term`] for SIGINT/SIGTERM/SIGHUP and records the socket path
/// it should clean up. The child resets signal dispositions to default on exec,
/// so this only affects us, not the wrapped program.
fn install_cleanup(socket_path: &str) {
    let leaked = CString::new(socket_path).expect("socket path has no NUL").into_raw();
    SOCKET_PATH.store(leaked, Ordering::Release);
    unsafe {
        let mut sa: nix::libc::sigaction = std::mem::zeroed();
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
        if config.quiet { 0 } else if config.verbose { 2 } else { 1 },
        Ordering::Relaxed,
    );
    // Take the command out before `config` moves into the Server (which doesn't
    // need it). Without `-nowrap` a command is required.
    let command = std::mem::take(&mut config.command);
    if command.is_empty() && !config.nowrap {
        crate::warning!("no command given (use -nowrap to run without one); try -help");
        process::exit(2);
    }

    prof::start(config.profile);

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
        events: event::EventSink::default(),
        clipboard: clipboard::Clipboard::default(),
        framebuffer: Arc::new(capture::Framebuffer::default()),
        damage: damage::DamageSink::default(),
        cursor: cursor::CursorState::default(),
        keymap: Mutex::new(None),
    });

    wayland::spawn(server.clone());

    for listener in listeners {
        let server = server.clone();
        thread::spawn(move || accept_loop(listener, server));
    }

    // Signal readiness by writing the display number to -displayfd (like Xorg),
    // then closing it so a waiting parent sees EOF.
    if let Some(fd) = displayfd {
        use std::io::Write;
        use std::os::fd::FromRawFd;
        let mut f = unsafe { fs::File::from_raw_fd(fd as i32) };
        if writeln!(f, "{display}").is_err() {
            crate::warning!("failed to write display number to fd {fd}");
        }
    }

    // With -nowrap there's no child: run until a terminating signal, which the
    // handler cleans up after (the socket is removed there since CHILD_PID is 0).
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
    let mut child = match Command::new(program)
        .args(program_args)
        .env("DISPLAY", format!(":{display}"))
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_SESSION_TYPE", "x11")
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            crate::warning!("failed to spawn {program:?}: {e}");
            let _ = fs::remove_file(&socket_path);
            process::exit(1);
        }
    };
    CHILD_PID.store(child.id() as i32, Ordering::Release);

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

/// Binds the X sockets for a display number and returns its listeners. With
/// `forced`, only that display is tried (erroring if it's taken); otherwise the
/// first free number in 20..100 is used. Binds both the filesystem socket and
/// the abstract socket Xlib prefers on Linux.
fn bind_display(forced: Option<u32>) -> io::Result<(u32, Vec<UnixListener>)> {
    let _ = fs::create_dir_all("/tmp/.X11-unix");
    let candidates: Vec<u32> = match forced {
        Some(n) => vec![n],
        None => (20..100).collect(),
    };
    for n in candidates {
        let path = format!("/tmp/.X11-unix/X{n}");
        if fs::symlink_metadata(&path).is_ok() {
            if forced.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("display :{n} is already in use"),
                ));
            }
            continue; // already in use (possibly stale, but don't clobber it)
        }
        let fs_listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) if forced.is_some() => return Err(e),
            Err(_) => continue,
        };
        let mut listeners = vec![fs_listener];
        if let Ok(addr) = SocketAddr::from_abstract_name(path.as_bytes())
            && let Ok(abstract_listener) = UnixListener::bind_addr(&addr)
        {
            listeners.push(abstract_listener);
        }
        crate::log!("serving X display :{n}");
        return Ok((n, listeners));
    }
    Err(io::Error::new(io::ErrorKind::AddrInUse, "no free X display"))
}
