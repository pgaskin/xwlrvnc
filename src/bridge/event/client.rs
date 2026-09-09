use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::time::Duration;

use crate::bridge::clipboard::Sel;

/// How long a write may block before the client is given up on. Events are
/// pushed from the Wayland thread, so a client that stops reading (stopped, or
/// wedged) with a full socket buffer would otherwise stall screen capture and
/// the clipboard for every other client too. An X server buffers per client
/// instead; this is the cheap approximation.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The socket plus the last 16-bit sequence written to it.
///
/// libxcb widens the wire sequence to 64-bit and assumes a wrap whenever it
/// appears to go *backwards*, bumping by 0x10000. An event from a background
/// thread carrying a sequence lower than a reply already written would therefore
/// be widened past the last request the client sent, and libxcb aborts with
/// `xcb_xlib_threads_sequence_lost`. So every write goes through here.
struct SeqWriter {
    stream: UnixStream,
    last_seq: u16,
}

impl SeqWriter {
    /// Clamps `seq` so the wire sequence never decreases: if `seq` is behind
    /// `last_seq` (within half the 16-bit range), reuse `last_seq`.
    fn monotonic(&self, seq: u16) -> u16 {
        if (seq.wrapping_sub(self.last_seq) as i16) >= 0 {
            seq
        } else {
            self.last_seq
        }
    }
}

/// A client's XFixes registration for one selection's owner.
struct SelReg {
    kind: Sel,
    atom: u32,
    window: u32,
}

/// One connection, as seen by [`EventSink`](super::EventSink): the guarded
/// socket, plus what the client has asked to be notified about.
pub struct Client {
    writer: Mutex<SeqWriter>,
    seq: AtomicU16,           // the client's most recent request sequence
    randr_window: AtomicU32,  // selected RandR ScreenChangeNotify, 0 if none
    cursor_window: AtomicU32, // selected XFixes cursor changes, 0 if none
    selections: Mutex<Vec<SelReg>>,
    dead: AtomicBool,
    /// Whether the client selected StructureNotify on the root window, and so
    /// wants a root ConfigureNotify on resize. This, not RandR SelectInput, is
    /// how vncagent notices the screen changed size.
    root_structure: AtomicBool,
}

impl Client {
    pub fn new(writer: UnixStream) -> Self {
        // the option is on the socket, so the connection thread's reads on its
        // duplicate fd are unaffected
        let _ = writer.set_write_timeout(Some(WRITE_TIMEOUT));
        Self {
            writer: Mutex::new(SeqWriter {
                stream: writer,
                last_seq: 0,
            }),
            seq: AtomicU16::new(0),
            randr_window: AtomicU32::new(0),
            cursor_window: AtomicU32::new(0),
            selections: Mutex::new(Vec::new()),
            dead: AtomicBool::new(false),
            root_structure: AtomicBool::new(false),
        }
    }

    /// Writes the connection-setup bytes, the only write with no sequence.
    pub fn write_setup(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.lock().unwrap().stream.write_all(bytes)
    }

    /// Writes a framed reply or error, stamping `seq` (clamped monotonic) into
    /// its sequence field at `[2..4]`.
    pub fn send_reply(&self, seq: u16, buf: &mut [u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        let seq = w.monotonic(seq);
        buf[2..4].copy_from_slice(&seq.to_le_bytes());
        w.last_seq = seq;
        w.stream.write_all(buf)
    }

    /// Writes a server-initiated event, stamping the client's current request
    /// sequence under the writer lock so it can't race a concurrent reply. A
    /// failed write (the client went away, or timed out) drops the client: it
    /// is marked dead and its socket shut down, so the connection thread's next
    /// read ends and it cleans up.
    pub fn send_event_bytes(&self, buf: &mut [u8]) {
        let mut w = self.writer.lock().unwrap();
        let seq = w.monotonic(self.seq.load(Ordering::Relaxed));
        buf[2..4].copy_from_slice(&seq.to_le_bytes());
        w.last_seq = seq;
        if let Err(e) = w.stream.write_all(buf) {
            crate::vlog!("dropping client: event write failed: {e}");
            self.mark_dead();
            let _ = w.stream.shutdown(Shutdown::Both);
        }
    }

    pub fn set_seq(&self, seq: u16) {
        self.seq.store(seq, Ordering::Relaxed);
    }

    pub fn seq(&self) -> u16 {
        self.seq.load(Ordering::Relaxed)
    }

    pub fn mark_dead(&self) {
        self.dead.store(true, Ordering::Relaxed);
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    pub fn select_randr(&self, window: u32) {
        self.randr_window.store(window, Ordering::Relaxed);
    }

    pub fn select_cursor(&self, window: u32) {
        self.cursor_window.store(window, Ordering::Relaxed);
    }

    pub fn select_root_structure(&self, enable: bool) {
        self.root_structure.store(enable, Ordering::Relaxed);
    }

    /// Registers, or with `enable` false clears, XFixes selection-owner
    /// notifications for one selection kind.
    pub fn select_selection(&self, kind: Sel, atom: u32, window: u32, enable: bool) {
        let mut sels = self.selections.lock().unwrap();
        sels.retain(|s| s.kind != kind);
        if enable {
            sels.push(SelReg { kind, atom, window });
        }
    }

    /// The window selected for RandR ScreenChangeNotify, 0 if none.
    pub(super) fn randr_window(&self) -> u32 {
        self.randr_window.load(Ordering::Relaxed)
    }

    /// The window selected for XFixes cursor changes, 0 if none.
    pub(super) fn cursor_window(&self) -> u32 {
        self.cursor_window.load(Ordering::Relaxed)
    }

    pub(super) fn wants_root_structure(&self) -> bool {
        self.root_structure.load(Ordering::Relaxed)
    }

    /// The `(window, atom)` of each XFixes registration for `kind`.
    pub(super) fn selections_for(&self, kind: Sel) -> Vec<(u32, u32)> {
        self.selections
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.kind == kind)
            .map(|r| (r.window, r.atom))
            .collect()
    }
}
