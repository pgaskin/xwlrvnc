//! Server-initiated X events (RandR/XFixes/core notifications), pushed to
//! clients from background threads — the Wayland output and clipboard thread.
//!
//! Every connection registers a [`Client`], whose socket is shared with, and
//! mutex-guarded against, the connection thread's own replies so concurrent
//! writes can't interleave.

use std::sync::{Arc, Mutex, OnceLock};
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
    clients: Mutex<Vec<Arc<Client>>>,
}

impl EventSink {
    pub fn register(&self, client: Arc<Client>) {
        self.clients.lock().unwrap().push(client);
    }

    /// Runs `f` for every client still connected, dropping the dead ones first.
    fn each_live(&self, mut f: impl FnMut(&Arc<Client>)) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| !c.is_dead());
        clients.iter().for_each(&mut f);
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

    /// XFixesSelectionNotify (owner changed) to clients watching `kind`.
    pub fn selection_changed(&self, kind: Sel, owner: u32, timestamp: u32) {
        let first_event = EXTENSIONS.lookup(b"XFIXES").map_or(0, |e| e.first_event);
        self.each_live(|c| {
            for (window, atom) in c.selections_for(kind) {
                send(
                    c,
                    &xfixes::SelectionNotifyEvent {
                        response_type: first_event, // + SELECTION_NOTIFY_EVENT (0)
                        subtype: xfixes::SelectionEvent::SET_SELECTION_OWNER,
                        sequence: c.seq(),
                        window,
                        owner,
                        selection: atom,
                        timestamp,
                        selection_timestamp: timestamp,
                    },
                );
            }
        });
    }

    /// MappingNotify(Keyboard) to every client, so they re-read the mapping via
    /// `XRefreshKeyboardMapping`. Without it a compositor layout switch leaves
    /// clients with a stale keycode->keysym table, typing the wrong characters.
    /// MappingNotify is unmaskable, hence every client rather than a selection.
    pub fn keyboard_mapping_changed(&self, first_keycode: u8, count: u8) {
        self.each_live(|c| {
            send(
                c,
                &xproto::MappingNotifyEvent {
                    response_type: xproto::MAPPING_NOTIFY_EVENT,
                    sequence: c.seq(),
                    request: xproto::Mapping::KEYBOARD,
                    first_keycode,
                    count,
                },
            );
        });
    }
}

/// A monotonic server timestamp in milliseconds, never 0 (X reserves that for
/// `CurrentTime`). One clock for every server-generated event.
pub fn server_time_ms() -> u32 {
    static START: OnceLock<Instant> = OnceLock::new();
    (START.get_or_init(Instant::now).elapsed().as_millis() as u32).max(1)
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
