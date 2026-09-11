//! Server-initiated X events (RandR/XFixes/core notifications), pushed to
//! clients from background threads — the Wayland output and clipboard thread.
//!
//! Every connection registers a [`Client`], which queues everything written
//! to it (the connection thread's replies and these events alike) for its own
//! writer thread, so the sequence order is kept and nothing here waits on a
//! client's socket.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

use x11rb_protocol::protocol::{randr, render, xfixes, xproto};
use x11rb_protocol::x11_utils::Serialize;

use crate::bridge::clipboard::Sel;
use crate::bridge::x11::ROOT_WINDOW;
use crate::bridge::x11::ext::EXTENSIONS;
use crate::util::mm;

mod client;
pub use client::Client;

#[derive(Default)]
pub struct EventSink {
    /// Weak, deliberately: the connection owns its [`Client`], and that owns a
    /// duplicate of the socket. A strong reference here would keep that
    /// duplicate open for every connection that has already gone away, until
    /// something happened to send an event and sweep the list — so a display
    /// with little event traffic and many short-lived clients runs out of file
    /// descriptors and stops accepting connections entirely.
    clients: Mutex<Vec<Weak<Client>>>,
}

impl EventSink {
    pub fn register(&self, client: &Arc<Client>) {
        let mut clients = self.clients.lock().unwrap();
        // the entries themselves are tiny, but sweeping here as well as on
        // fanout bounds the list by the connections that overlap in time rather
        // than by every connection the display has ever had
        clients.retain(Self::live);
        clients.push(Arc::downgrade(client));
    }

    /// Whether a registration still refers to a connected client.
    fn live(client: &Weak<Client>) -> bool {
        client.upgrade().is_some_and(|c| !c.is_dead())
    }

    /// Runs `f` for every client still connected, dropping the dead ones as it
    /// goes.
    fn each_live(&self, mut f: impl FnMut(&Arc<Client>)) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|client| {
            let Some(client) = client.upgrade().filter(|c| !c.is_dead()) else {
                return false;
            };
            f(&client);
            true
        });
    }

    /// RRScreenChangeNotify to clients that selected RandR input, and a root
    /// ConfigureNotify to those that selected StructureNotify on the root.
    pub fn screen_changed(&self, width: u16, height: u16, timestamp: u32, config_timestamp: u32) {
        let first_event = EXTENSIONS.lookup(b"RANDR").map_or(0, |e| e.first_event);
        self.each_live(|c| {
            let window = c.randr_window();
            if window != 0 {
                send(
                    c,
                    &randr::ScreenChangeNotifyEvent {
                        response_type: first_event, // + SCREEN_CHANGE_NOTIFY_EVENT (0)
                        rotation: randr::Rotation::ROTATE0,
                        sequence: c.seq(),
                        timestamp,
                        config_timestamp,
                        root: ROOT_WINDOW,
                        request_window: window,
                        size_id: 0,
                        subpixel_order: render::SubPixel::UNKNOWN,
                        width,
                        height,
                        mwidth: mm(width) as u16,
                        mheight: mm(height) as u16,
                    },
                );
            }
            if c.wants_root_structure() {
                // StructureNotify on root, so event == window == root
                send(
                    c,
                    &xproto::ConfigureNotifyEvent {
                        response_type: xproto::CONFIGURE_NOTIFY_EVENT,
                        sequence: c.seq(),
                        event: ROOT_WINDOW,
                        window: ROOT_WINDOW,
                        above_sibling: 0,
                        x: 0,
                        y: 0,
                        width,
                        height,
                        border_width: 0,
                        override_redirect: false,
                    },
                );
            }
        });
    }

    /// XFixesCursorNotify to clients that called SelectCursorInput.
    pub fn cursor_changed(&self, serial: u32) {
        let first_event = EXTENSIONS.lookup(b"XFIXES").map_or(0, |e| e.first_event);
        let timestamp = server_time_ms();
        self.each_live(|c| {
            let window = c.cursor_window();
            if window == 0 {
                return;
            }
            send(
                c,
                &xfixes::CursorNotifyEvent {
                    response_type: first_event + xfixes::CURSOR_NOTIFY_EVENT,
                    subtype: xfixes::CursorNotify::DISPLAY_CURSOR,
                    sequence: c.seq(),
                    window,
                    cursor_serial: serial,
                    timestamp,
                    name: xproto::Atom::from(0u32),
                },
            );
        });
    }

    /// XFixesSelectionNotify of `subtype` to every client watching the bridged
    /// selection `kind` (interned as `atom`) for it. `selection_timestamp` is
    /// the selection's last change (dix's `lastTimeChanged`), which is
    /// `timestamp` itself for a change of owner but stays put when the owner
    /// merely went away.
    pub fn selection_changed(
        &self,
        kind: Sel,
        atom: u32,
        subtype: xfixes::SelectionEvent,
        owner: u32,
        timestamp: u32,
        selection_timestamp: u32,
    ) {
        let mut sent = 0usize;
        self.each_live(|c| {
            let regs = c.selections_for(atom, subtype);
            sent += regs.len();
            selection_notify(c, &regs, subtype, owner, timestamp, selection_timestamp);
        });
        crate::cliplog!(
            "{kind:?} {subtype:?} owner {owner:#x} at {timestamp}: notified {sent} watcher(s)"
        );
    }

    /// MappingNotify(Keyboard) and MappingNotify(Modifier) to every client, so
    /// they re-read both tables via `XRefreshKeyboardMapping`. Without it a
    /// compositor layout switch leaves clients with a stale keycode->keysym
    /// table, typing the wrong characters. MappingNotify is unmaskable, hence
    /// every client rather than a selection.
    pub fn keyboard_mapping_changed(&self, first_keycode: u8, count: u8) {
        self.each_live(|c| {
            for request in [xproto::Mapping::KEYBOARD, xproto::Mapping::MODIFIER] {
                send(
                    c,
                    &xproto::MappingNotifyEvent {
                        response_type: xproto::MAPPING_NOTIFY_EVENT,
                        sequence: c.seq(),
                        request,
                        first_keycode,
                        count,
                    },
                );
            }
        });
    }
}

/// XFixesSelectionNotify to one client, for each `(window, atom)` it registered.
pub fn selection_notify(
    client: &Client,
    regs: &[(u32, u32)],
    subtype: xfixes::SelectionEvent,
    owner: u32,
    timestamp: u32,
    selection_timestamp: u32,
) {
    let first_event = EXTENSIONS.lookup(b"XFIXES").map_or(0, |e| e.first_event);
    for &(window, atom) in regs {
        send(
            client,
            &xfixes::SelectionNotifyEvent {
                response_type: first_event, // + SELECTION_NOTIFY_EVENT (0)
                subtype,
                sequence: client.seq(),
                window,
                owner,
                selection: atom,
                timestamp,
                selection_timestamp,
            },
        );
    }
}

/// The two-sided comparison X uses for timestamps, so that a wrap of the
/// millisecond clock does not make the newest time look like the oldest.
pub fn time_cmp(a: u32, b: u32) -> std::cmp::Ordering {
    (a.wrapping_sub(b) as i32).cmp(&0)
}

/// The latest timestamp handed out for a selection change, which
/// [`server_time_ms`] never drops below. Two changes inside one millisecond
/// are pushed apart, so this can run a step ahead of the clock; without the
/// floor a `PropertyNotify` in that step would carry a time *earlier* than the
/// last change, and the `SetSelectionOwner` a client builds from it would be
/// stale on arrival.
static TIME_FLOOR: AtomicU32 = AtomicU32::new(0);

/// A monotonic server timestamp in milliseconds, never 0 (X reserves that for
/// `CurrentTime`). One clock for every server-generated event.
pub fn server_time_ms() -> u32 {
    static START: OnceLock<Instant> = OnceLock::new();
    let now = (START.get_or_init(Instant::now).elapsed().as_millis() as u32).max(1);
    let floor = TIME_FLOOR.load(Ordering::Relaxed);
    if time_cmp(now, floor).is_lt() {
        floor
    } else {
        now
    }
}

/// Records `ts` as handed out for a selection change; see [`TIME_FLOOR`].
pub fn note_selection_time(ts: u32) {
    let _ = TIME_FLOOR.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |floor| {
        time_cmp(ts, floor).is_gt().then_some(ts)
    });
}

/// Serializes a 32-byte event and writes it through the client's guarded
/// socket, which stamps the sequence number.
pub fn send(client: &Client, event: &impl Serialize) {
    let mut buf = Vec::with_capacity(32);
    event.serialize_into(&mut buf);
    if buf.len() < 32 {
        buf.resize(32, 0);
    }
    client.send_event_bytes(&mut buf);
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use super::*;

    /// A closed connection must not keep its socket open. The sink used to hold
    /// an `Arc`, so the duplicate `Client` makes of the socket stayed open until
    /// some later event happened to sweep the list; a display with little event
    /// traffic and many short-lived clients then ran out of file descriptors and
    /// stopped accepting connections.
    #[test]
    fn a_gone_client_does_not_keep_its_socket_open() {
        let sink = EventSink::default();
        let (mut peer, ours) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let client = Client::new(ours).unwrap();
        sink.register(&client);

        // what `Drop for Connection` does
        client.close();
        drop(client);

        // EOF only once every copy of the socket is closed, which needs the
        // writer thread to have finished and the sink to be holding nothing
        let mut buf = [0u8; 1];
        assert_eq!(
            peer.read(&mut buf).unwrap(),
            0,
            "the socket is still open somewhere"
        );
    }

    #[test]
    fn the_registration_list_does_not_grow_without_bound() {
        let sink = EventSink::default();
        for _ in 0..50 {
            let (_peer, ours) = UnixStream::pair().unwrap();
            let client = Client::new(ours).unwrap();
            sink.register(&client);
            client.close();
        }
        // the last one may still be settling; everything before it is gone
        assert!(
            sink.clients.lock().unwrap().len() <= 2,
            "dead registrations piled up"
        );
    }

    #[test]
    fn the_clock_never_drops_below_a_selection_time() {
        // the floor is shared with every other test in the process, so only
        // ever push it forward, and compare rather than expect equality
        let ahead = server_time_ms().wrapping_add(1000);
        note_selection_time(ahead);
        assert!(time_cmp(server_time_ms(), ahead).is_ge());
        // an older one does not pull it back
        note_selection_time(ahead.wrapping_sub(500));
        assert!(time_cmp(server_time_ms(), ahead).is_ge());
    }

    #[test]
    fn timestamps_compare_across_the_wrap() {
        assert!(time_cmp(1, u32::MAX).is_gt());
        assert!(time_cmp(u32::MAX, 1).is_lt());
        assert!(time_cmp(7, 7).is_eq());
    }
}
