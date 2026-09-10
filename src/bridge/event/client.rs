use std::collections::VecDeque;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::bridge::clipboard::Sel;

/// How much may be queued for one client before it is given up on. Events are
/// pushed from the Wayland thread and replies from the connection thread, and
/// neither waits on the socket: a writer thread per client drains the queue,
/// so a client that stops reading (stopped, or wedged) only ever costs memory,
/// up to this much, rather than stalling capture and the clipboard for every
/// other client. A full-screen `GetImage` reply is tens of MB on a big screen,
/// so this allows a few of those in flight; a single write is never refused
/// on its own, however large.
const MAX_QUEUED_BYTES: usize = 64 << 20;

/// How long one write may block before the client is given up on. With the
/// queue bounded above this is belt and braces, but a client that has not
/// read a byte in this long is not coming back.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The outbound queue, plus the last 16-bit sequence stamped into it.
///
/// libxcb widens the wire sequence to 64-bit and assumes a wrap whenever it
/// appears to go *backwards*, bumping by 0x10000. An event from a background
/// thread carrying a sequence lower than a reply already written would therefore
/// be widened past the last request the client sent, and libxcb aborts with
/// `xcb_xlib_threads_sequence_lost`. So every write is stamped and queued under
/// this one lock, and the writer thread sends them in that order.
struct Outbox {
    queue: VecDeque<Vec<u8>>,
    bytes: usize, // sum of the queued lengths
    last_seq: u16,
    /// No more will be queued: the connection ended, or the client was
    /// dropped. The writer thread exits once the queue is drained.
    closed: bool,
}

impl Outbox {
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

/// One connection, as seen by [`EventSink`](super::EventSink): the outbound
/// queue to its socket, plus what the client has asked to be notified about.
pub struct Client {
    outbox: Mutex<Outbox>,
    outbox_ready: Condvar,
    /// A duplicate of the socket, only ever used to shut it down when the
    /// client is dropped, so the connection thread's blocking read ends.
    socket: UnixStream,
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
    /// Takes a duplicate of the connection's socket for writing, and starts
    /// the writer thread that drains the queue into it.
    pub fn new(writer: UnixStream) -> std::io::Result<Arc<Self>> {
        // the option is on the socket, so the connection thread's reads on its
        // duplicate fd are unaffected
        let _ = writer.set_write_timeout(Some(WRITE_TIMEOUT));
        let socket = writer.try_clone()?;
        let client = Arc::new(Self {
            outbox: Mutex::new(Outbox {
                queue: VecDeque::new(),
                bytes: 0,
                last_seq: 0,
                closed: false,
            }),
            outbox_ready: Condvar::new(),
            socket,
            seq: AtomicU16::new(0),
            randr_window: AtomicU32::new(0),
            cursor_window: AtomicU32::new(0),
            selections: Mutex::new(Vec::new()),
            dead: AtomicBool::new(false),
            root_structure: AtomicBool::new(false),
        });
        let for_writer = client.clone();
        std::thread::spawn(move || for_writer.writer_loop(writer));
        Ok(client)
    }

    /// The writer thread: sends queued buffers in order until the outbox is
    /// closed and drained. A failed write (the client went away, or timed out)
    /// drops the client.
    fn writer_loop(&self, mut stream: UnixStream) {
        loop {
            let next = {
                let mut o = self.outbox.lock().unwrap();
                loop {
                    if let Some(buf) = o.queue.pop_front() {
                        o.bytes -= buf.len();
                        break Some(buf);
                    }
                    if o.closed {
                        break None;
                    }
                    o = self.outbox_ready.wait(o).unwrap();
                }
            };
            let Some(buf) = next else { return };
            if let Err(e) = stream.write_all(&buf) {
                crate::vlog!("dropping client: write failed: {e}");
                self.drop_client();
                return;
            }
        }
    }

    /// Queues `buf` for the writer thread, or drops the client if it has
    /// fallen too far behind. Call with the outbox locked.
    fn enqueue(&self, o: &mut Outbox, buf: Vec<u8>) {
        if o.closed {
            return;
        }
        // an empty queue always takes the write: the cap is about a backlog,
        // and one oversize reply (a huge GetImage) is not one
        if o.bytes > 0 && o.bytes + buf.len() > MAX_QUEUED_BYTES {
            crate::vlog!("dropping client: {} bytes queued and not read", o.bytes);
            self.dead.store(true, Ordering::Relaxed);
            o.closed = true;
            o.queue.clear();
            o.bytes = 0;
            let _ = self.socket.shutdown(Shutdown::Both);
            self.outbox_ready.notify_one();
            return;
        }
        o.bytes += buf.len();
        o.queue.push_back(buf);
        self.outbox_ready.notify_one();
    }

    /// Marks the client dead and shuts its socket down, so the connection
    /// thread's next read ends and it cleans up, and discards anything still
    /// queued.
    fn drop_client(&self) {
        self.mark_dead();
        let mut o = self.outbox.lock().unwrap();
        o.closed = true;
        o.queue.clear();
        o.bytes = 0;
        let _ = self.socket.shutdown(Shutdown::Both);
        self.outbox_ready.notify_one();
    }

    /// Ends the queue once the connection is done: whatever is queued still
    /// goes out, then the writer thread exits. Called from the connection
    /// thread when it finishes.
    pub fn close(&self) {
        self.mark_dead();
        let mut o = self.outbox.lock().unwrap();
        o.closed = true;
        self.outbox_ready.notify_one();
    }

    /// Queues the connection-setup bytes, the only write with no sequence.
    pub fn write_setup(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut o = self.outbox.lock().unwrap();
        self.enqueue(&mut o, bytes.to_vec());
        self.check_open(&o)
    }

    /// Queues a framed reply or error, stamping `seq` (clamped monotonic) into
    /// its sequence field at `[2..4]`. Fails once the client is gone, which is
    /// the connection thread's cue to stop.
    pub fn send_reply(&self, seq: u16, buf: &mut [u8]) -> std::io::Result<()> {
        let mut o = self.outbox.lock().unwrap();
        let seq = o.monotonic(seq);
        buf[2..4].copy_from_slice(&seq.to_le_bytes());
        o.last_seq = seq;
        self.enqueue(&mut o, buf.to_vec());
        self.check_open(&o)
    }

    /// Queues a server-initiated event, stamping the client's current request
    /// sequence under the outbox lock so it can't race a concurrent reply.
    pub fn send_event_bytes(&self, buf: &mut [u8]) {
        let mut o = self.outbox.lock().unwrap();
        let seq = o.monotonic(self.seq.load(Ordering::Relaxed));
        buf[2..4].copy_from_slice(&seq.to_le_bytes());
        o.last_seq = seq;
        self.enqueue(&mut o, buf.to_vec());
    }

    fn check_open(&self, o: &Outbox) -> std::io::Result<()> {
        if o.closed {
            Err(std::io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
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

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::time::Duration;

    use super::*;

    /// A 32-byte frame with a marker byte, to tell frames apart on the wire.
    fn frame(marker: u8) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[0] = marker;
        b
    }

    fn seq_of(frame: &[u8]) -> u16 {
        u16::from_le_bytes([frame[2], frame[3]])
    }

    #[test]
    fn writes_arrive_in_order_with_a_monotonic_sequence() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let client = Client::new(a).unwrap();
        client.write_setup(&[0xaa; 8]).unwrap();
        // a reply at sequence 5, then an event stamped from a stale request
        // sequence of 3, which must not go backwards on the wire
        let mut reply = frame(1);
        client.send_reply(5, &mut reply).unwrap();
        client.set_seq(3);
        let mut event = frame(2);
        client.send_event_bytes(&mut event);
        // and a reply that has moved on
        let mut reply2 = frame(3);
        client.send_reply(6, &mut reply2).unwrap();

        let mut got = vec![0u8; 8 + 32 * 3];
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        b.read_exact(&mut got).unwrap();
        assert_eq!(&got[..8], &[0xaa; 8]);
        let frames: Vec<&[u8]> = got[8..].chunks(32).collect();
        assert_eq!(frames.iter().map(|f| f[0]).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(
            frames.iter().map(|f| seq_of(f)).collect::<Vec<_>>(),
            [5, 5, 6]
        );
        assert!(!client.is_dead());
    }

    #[test]
    fn close_sends_what_is_queued_then_ends_the_stream() {
        let (a, mut b) = UnixStream::pair().unwrap();
        let client = Client::new(a).unwrap();
        let mut f = frame(7);
        client.send_reply(1, &mut f).unwrap();
        client.close();
        assert!(client.is_dead());
        assert!(client.send_reply(2, &mut frame(8)).is_err());
        // once every holder of the socket is gone, the reader sees EOF after
        // the queued frame
        drop(client);
        let mut got = Vec::new();
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        b.read_to_end(&mut got).unwrap();
        assert_eq!(got.len(), 32);
        assert_eq!(got[0], 7);
    }

    #[test]
    fn a_client_that_stops_reading_is_dropped_at_the_cap() {
        let (a, b) = UnixStream::pair().unwrap();
        let client = Client::new(a).unwrap();
        // nobody reads `b`, so the socket buffer fills and the queue grows
        let chunk = vec![0u8; 1 << 20];
        let mut sent = 0usize;
        let mut dropped = false;
        while sent <= MAX_QUEUED_BYTES + (1 << 20) {
            if client.send_reply(1, &mut chunk.clone()).is_err() {
                dropped = true;
                break;
            }
            sent += chunk.len();
        }
        assert!(dropped, "sent {sent} bytes without being dropped");
        assert!(client.is_dead());
        assert!(client.send_reply(1, &mut frame(1)).is_err());
        // the queue was discarded, not left holding the memory
        assert_eq!(client.outbox.lock().unwrap().bytes, 0);
        // the socket was shut down, so the connection thread's read would end
        let mut peek = [0u8; 1];
        let mut b = b;
        b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // drain whatever the writer managed to push before the shutdown
        let mut sink = vec![0u8; 1 << 16];
        loop {
            match b.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) => panic!("read after shutdown failed: {e}"),
            }
        }
        assert_eq!(b.read(&mut peek).unwrap(), 0);
    }
}
