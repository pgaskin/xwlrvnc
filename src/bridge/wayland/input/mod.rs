//! Virtual input: creates the per-seat virtual pointer/keyboard devices once a
//! backend and the seat are available, and forwards the compositor keymap to the
//! virtual keyboard.
//!
//! A backend is whatever bound manager global can mint a device for a seat —
//! [`PointerBackend`] for the pointer, [`KeyboardBackend`] for the keyboard. The
//! current backends are `zwlr_virtual_pointer_v1` ([`wlr`]) and
//! `zwp_virtual_keyboard_v1` ([`zwp`]); a libei backend (which would provide
//! both) can slot in as another module. The created devices are handed to
//! [`crate::bridge::input::Input`] as [`VirtualPointer`](crate::bridge::input::VirtualPointer) /
//! [`VirtualKeyboard`](crate::bridge::input::VirtualKeyboard) trait objects, so the input
//! translation layer is decoupled from the backing protocol.
//!
//! The input proxies carry no events (all `delegate_noop`), so there is no
//! per-backend `Dispatch` routing — the backends are plain factory trait objects.

use wayland_client::protocol::wl_seat;

use crate::bridge::input::{VirtualKeyboard, VirtualPointer};

use super::*;

mod wlr;
mod zwp;

/// wl_keyboard/virtual_keyboard keymap format for an xkb v1 text keymap.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// A bound manager global that can mint a virtual pointer for a seat.
pub(crate) trait PointerBackend: Send {
    fn create_pointer(
        &self,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<State>,
    ) -> Box<dyn VirtualPointer>;
}

/// A bound manager global that can mint a virtual keyboard for a seat.
pub(crate) trait KeyboardBackend: Send {
    fn create_keyboard(
        &self,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<State>,
    ) -> Box<dyn VirtualKeyboard>;
}

impl State {
    /// Creates the virtual pointer + keyboard once the seat and both backends are
    /// available, and forwards any keymap we've already seen.
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
            // Read the keymap text first so we can dedup. Forwarding a keymap to
            // our virtual keyboard makes sway re-broadcast the same keymap to our
            // passive wl_keyboard; ignoring an unchanged keymap breaks that
            // feedback loop (which otherwise spams + exhausts fds: ETOOMANYREFS).
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
            // Forward the keymap to the virtual keyboard verbatim. The X keymap
            // we advertise is built from this same keymap (X keycode = evdev+8 =
            // xkb keycode), so the keycodes the client sends resolve to the same
            // keysyms — the forward is an identity mapping, but the protocol
            // requires a keymap be set before any `key` request. A dup is kept so
            // we can re-forward if the virtual keyboard is created later.
            if let Ok(dup) = fd.try_clone() {
                state
                    .server
                    .input
                    .set_keymap(KEYMAP_FORMAT_XKB_V1, dup.as_fd(), size);
                state.vkbd_keymap = Some((KEYMAP_FORMAT_XKB_V1, dup, size));
            }
            // Track modifier state from this keymap so chords (Ctrl+C etc.)
            // produce `modifiers` updates on the virtual keyboard.
            state.server.input.set_modifier_keymap(&text);
            if let Some(table) = crate::bridge::keymap::build(&text) {
                *state.server.keymap.lock().unwrap() = Some(table);
                crate::log!("loaded compositor keymap");
                // Tell X clients the keyboard mapping changed so they re-read it
                // (vncagent caches keycode→keysym and would otherwise type stale
                // characters after a compositor layout switch).
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

/// Reads the null-terminated xkb keymap text from the compositor's fd.
fn read_keymap(fd: OwnedFd, size: u32) -> Option<String> {
    let mut buf = vec![0u8; size as usize];
    File::from(fd).read_exact_at(&mut buf, 0).ok()?;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}
