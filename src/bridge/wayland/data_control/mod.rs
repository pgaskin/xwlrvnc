//! Clipboard bridge over the data-control protocols: `ext-data-control-v1`
//! (preferred) or `zwlr-data-control-v1` (fallback).
//!
//! The two are equivalent for our purposes, so the manager-specific bits live
//! behind the [`DataControlManager`] trait, implemented per protocol in [`ext`]
//! and [`wlr`], and the X-to-Wayland publisher is written once over that trait
//! in [`publisher`]. The `Dispatch` impls have to stay concrete, but they all
//! defer to [`State::update_selection`].
//!
//! Serving an X-owned selection to Wayland goes through [`PendingSend`], which
//! keeps the write off this thread's critical path; see [`State::queue_send`].
//! Serving a Wayland-owned selection to X is its mirror image, [`PendingReceive`]
//! — and since an X thread may not touch the offer to start one, both ends of
//! that transfer belong to this thread: see [`State::run_clip_jobs`].

use std::os::fd::{AsFd, RawFd};
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_seat;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::ExtDataControlDeviceV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1;

use super::*;
use crate::bridge::clipboard::Published;
use crate::bridge::clipjobs::{Job, PendingConversion};

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

/// How many conversions may be reading from Wayland apps at once. One X client
/// polling a wedged app is the realistic case; past this the rest are refused
/// rather than queued, which is what X does when nobody answers.
const MAX_PENDING_RECEIVES: usize = 8;

/// How long a receive may make no progress before we give up on it.
///
/// The app is handed the write end of a pipe and can simply never use it —
/// wedged, stopped, or waiting on something of its own. The X client that
/// asked is no longer waiting on this thread, but it *is* waiting for its
/// `SelectionNotify`, and an answer it never gets is a paste that never
/// happens.
const RECEIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a conversion may take in all, queueing here included, however
/// steadily it progresses. The idle budget alone lets an app that trickles a
/// byte every few seconds hold a pipe (and the requestor) indefinitely.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest selection we will carry in either direction. It is buffered
/// whole, then copied into a property and a `GetProperty` reply, so this is
/// really three times as much; it matches the per-client outbox cap, past
/// which the reply could not be delivered anyway.
const MAX_SELECTION_BYTES: usize = 64 << 20;

/// The live device, whichever protocol won. Held so it can be destroyed if ext
/// replaces wlr: dropping the proxy does not send `destroy`, so the compositor
/// keeps delivering selection events to it.
#[allow(dead_code)] // the ext variant is only ever matched, never read
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
    /// Mints a source carrying `published` (the selection and its value) as
    /// its `Dispatch` userdata.
    fn create_source(&self, qh: &QueueHandle<State>, published: Published) -> Self::Source;
    /// Advertises a mime type on the source.
    fn offer(source: &Self::Source, mime: String);
    /// Sets `sel`'s selection on the device to `source`, or clears it with
    /// `None` when X owns a selection whose value we could not get. Only ever
    /// called for a selection the device [carries](Self::carries).
    fn set_selection(device: &Self::Device, sel: Sel, source: Option<&Self::Source>);
    /// Whether the device has `sel` at all: a wlr v1 device has no primary
    /// selection. Asked rather than inferred from a failed `set_selection`,
    /// which would wipe the compositor's clipboard to find out.
    fn carries(device: &Self::Device, sel: Sel) -> bool;
}

/// Builds the publisher run for a [`Job::Publish`]: mint a data-control source
/// advertising our text mimes and set it as the selection, or withdraw ours.
///
/// Also tells the clipboard which selections this device carries at all, so
/// that a publication to one it does not (a wlr v1 device has no primary
/// selection) is dropped before it is ever recorded as owed an echo.
fn publisher<M: DataControlManager>(
    mgr: M,
    device: M::Device,
    conn: Connection,
    qh: QueueHandle<State>,
    clipboard: &clipboard::Clipboard,
) -> clipboard::Publisher {
    for sel in [Sel::Clipboard, Sel::Primary] {
        clipboard.set_publishable(sel, M::carries(&device, sel));
    }
    Box::new(move |sel, data| {
        let source = data.map(|data| {
            let source = mgr.create_source(&qh, Published { sel, data });
            // the mimes have to be offered before the source is used, or the
            // compositor takes it for a protocol error
            for m in clipboard::TEXT_MIMES {
                M::offer(&source, (*m).to_string());
            }
            source
        });
        M::set_selection(&device, sel, source.as_ref());
        let _ = conn.flush();
    })
}

/// One in-flight data-control `send`: the receiving app's pipe, the value we
/// are feeding into it, and when we last managed to write something.
pub(super) struct PendingSend {
    fd: OwnedFd,
    data: Arc<[u8]>,
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
            let n = unsafe { libc::write(self.fd.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
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

/// One conversion in flight: the pipe the owning app is writing its value
/// into, what has arrived so far, and the X requestor waiting for it.
///
/// The mirror image of [`PendingSend`], and for the same reason: reading to
/// EOF here would block the event loop for as long as the app takes, so the
/// pipe goes non-blocking and the loop pumps it (see
/// [`flush_pending_receives`](State::flush_pending_receives)).
pub(super) struct PendingReceive {
    fd: OwnedFd,
    sel: Sel,
    mime: String,
    /// The offer we asked, to tell an offer replaced under us from an app that
    /// really has nothing to say.
    offer: DataOffer,
    buf: Vec<u8>,
    /// When the X client asked, not when this thread got to it: the total
    /// budget covers the wait for this thread as well as the transfer.
    since: Instant,
    progress_at: Instant,
    /// The budgets, as fields so the tests need not wait out the real ones.
    idle: Duration,
    total: Duration,
    max: usize,
    conversion: PendingConversion,
}

/// Why a [`PendingReceive`] stopped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReadEnd {
    /// Every writer closed its end: the value is complete.
    Eof,
    /// Nothing arrived for the idle budget.
    Stalled,
    /// The total budget ran out, however steadily it came.
    TooSlow,
    /// More than [`MAX_SELECTION_BYTES`].
    TooBig,
}

impl PendingReceive {
    /// Reads whatever the pipe holds right now, returning why the transfer
    /// ended or `None` if it is still going. The idle budget restarts on every
    /// byte, so a large but slow transfer is only ever cut short by the total.
    fn pump(&mut self, now: Instant) -> Option<ReadEnd> {
        let mut chunk = [0u8; 8192];
        loop {
            // SAFETY: reading at most `chunk.len()` bytes into `chunk` from our fd
            let n =
                unsafe { libc::read(self.fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
            if n == 0 {
                return Some(ReadEnd::Eof); // EOF: every writer is done
            }
            if n > 0 {
                self.buf.extend_from_slice(&chunk[..n as usize]);
                self.progress_at = now;
                if self.buf.len() > self.max {
                    return Some(ReadEnd::TooBig);
                }
                continue;
            }
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::WouldBlock => break,
                std::io::ErrorKind::Interrupted => continue,
                // nothing left to read from and nothing to be done about it
                _ => return Some(ReadEnd::Eof),
            }
        }
        if now.duration_since(self.since) >= self.total {
            return Some(ReadEnd::TooSlow);
        }
        if now.duration_since(self.progress_at) >= self.idle {
            return Some(ReadEnd::Stalled);
        }
        None
    }

    /// Answers the requestor. Anything short of a clean EOF is a refusal
    /// rather than a truncated value: the client would have no way of telling
    /// the tail of its paste had been lost.
    fn finish(self, end: ReadEnd, clipboard: &clipboard::Clipboard) {
        let took = self.since.elapsed().as_millis();
        let why = match end {
            ReadEnd::Eof => None,
            ReadEnd::Stalled => Some("the source app wrote and then stopped"),
            ReadEnd::TooSlow => Some("the source app is still writing"),
            ReadEnd::TooBig => Some("the value is bigger than we carry"),
        };
        if let Some(why) = why {
            crate::warning!(
                "clipboard read of {:?} {} gave up after {took}ms and {} bytes: {why}",
                self.sel,
                self.mime,
                self.buf.len(),
            );
            return self.conversion.finish(None);
        }
        // a request on an offer the compositor has replaced is dropped on the
        // floor, which reads as an instant EOF; delivering that as an empty
        // selection would wipe the requestor's paste
        if self.buf.is_empty() && !clipboard.is_current(self.sel, &self.offer) {
            crate::cliplog!(
                "read {:?} {}: the offer was replaced while asking",
                self.sel,
                self.mime
            );
            return self.conversion.finish(None);
        }
        crate::cliplog!(
            "read {:?} {}: {} bytes in {took}ms",
            self.sel,
            self.mime,
            self.buf.len(),
        );
        self.conversion.finish(Some(self.buf));
    }
}

/// Puts a pipe of ours in non-blocking mode, reporting whether it worked.
///
/// Everything pumped from the Wayland loop depends on this. A pipe left
/// blocking stalls that thread — screen capture and input injection with it,
/// not just the clipboard — until the app on the far end writes or closes,
/// which is exactly what a wedged app never does. On a pipe created moments ago
/// it cannot realistically fail, but the failure is silent and the consequence
/// is the whole session, so it is worth the branch.
fn set_nonblocking(fd: &OwnedFd) -> bool {
    // SAFETY: the fd is ours, so O_NONBLOCK on it affects nobody else
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return false;
    }
    // SAFETY: as above
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0 }
}

impl State {
    /// Runs everything the X threads have posted (see
    /// [`Jobs`](crate::bridge::clipjobs::Jobs)). Every data-control request in
    /// the bridge is issued from here or from a `Dispatch` impl, so no other
    /// thread ever touches an offer, a source, or the device.
    pub(super) fn run_clip_jobs(&mut self, conn: &Connection) {
        for job in self.server.clipboard.jobs().take() {
            match job {
                Job::Publish(sel, data) => match &self.publish {
                    Some(publish) => publish(sel, data),
                    // the seat (or the protocol) never turned up, so there is
                    // no device to publish on; the clipboard is told as much
                    // and does not expect an echo, but a job already in flight
                    // can still land here
                    None => crate::cliplog!("{sel:?}: no data-control device to publish on"),
                },
                Job::Receive {
                    sel,
                    mime,
                    generation,
                    since,
                    conversion,
                } => self.start_receive(conn, sel, mime, generation, since, conversion),
                // dropping a proxy does not send `destroy`, and the protocol
                // asks us to destroy a replaced offer
                Job::Destroy(offers) => {
                    for offer in offers {
                        offer.destroy();
                    }
                }
            }
        }
    }

    /// Asks the app owning `sel` for its value and parks the transfer, to be
    /// finished by [`flush_pending_receives`](Self::flush_pending_receives).
    /// Anything that stops it here refuses the conversion, by dropping it.
    fn start_receive(
        &mut self,
        conn: &Connection,
        sel: Sel,
        mime: String,
        generation: u64,
        since: Instant,
        conversion: PendingConversion,
    ) {
        let Some(offer) = self.server.clipboard.offer_at(sel, generation) else {
            // either no Wayland app owns it any more, or a different one does:
            // asking a new owner for a mime resolved against the old one is
            // refused by the compositor as an instant EOF, which would reach
            // the requestor as a successful empty paste
            crate::cliplog!("{sel:?} is no longer the owner the conversion was for; refused");
            return;
        };
        if self.pending_receives.len() >= MAX_PENDING_RECEIVES {
            crate::warning!("too many clipboard reads in flight; refusing this one");
            return;
        }
        let Ok((rx, tx)) = std::io::pipe() else {
            crate::warning!("could not make a pipe for a clipboard read");
            return;
        };
        offer.receive(mime.clone(), tx.as_fd());
        // the request has to reach the compositor before we wait on the pipe,
        // exactly as wl-paste flushes before reading
        let _ = conn.flush();
        drop(tx); // so we see EOF once the app finishes writing
        let fd = OwnedFd::from(rx);
        if !set_nonblocking(&fd) {
            // reading it would block this thread, and the whole session with
            // it; dropping `conversion` on the way out refuses the requestor
            crate::warning!(
                "could not make a clipboard read non-blocking ({}); refusing it",
                std::io::Error::last_os_error()
            );
            return;
        }
        self.pending_receives.push(PendingReceive {
            fd,
            sel,
            mime,
            offer,
            buf: Vec::new(),
            since,
            progress_at: Instant::now(),
            idle: RECEIVE_IDLE_TIMEOUT,
            total: RECEIVE_TIMEOUT,
            max: MAX_SELECTION_BYTES,
            conversion,
        });
    }

    /// The pipes still being read, for the event loop's poll set, so a value
    /// arriving wakes us rather than waiting out the capture interval.
    pub(super) fn pending_receive_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.pending_receives.iter().map(|r| r.fd.as_raw_fd())
    }

    /// Reads whatever each in-flight conversion's pipe holds right now,
    /// answering the requestors whose values are complete (or whose apps gave
    /// up on them).
    pub(super) fn flush_pending_receives(&mut self) {
        let now = Instant::now();
        let server = Arc::clone(&self.server);
        let mut i = 0;
        while i < self.pending_receives.len() {
            match self.pending_receives[i].pump(now) {
                Some(end) => self
                    .pending_receives
                    .remove(i)
                    .finish(end, &server.clipboard),
                None => i += 1,
            }
        }
    }

    /// Starts feeding `data` to the pipe a data-control `send` handed us.
    ///
    /// The obvious `write_all` here would block this thread — and with it the
    /// whole event loop, so screen capture and input injection too — for as long
    /// as the receiving app takes to drain a value larger than the 64KiB pipe
    /// buffer, or forever if it never reads at all. So the pipe goes
    /// non-blocking and whatever doesn't fit is finished by the event loop in
    /// [`flush_pending_sends`](Self::flush_pending_sends).
    pub(super) fn queue_send(&mut self, fd: OwnedFd, data: Arc<[u8]>) {
        if !set_nonblocking(&fd) {
            // writing it could block this thread indefinitely; closing the pipe
            // instead gives the receiving app an EOF, which it can act on
            crate::warning!(
                "could not make a clipboard send non-blocking ({}); dropping it",
                std::io::Error::last_os_error()
            );
            return;
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
        let Some(seat) = &self.clipboard_seat else {
            return;
        };

        if let Some(mgr) = &self.ext_manager {
            // ext is preferred, so replace any wlr device we already made
            crate::log!("using ext-data-control-v1");
            let device = mgr.create_device(seat, qh);
            self.publish = Some(publisher(
                mgr.clone(),
                device.clone(),
                conn.clone(),
                qh.clone(),
                &self.server.clipboard,
            ));
            // Destroy the wlr device, not just drop it: the compositor would
            // otherwise announce every selection change on both devices, and the
            // second `update_selection` would clear the X owner and notify X
            // clients of a change that didn't happen.
            if let Some(DataDevice::Wlr(old)) = self.device.replace(DataDevice::Ext(device)) {
                old.destroy();
            }
        } else if self.device.is_none() {
            let Some(mgr) = &self.wlr_manager else { return };
            crate::log!("using zwlr-data-control-v1");
            let device = mgr.create_device(seat, qh);
            self.publish = Some(publisher(
                mgr.clone(),
                device.clone(),
                conn.clone(),
                qh.clone(),
                &self.server.clipboard,
            ));
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
        // the compositor announces the source we published (and the one we
        // withdrew) through here too, which the state machine tells apart; all
        // that reaches X is what it hands back
        self.server
            .clipboard
            .wayland_announced(sel, offer, mimes)
            .apply(&self.server, sel);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{PipeReader, Write};
    use std::sync::Mutex;

    use super::*;
    use crate::bridge::clipboard::Clipboard;

    const SHORT: Duration = Duration::from_millis(150);
    const LONG: Duration = Duration::from_secs(10);

    /// What the X requestor was answered with: `None` until it has been,
    /// `Some(None)` for a refusal.
    type Answer = Arc<Mutex<Option<Option<Vec<u8>>>>>;

    /// A conversion reading `rx`, with the budgets spelled out so the tests
    /// need not wait out the real ones.
    fn receiving(
        rx: PipeReader,
        idle: Duration,
        total: Duration,
        max: usize,
    ) -> (PendingReceive, Answer) {
        let answer: Answer = Arc::default();
        let slot = answer.clone();
        let now = Instant::now();
        let r = PendingReceive {
            fd: OwnedFd::from(rx),
            sel: Sel::Clipboard,
            mime: "text/plain".to_string(),
            offer: DataOffer::Fake,
            buf: Vec::new(),
            since: now,
            progress_at: now,
            idle,
            total,
            max,
            conversion: PendingConversion::new(move |data| *slot.lock().unwrap() = Some(data)),
        };
        assert!(
            set_nonblocking(&r.fd),
            "a fresh pipe should go non-blocking"
        );
        (r, answer)
    }

    /// Pumps as the event loop does (which polls the pipe, so the sleep here
    /// stands in for waiting on it) until the transfer ends.
    fn until_done(r: &mut PendingReceive) -> ReadEnd {
        loop {
            if let Some(end) = r.pump(Instant::now()) {
                return end;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A clipboard the fake offer is the current selection of, so a completed
    /// read is delivered rather than taken for a replaced offer's instant EOF.
    fn clipboard_offering() -> Clipboard {
        let c = Clipboard::default();
        drop(c.wayland_announced(
            Sel::Clipboard,
            Some(DataOffer::Fake),
            vec!["text/plain".into()],
        ));
        c
    }

    #[test]
    fn reads_until_the_writer_closes() {
        let (rx, mut tx) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            tx.write_all(b"hello ").unwrap();
            std::thread::sleep(Duration::from_millis(20));
            tx.write_all(b"world").unwrap();
            // dropping tx is the EOF
        });
        let (mut r, answer) = receiving(rx, SHORT, LONG, usize::MAX);
        assert_eq!(until_done(&mut r), ReadEnd::Eof);
        assert_eq!(r.buf, b"hello world");
        // and the requestor gets it, without ever having waited on a thread
        r.finish(ReadEnd::Eof, &clipboard_offering());
        assert_eq!(
            answer.lock().unwrap().as_ref(),
            Some(&Some(b"hello world".to_vec()))
        );
    }

    #[test]
    fn gives_up_on_a_source_that_never_writes() {
        // the writer stays open for the whole test, as a wedged app's would
        let (rx, _tx) = std::io::pipe().unwrap();
        let t0 = Instant::now();
        let (mut r, answer) = receiving(rx, SHORT, LONG, usize::MAX);
        assert_eq!(until_done(&mut r), ReadEnd::Stalled);
        assert!(
            t0.elapsed() >= SHORT,
            "gave up too early: {:?}",
            t0.elapsed()
        );
        // a refusal, not an empty paste, and the requestor is told at once
        r.finish(ReadEnd::Stalled, &clipboard_offering());
        assert_eq!(answer.lock().unwrap().as_ref(), Some(&None));
    }

    #[test]
    fn gives_up_on_a_source_that_stops_partway() {
        let (rx, mut tx) = std::io::pipe().unwrap();
        let held = std::thread::spawn(move || {
            tx.write_all(b"half a value").unwrap();
            std::thread::sleep(Duration::from_secs(3)); // and then nothing
            drop(tx);
        });
        let (mut r, answer) = receiving(rx, SHORT, LONG, usize::MAX);
        assert_eq!(until_done(&mut r), ReadEnd::Stalled);
        assert_eq!(r.buf, b"half a value", "what arrived is still here");
        r.finish(ReadEnd::Stalled, &clipboard_offering());
        assert_eq!(
            answer.lock().unwrap().as_ref(),
            Some(&None),
            "half a value is worse than none: the client cannot tell"
        );
        let _ = held.join();
    }

    #[test]
    fn the_idle_budget_restarts_on_every_chunk() {
        // each write lands well inside SHORT, but the whole transfer takes
        // longer than it: a slow-but-progressing source must not be cut off
        let (rx, mut tx) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            for _ in 0..6 {
                std::thread::sleep(Duration::from_millis(50));
                tx.write_all(b"x").unwrap();
            }
        });
        let (mut r, _a) = receiving(rx, SHORT, LONG, usize::MAX);
        assert_eq!(
            until_done(&mut r),
            ReadEnd::Eof,
            "a progressing transfer was cut short"
        );
        assert_eq!(r.buf, b"xxxxxx");
    }

    #[test]
    fn the_total_budget_does_not() {
        // a trickle that never stalls still ends when the total runs out
        let (rx, mut tx) = std::io::pipe().unwrap();
        let held = std::thread::spawn(move || {
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(50));
                if tx.write_all(b"x").is_err() {
                    break;
                }
            }
        });
        let t0 = Instant::now();
        let (mut r, _a) = receiving(rx, SHORT, Duration::from_millis(400), usize::MAX);
        assert_eq!(until_done(&mut r), ReadEnd::TooSlow);
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "took {:?}",
            t0.elapsed()
        );
        drop(r);
        let _ = held.join();
    }

    #[test]
    fn a_value_past_the_size_cap_is_refused() {
        let (rx, mut tx) = std::io::pipe().unwrap();
        let held = std::thread::spawn(move || {
            let _ = tx.write_all(&[b'x'; 20_000]);
        });
        let (mut r, _a) = receiving(rx, SHORT, LONG, 10_000);
        assert_eq!(until_done(&mut r), ReadEnd::TooBig);
        drop(r);
        let _ = held.join();
    }

    #[test]
    fn an_empty_selection_is_a_value_but_a_replaced_offer_is_not() {
        // wl-copy '' is a real, empty selection; an offer the compositor has
        // replaced answers with an instant EOF that looks exactly like it
        let (rx, tx) = std::io::pipe().unwrap();
        drop(tx);
        let (mut r, answer) = receiving(rx, SHORT, LONG, usize::MAX);
        assert_eq!(until_done(&mut r), ReadEnd::Eof);
        r.finish(ReadEnd::Eof, &clipboard_offering());
        assert_eq!(answer.lock().unwrap().as_ref(), Some(&Some(Vec::new())));

        let (rx, tx) = std::io::pipe().unwrap();
        drop(tx);
        let (mut r, answer) = receiving(rx, SHORT, LONG, usize::MAX);
        assert_eq!(until_done(&mut r), ReadEnd::Eof);
        r.finish(ReadEnd::Eof, &Clipboard::default()); // nothing offered any more
        assert_eq!(answer.lock().unwrap().as_ref(), Some(&None));
    }

    #[test]
    fn a_dropped_transfer_still_answers_its_requestor() {
        // the event loop ending, the queue overflowing, the compositor going
        // away: whatever happens to a transfer, the X client hears back
        let (rx, _tx) = std::io::pipe().unwrap();
        let (r, answer) = receiving(rx, SHORT, LONG, usize::MAX);
        drop(r);
        assert_eq!(answer.lock().unwrap().as_ref(), Some(&None));
    }
}
