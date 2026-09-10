//! Resolution changes requested by an X client, on their way to the compositor
//! (`-dynres`).
//!
//! A RandR client (RealVNC with the `dynres/` hook, or plain `xrandr`)
//! reconfigures a CRTC from an X connection thread, but only the Wayland thread
//! may touch the `wlr-output-management` objects that carry the change to the
//! compositor. So the X side parks its request here, wakes the Wayland thread,
//! and blocks until that thread reports how the compositor answered — exactly
//! as `RRSetCrtcConfig` blocks in Xorg while the driver mode-sets. The reply the
//! client gets is therefore truthful, and the RandR model never says something
//! the compositor did not do: the change reaches the model the ordinary way, as
//! a `wl_output` change, before the X side is woken.
//!
//! One request is handled at a time: RealVNC drives resizes from a single
//! connection, and a compositor configuration is single-use anyway.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A mode change for one Wayland output, in the screen's (transformed)
/// physical pixels, i.e. what the X client asked for.
#[derive(Clone, Copy, Debug)]
pub struct ModeRequest {
    pub id: u64,
    pub wl_name: u32,
    pub width: u16,
    pub height: u16,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// Parked by an X thread, not yet picked up by the Wayland thread.
    queued: Option<ModeRequest>,
    /// Picked up, and not yet completed.
    in_flight: Option<u64>,
    /// The last completed request, for its waiter.
    done: Option<(u64, Result<(), String>)>,
}

pub struct DynRes {
    inner: Mutex<Inner>,
    cv: Condvar,
    /// An eventfd the Wayland thread polls, so a parked request is picked up at
    /// once rather than at the end of the capture interval.
    wake: OwnedFd,
    /// Whether the Wayland thread has an output manager to carry changes over.
    available: AtomicBool,
}

impl Default for DynRes {
    fn default() -> Self {
        // SAFETY: eventfd takes no pointers; the fd is owned from here on
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        assert!(
            fd >= 0,
            "eventfd failed: {}",
            std::io::Error::last_os_error()
        );
        Self {
            inner: Mutex::new(Inner::default()),
            cv: Condvar::new(),
            // SAFETY: a fresh, valid fd nobody else owns
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
            available: AtomicBool::new(false),
        }
    }
}

impl DynRes {
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Release);
    }

    /// Whether a request stands any chance: the Wayland thread has bound an
    /// output manager. Checked by the X side so a request never waits on a
    /// thread that will not answer.
    pub fn available(&self) -> bool {
        self.available.load(Ordering::Acquire)
    }

    /// Asks the Wayland thread to drive `wl_name` at `width`x`height` and waits
    /// for the outcome. `Err` carries why it did not happen: the compositor
    /// refused, nothing answered within `timeout`, or another request is still
    /// in progress. Called from an X connection thread, never with the screen
    /// lock held (the Wayland thread needs it to finish).
    pub fn change_mode(
        &self,
        wl_name: u32,
        width: u16,
        height: u16,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap();
        // one at a time; a second client asking concurrently waits its turn
        while inner.queued.is_some() || inner.in_flight.is_some() {
            let now = Instant::now();
            if now >= deadline {
                return Err("another resolution change is still in progress".into());
            }
            inner = self.cv.wait_timeout(inner, deadline - now).unwrap().0;
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.queued = Some(ModeRequest {
            id,
            wl_name,
            width,
            height,
        });
        inner.done = None;
        self.wake_wayland();
        loop {
            if let Some((done_id, _)) = &inner.done
                && *done_id == id
            {
                return inner.done.take().unwrap().1;
            }
            let now = Instant::now();
            if now >= deadline {
                // if still queued, nobody will ever act on it; if in flight the
                // Wayland thread finishes on its own and the result is dropped
                if inner.queued.is_some_and(|r| r.id == id) {
                    inner.queued = None;
                }
                return Err("the compositor did not answer in time".into());
            }
            inner = self.cv.wait_timeout(inner, deadline - now).unwrap().0;
        }
    }

    fn wake_wayland(&self) {
        let one: u64 = 1;
        // SAFETY: writing 8 bytes from a u64 to an eventfd
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                (&one as *const u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }

    /// The fd to poll for a parked request (Wayland thread).
    pub fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Clears the wake fd after it polled readable (Wayland thread).
    pub fn drain_wake(&self) {
        let mut buf = 0u64;
        // SAFETY: reading 8 bytes into a u64 from an eventfd; EAGAIN is fine
        unsafe {
            libc::read(
                self.wake.as_raw_fd(),
                (&mut buf as *mut u64).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }

    /// Takes the parked request, if any, marking it in flight (Wayland thread).
    pub fn take(&self) -> Option<ModeRequest> {
        let mut inner = self.inner.lock().unwrap();
        if inner.in_flight.is_some() {
            return None;
        }
        let req = inner.queued.take()?;
        inner.in_flight = Some(req.id);
        Some(req)
    }

    /// Reports the outcome of the in-flight request and wakes its waiter
    /// (Wayland thread).
    pub fn complete(&self, id: u64, result: Result<(), String>) {
        let mut inner = self.inner.lock().unwrap();
        if inner.in_flight == Some(id) {
            inner.in_flight = None;
        }
        inner.done = Some((id, result));
        self.cv.notify_all();
    }
}
