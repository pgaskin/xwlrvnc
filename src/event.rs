//! Delivery of server-initiated X events (RandR/XFixes notifications) to
//! clients from background threads (the Wayland output/clipboard thread).
//!
//! Each connection registers a [`Client`] whose `writer` is shared with — and
//! mutex-guarded against — the connection thread's own replies, so concurrent
//! writes to the same socket can't interleave.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use x11rb_protocol::protocol::{randr, render, xfixes, xproto};
use x11rb_protocol::x11_utils::Serialize;

use crate::clipboard::Sel;
use crate::x11::ROOT_WINDOW;
use crate::x11::ext;

/// A client's XFixes registration to be notified about a selection's owner.
struct SelReg {
    kind: Sel,
    atom: u32,
    window: u32,
}

/// The client's socket plus the last 16-bit sequence number written to it.
///
/// X clients (via libxcb) widen the 16-bit wire sequence to 64-bit and assume a
/// wrap whenever it appears to go *backwards*, bumping by 0x10000. If a
/// background-thread event ever went out with a sequence lower than a reply
/// already written, libxcb would compute a sequence beyond the last request the
/// client sent and abort with `xcb_xlib_threads_sequence_lost`. So every write
/// goes through here, which stamps a sequence clamped to never decrease.
struct SeqWriter {
    stream: UnixStream,
    last_seq: u16,
}

impl SeqWriter {
    /// Clamps `seq` so the on-wire sequence is monotonically non-decreasing: if
    /// `seq` is behind `last_seq` (within half the 16-bit range), reuse `last_seq`.
    fn monotonic(&self, seq: u16) -> u16 {
        if (seq.wrapping_sub(self.last_seq) as i16) >= 0 {
            seq
        } else {
            self.last_seq
        }
    }
}

/// Per-connection state the sink needs to push events.
pub struct Client {
    writer: Mutex<SeqWriter>,
    /// The client's most recent request sequence number (echoed in events).
    seq: AtomicU16,
    /// Window that selected RandR ScreenChange notifications (0 = none).
    randr_window: AtomicU32,
    /// XFixes selection-owner registrations.
    selections: Mutex<Vec<SelReg>>,
    dead: AtomicBool,
    /// Window that registered for XFixes cursor-change notifications (0 = none).
    cursor_window: AtomicU32,
    /// Whether the client selected StructureNotify on the root window, so it
    /// wants a root ConfigureNotify when the screen size changes (this is how
    /// vncagent detects screen resizes — it doesn't use RandR SelectInput).
    root_structure: AtomicBool,
}

impl Client {
    pub fn new(writer: UnixStream) -> Self {
        Self {
            writer: Mutex::new(SeqWriter { stream: writer, last_seq: 0 }),
            seq: AtomicU16::new(0),
            randr_window: AtomicU32::new(0),
            selections: Mutex::new(Vec::new()),
            dead: AtomicBool::new(false),
            cursor_window: AtomicU32::new(0),
            root_structure: AtomicBool::new(false),
        }
    }

    /// Writes the raw connection-setup bytes (the only write with no sequence).
    pub fn write_setup(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.lock().unwrap().stream.write_all(bytes)
    }

    /// Writes a framed reply/error whose sequence field at `[2..4]` this stamps
    /// (clamped monotonic) before sending. `seq` is the request's sequence.
    pub fn send_reply(&self, seq: u16, buf: &mut [u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        let seq = w.monotonic(seq);
        buf[2..4].copy_from_slice(&seq.to_le_bytes());
        w.last_seq = seq;
        w.stream.write_all(buf)
    }

    /// Stamps a server-initiated event with the current request sequence (clamped
    /// monotonic, under the writer lock so it can't race a concurrent reply) and
    /// writes it.
    pub fn send_event_bytes(&self, buf: &mut [u8]) {
        let mut w = self.writer.lock().unwrap();
        let seq = w.monotonic(self.seq.load(Ordering::Relaxed));
        buf[2..4].copy_from_slice(&seq.to_le_bytes());
        w.last_seq = seq;
        let _ = w.stream.write_all(buf);
    }

    pub fn set_seq(&self, seq: u16) {
        self.seq.store(seq, Ordering::Relaxed);
    }

    pub fn seq(&self) -> u16 {
        self.seq.load(Ordering::Relaxed)
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    pub fn select_randr(&self, window: u32) {
        self.randr_window.store(window, Ordering::Relaxed);
    }

    /// Records whether the client selected StructureNotify on the root window.
    pub fn select_root_structure(&self, enable: bool) {
        self.root_structure.store(enable, Ordering::Relaxed);
    }

    /// Registers (or, if `enable` is false, clears) XFixes selection-owner
    /// notifications for a selection kind.
    pub fn select_selection(&self, kind: Sel, atom: u32, window: u32, enable: bool) {
        let mut sels = self.selections.lock().unwrap();
        sels.retain(|s| s.kind != kind);
        if enable {
            sels.push(SelReg { kind, atom, window });
        }
    }

    pub fn mark_dead(&self) {
        self.dead.store(true, Ordering::Relaxed);
    }

    pub fn select_cursor(&self, window: u32) {
        self.cursor_window.store(window, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct EventSink {
    clients: Mutex<Vec<Arc<Client>>>,
}

impl EventSink {
    pub fn register(&self, client: Arc<Client>) {
        self.clients.lock().unwrap().push(client);
    }

    /// Notifies clients of a screen-size change: RRScreenChangeNotify to those
    /// that selected RandR input, and a root-window ConfigureNotify to those that
    /// selected StructureNotify on the root (vncagent uses the latter, not RandR,
    /// to detect resizes).
    pub fn screen_changed(&self, width: u16, height: u16, timestamp: u32, config_timestamp: u32) {
        let first_event = ext::lookup(b"RANDR").map_or(0, |e| e.first_event);
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| !c.dead.load(Ordering::Relaxed));
        for c in clients.iter() {
            let window = c.randr_window.load(Ordering::Relaxed);
            if window != 0 {
                let event = randr::ScreenChangeNotifyEvent {
                    response_type: first_event, // + SCREEN_CHANGE_NOTIFY_EVENT (0)
                    rotation: randr::Rotation::ROTATE0,
                    sequence: c.seq.load(Ordering::Relaxed),
                    timestamp,
                    config_timestamp,
                    root: ROOT_WINDOW,
                    request_window: window,
                    size_id: 0,
                    subpixel_order: render::SubPixel::UNKNOWN,
                    width,
                    height,
                    mwidth: (u32::from(width) * 254 / 960) as u16,
                    mheight: (u32::from(height) * 254 / 960) as u16,
                };
                send(c, &event);
            }
            if c.root_structure.load(Ordering::Relaxed) {
                // StructureNotify on root: event == window == root.
                let event = xproto::ConfigureNotifyEvent {
                    response_type: xproto::CONFIGURE_NOTIFY_EVENT,
                    sequence: c.seq.load(Ordering::Relaxed),
                    event: ROOT_WINDOW,
                    window: ROOT_WINDOW,
                    above_sibling: 0,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    border_width: 0,
                    override_redirect: false,
                };
                send(c, &event);
            }
        }
    }

    /// Sends XFixesCursorNotify to clients that called SelectCursorInput.
    pub fn cursor_changed(&self, serial: u32) {
        let first_event = ext::lookup(b"XFIXES").map_or(0, |e| e.first_event);
        let timestamp = crate::event::server_time_ms();
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| !c.dead.load(Ordering::Relaxed));
        for c in clients.iter() {
            let window = c.cursor_window.load(Ordering::Relaxed);
            if window == 0 {
                continue;
            }
            let event = xfixes::CursorNotifyEvent {
                response_type: first_event + xfixes::CURSOR_NOTIFY_EVENT,
                subtype: xfixes::CursorNotify::DISPLAY_CURSOR,
                sequence: c.seq.load(Ordering::Relaxed),
                window,
                cursor_serial: serial,
                timestamp,
                name: xproto::Atom::from(0u32),
            };
            send(c, &event);
        }
    }

    /// Sends XFixesSelectionNotify (owner changed) to clients watching `kind`.
    pub fn selection_changed(&self, kind: Sel, owner: u32, timestamp: u32) {
        let first_event = ext::lookup(b"XFIXES").map_or(0, |e| e.first_event);
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| !c.dead.load(Ordering::Relaxed));
        for c in clients.iter() {
            let regs = c.selections.lock().unwrap();
            for reg in regs.iter().filter(|r| r.kind == kind) {
                let event = xfixes::SelectionNotifyEvent {
                    response_type: first_event, // + SELECTION_NOTIFY_EVENT (0)
                    subtype: xfixes::SelectionEvent::SET_SELECTION_OWNER,
                    sequence: c.seq.load(Ordering::Relaxed),
                    window: reg.window,
                    owner,
                    selection: reg.atom,
                    timestamp,
                    selection_timestamp: timestamp,
                };
                send(c, &event);
            }
        }
    }

    /// Sends MappingNotify(Keyboard) to every client so it re-reads the keyboard
    /// mapping (via XRefreshKeyboardMapping/GetKeyboardMapping). Sent when the
    /// compositor keymap changes — otherwise clients keep a stale keycode→keysym
    /// table and type the wrong characters after a layout switch. MappingNotify
    /// is unmaskable, so it goes to all clients regardless of any selection.
    pub fn keyboard_mapping_changed(&self, first_keycode: u8, count: u8) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| !c.dead.load(Ordering::Relaxed));
        for c in clients.iter() {
            let event = xproto::MappingNotifyEvent {
                response_type: xproto::MAPPING_NOTIFY_EVENT,
                sequence: c.seq.load(Ordering::Relaxed),
                request: xproto::Mapping::KEYBOARD,
                first_keycode,
                count,
            };
            send(c, &event);
        }
    }
}

/// A monotonic server timestamp in milliseconds, never 0 (X reserves 0 for
/// `CurrentTime`). Shared by all server-generated events so their timestamps are
/// drawn from one clock.
pub fn server_time_ms() -> u32 {
    static START: OnceLock<Instant> = OnceLock::new();
    (START.get_or_init(Instant::now).elapsed().as_millis() as u32).max(1)
}

/// Serializes a 32-byte event and writes it through the client's guarded socket,
/// which stamps a monotonic sequence number (see [`SeqWriter`]).
pub fn send(client: &Client, event: &impl Serialize) {
    let mut buf = Vec::with_capacity(32);
    event.serialize_into(&mut buf);
    if buf.len() < 32 {
        buf.resize(32, 0);
    }
    client.send_event_bytes(&mut buf);
}
