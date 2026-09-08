//! Virtual input: mints the per-seat pointer and keyboard once a backend and the
//! seat exist, and forwards the compositor keymap to the keyboard.
//!
//! A backend is any bound manager global that can mint a device for a seat —
//! [`PointerBackend`] and [`KeyboardBackend`], currently
//! `zwlr_virtual_pointer_v1` ([`wlr`]) and `zwp_virtual_keyboard_v1` ([`zwp`]).
//! A libei backend, which would provide both, can slot in as another module. The
//! devices reach [`crate::bridge::input::Input`] as trait objects, so the
//! translation layer never sees which protocol won.
//!
//! The input proxies carry no events, all `delegate_noop`, so the backends are
//! plain factories with no `Dispatch` routing of their own.

use wayland_client::protocol::wl_seat;

use crate::bridge::input::{VirtualKeyboard, VirtualPointer};

use super::*;

mod wlr;
mod zwp;

/// The wl_keyboard/virtual_keyboard format value for an xkb v1 text keymap.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// A manager global that can mint a virtual pointer for a seat.
pub(crate) trait PointerBackend: Send {
    fn create_pointer(
        &self,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<State>,
    ) -> Box<dyn VirtualPointer>;
}

/// A manager global that can mint a virtual keyboard for a seat.
pub(crate) trait KeyboardBackend: Send {
    fn create_keyboard(
        &self,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<State>,
    ) -> Box<dyn VirtualKeyboard>;
}

impl State {
    /// Creates both devices once the seat and both backends exist, then forwards
    /// any keymap already seen.
    pub(super) fn try_init_virtual_input(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        if self.virtual_input_ready {
            return;
        }
        let (Some(seat), Some(pb), Some(kb)) =
            (&self.seat, &self.pointer_backend, &self.keyboard_backend)
        else {
            return;
        };
        let pointer = pb.create_pointer(seat, qh);
        let keyboard = kb.create_keyboard(seat, qh);
        self.server
            .input
            .set_devices(conn.clone(), pointer, keyboard);
        if let Some((format, fd, size)) = &self.vkbd_keymap {
            self.server.input.set_keymap(*format, fd.as_fd(), *size);
        }
        self.virtual_input_ready = true;
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _kbd: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event
            && format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
        {
            // Read the text first, to dedup. Forwarding a keymap to our virtual
            // keyboard makes sway re-broadcast it to our passive wl_keyboard, and
            // ignoring an unchanged keymap is what breaks that loop before it
            // exhausts in-flight fds (ETOOMANYREFS).
            let Some(text) = (match fd.try_clone() {
                Ok(dup) => read_keymap(dup, size),
                Err(_) => None,
            }) else {
                return;
            };
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&text, &mut hasher);
            let hash = std::hash::Hasher::finish(&hasher);
            if state.keymap_hash == Some(hash) {
                return;
            }
            state.keymap_hash = Some(hash);
            // Forward it verbatim. The X keymap we advertise is built from this
            // same one (X keycode = evdev+8 = xkb keycode), so this is an identity
            // mapping — but the protocol demands a keymap before any `key`. Keep a
            // dup in case the virtual keyboard shows up later.
            if let Ok(dup) = fd.try_clone() {
                state
                    .server
                    .input
                    .set_keymap(KEYMAP_FORMAT_XKB_V1, dup.as_fd(), size);
                state.vkbd_keymap = Some((KEYMAP_FORMAT_XKB_V1, dup, size));
            }
            // track modifier state from it, so chords like Ctrl+C produce
            // `modifiers` updates on the virtual keyboard
            state.server.input.set_modifier_keymap(&text);
            if let Some(table) = crate::bridge::keymap::build(&text) {
                *state.server.keymap.lock().unwrap() = Some(table);
                crate::log!("loaded compositor keymap");
                // tell X clients to re-read the mapping; vncagent caches
                // keycode->keysym and would otherwise type stale characters after
                // a compositor layout switch
                let count =
                    crate::bridge::keymap::MAX_KEYCODE - crate::bridge::keymap::MIN_KEYCODE + 1;
                state
                    .server
                    .events
                    .keyboard_mapping_changed(crate::bridge::keymap::MIN_KEYCODE, count);
            }
        }
    }
}

/// Reads the NUL-terminated xkb keymap text out of the compositor's fd.
fn read_keymap(fd: OwnedFd, size: u32) -> Option<String> {
    let mut buf = vec![0u8; size as usize];
    File::from(fd).read_exact_at(&mut buf, 0).ok()?;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}
