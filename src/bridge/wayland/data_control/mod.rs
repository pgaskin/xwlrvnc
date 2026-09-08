//! Clipboard bridge over the data-control protocols: `ext-data-control-v1`
//! (preferred) or `zwlr-data-control-v1` (fallback).
//!
//! The two are equivalent for our purposes, so the manager-specific bits live
//! behind the [`DataControlManager`] trait, implemented per protocol in [`ext`]
//! and [`wlr`], and the X-to-Wayland source factory is written once over that
//! trait in [`install_source_factory`]. The `Dispatch` impls have to stay
//! concrete, but they all defer to [`State::update_selection`].
//!
//! Serving an X-owned selection to Wayland goes through [`PendingSend`], which
//! keeps the write off this thread's critical path; see [`State::queue_send`].

use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_seat;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::ExtDataControlDeviceV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1;

use super::*;

mod ext;
mod wlr;

/// Opcode of the `data_offer` event, which creates the child offer object. The
/// same value on both device interfaces.
const DATA_OFFER_OPCODE: u16 = 0;

/// How many `send` transfers may be in flight before the oldest is dropped. A
/// receiver that asks for the selection and never reads it would otherwise pin
/// a pipe forever, and a paste storm would pin one each.
const MAX_PENDING_SENDS: usize = 8;

/// How long a `send` may make no progress before we give up on it and close the
/// pipe (the receiver sees a truncated value, which beats holding the fd).
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The live device, whichever protocol won. Held rather than read: dropping the
/// proxy would destroy the device.
#[allow(dead_code)]
pub(crate) enum DataDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

/// The manager side of a data-control protocol: enough to create the per-seat
/// device, mint a source for an X-owned selection, and publish it.
trait DataControlManager: Clone + Send + Sync + 'static {
    type Device: Clone + Send + Sync + 'static;
    type Source: Clone;

    /// Binds the device for `seat`, kept alive in [`DataDevice`].
    fn create_device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> Self::Device;
    /// Mints a source tagged with `sel` as its `Dispatch` userdata.
    fn create_source(&self, qh: &QueueHandle<State>, sel: Sel) -> Self::Source;
    /// Advertises a mime type on the source.
    fn offer(source: &Self::Source, mime: String);
    /// Sets `sel`'s selection on the device to `source`.
    fn set_selection(device: &Self::Device, sel: Sel, source: &Self::Source);
}

/// Installs the source factory: when X takes a selection, mint a data-control
/// source advertising our text mimes and publish it.
fn install_source_factory<M: DataControlManager>(
    mgr: M,
    device: M::Device,
    conn: Connection,
    qh: QueueHandle<State>,
    clipboard: &clipboard::Clipboard,
) {
    clipboard.set_source_factory(Box::new(move |sel| {
        let source = mgr.create_source(&qh, sel);
        for m in clipboard::TEXT_MIMES {
            M::offer(&source, (*m).to_string());
        }
        M::set_selection(&device, sel, &source);
        let _ = conn.flush();
    }));
}

/// One in-flight data-control `send`: the receiving app's pipe, the value we
/// are feeding into it, and when we last managed to write something.
pub(super) struct PendingSend {
    fd: OwnedFd,
    data: Vec<u8>,
    offset: usize,
    progress_at: Instant,
}

impl PendingSend {
    /// Writes until the pipe is full, returning whether the transfer is over —
    /// either finished, or the receiver went away.
    fn pump(&mut self) -> bool {
        while self.offset < self.data.len() {
            let rest = &self.data[self.offset..];
            // SAFETY: writing `rest.len()` bytes from `rest` to an fd we own
            let n =
                unsafe { nix::libc::write(self.fd.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
            if n > 0 {
                self.offset += n as usize;
                self.progress_at = Instant::now();
                continue;
            }
            if n == 0 {
                return true; // can't happen on a pipe, but don't spin on it
            }
            return match std::io::Error::last_os_error().kind() {
                // the pipe is full or we were interrupted, so come back to it
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => false,
                // EPIPE (the receiver closed its end) or worse; nothing to do
                _ => true,
            };
        }
        true
    }
}

impl State {
    /// Starts feeding `data` to the pipe a data-control `send` handed us.
    ///
    /// The obvious `write_all` here would block this thread — and with it the
    /// whole event loop, so screen capture and input injection too — for as long
    /// as the receiving app takes to drain a value larger than the 64KiB pipe
    /// buffer, or forever if it never reads at all. So the pipe goes
    /// non-blocking and whatever doesn't fit is finished by the event loop in
    /// [`flush_pending_sends`](Self::flush_pending_sends).
    pub(super) fn queue_send(&mut self, fd: OwnedFd, data: Vec<u8>) {
        // SAFETY: the fd is ours, so O_NONBLOCK on it affects nobody else
        unsafe {
            let flags = nix::libc::fcntl(fd.as_raw_fd(), nix::libc::F_GETFL);
            if flags >= 0 {
                nix::libc::fcntl(
                    fd.as_raw_fd(),
                    nix::libc::F_SETFL,
                    flags | nix::libc::O_NONBLOCK,
                );
            }
        }
        let mut send = PendingSend {
            fd,
            data,
            offset: 0,
            progress_at: Instant::now(),
        };
        // the common case is a selection small enough to fit in the pipe buffer,
        // which finishes here and never reaches the queue at all
        if send.pump() {
            return;
        }
        crate::vlog!(
            "clipboard send did not fit the pipe; {} of {} bytes queued",
            send.data.len() - send.offset,
            send.data.len(),
        );
        if self.pending_sends.len() >= MAX_PENDING_SENDS {
            crate::warning!("too many clipboard sends in flight; dropping the oldest");
            self.pending_sends.remove(0);
        }
        self.pending_sends.push(send);
    }

    /// The pipes still being written, for the event loop's poll set, so a
    /// stalled send wakes us as soon as its receiver makes room.
    pub(super) fn pending_send_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.pending_sends.iter().map(|s| s.fd.as_raw_fd())
    }

    /// Writes whatever each in-flight send's pipe will take right now, dropping
    /// the ones that finish, fail, or stop making progress.
    pub(super) fn flush_pending_sends(&mut self) {
        let now = Instant::now();
        self.pending_sends.retain_mut(|s| {
            if s.pump() {
                return false;
            }
            if now.duration_since(s.progress_at) >= SEND_TIMEOUT {
                crate::warning!(
                    "clipboard send stalled with {} bytes left; giving up",
                    s.data.len() - s.offset,
                );
                return false;
            }
            true
        });
    }

    pub(super) fn try_init_device(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        // an ext device is already the best we can do
        if matches!(self.device, Some(DataDevice::Ext(_))) {
            return;
        }
        let Some(seat) = &self.seat else { return };

        if let Some(mgr) = &self.ext_manager {
            // ext is preferred, so replace any wlr device we already made
            crate::log!("using ext-data-control-v1");
            let device = mgr.create_device(seat, qh);
            install_source_factory(
                mgr.clone(),
                device.clone(),
                conn.clone(),
                qh.clone(),
                &self.server.clipboard,
            );
            self.device = Some(DataDevice::Ext(device));
        } else if self.device.is_none() {
            let Some(mgr) = &self.wlr_manager else { return };
            crate::log!("using zwlr-data-control-v1");
            let device = mgr.create_device(seat, qh);
            install_source_factory(
                mgr.clone(),
                device.clone(),
                conn.clone(),
                qh.clone(),
                &self.server.clipboard,
            );
            self.device = Some(DataDevice::Wlr(device));
        }
    }

    /// Records a selection change and notifies the X clients watching it.
    pub(super) fn update_selection(&mut self, sel: Sel, offer: Option<DataOffer>) {
        // with -noprimary, ignore PRIMARY entirely and just tidy up the offer
        // the compositor handed us
        if sel == Sel::Primary && self.server.config.noprimary {
            if let Some(o) = offer {
                self.offer_mimes.remove(&o.id());
                o.destroy();
            }
            return;
        }
        let mimes = offer
            .as_ref()
            .and_then(|o| self.offer_mimes.remove(&o.id()))
            .unwrap_or_default();
        // a foreign app taking the selection revokes any X owner, but the
        // compositor announces the source we published through here too
        if !self.server.clipboard.take_self_published(sel) {
            self.server.clipboard.clear_x_owner(sel);
        }
        let owner = if offer.is_some() {
            clipboard::OWNER_WINDOW
        } else {
            0
        };
        let (serial, old) = self.server.clipboard.set_offer(sel, offer, mimes);
        if let Some(old) = old {
            old.destroy();
        }
        self.server.events.selection_changed(sel, owner, serial);
    }
}
