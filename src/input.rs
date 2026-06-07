//! Turns XTest `FakeInput` events into Wayland virtual-input protocol requests.
//!
//! Keyboard events become `zwp_virtual_keyboard_v1` requests and pointer events
//! become `zwlr_virtual_pointer_v1` requests. A separate proxy (the standalone
//! `wl-uinput-proxy` tool) sits between us and the compositor and implements
//! these protocols over uinput, so compositors with broken/incomplete virtual
//! input support still work.
//!
//! Keyboard: X keycodes are evdev codes + 8, so we send `keycode - 8`. The
//! compositor keymap is forwarded to the virtual keyboard verbatim (see
//! [`Input::set_keymap`]) so the keycodes resolve to the same keysyms the X
//! client intended. Pointer: absolute motion is sent against the screen
//! geometry; X scroll buttons (4-7) become wheel axis notches; everything else
//! is a button.

use std::os::fd::BorrowedFd;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Instant;

use wayland_client::Connection;
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;
use xkbcommon_rs::xkb_state::{KeyDirection, StateComponent};
use xkbcommon_rs::{Context, Keymap, KeymapFormat, State};

// XTest/core input event types (from X.h).
pub const KEY_PRESS: u8 = 2;
pub const KEY_RELEASE: u8 = 3;
pub const BUTTON_PRESS: u8 = 4;
pub const BUTTON_RELEASE: u8 = 5;
pub const MOTION_NOTIFY: u8 = 6;

// evdev button codes (linux/input-event-codes.h).
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// Wayland axis units per wheel notch (matches wlroots/libinput).
const WHEEL_NOTCH: f64 = 15.0;

pub struct Input {
    width: AtomicI32,
    height: AtomicI32,
    /// Base for the millisecond timestamps the protocols want; only ordering
    /// and granularity matter, not the epoch.
    start: Instant,
    /// Installed once the Wayland thread has created the virtual devices.
    backend: OnceLock<Backend>,
    /// xkb state tracking the modifier keys we've injected. The virtual
    /// keyboard (unlike a real one) does not derive modifier state from `key`
    /// events, so we replay each key into this state and forward the resulting
    /// modifier masks via the `modifiers` request — otherwise chords like
    /// Ctrl+C arrive as a bare `c`.
    xkb: Mutex<Option<State>>,
    /// Physical↔logical output layout, used to remap absolute pointer motion
    /// from our physical-tiled X screen into the compositor's logical space.
    /// `None` until the first output is known (then we fall back to a plain
    /// proportional mapping over the screen geometry).
    layout: Mutex<Option<Layout>>,
}

/// One output's physical (X-screen) rect paired with its logical (compositor)
/// rect, for remapping absolute pointer coordinates.
#[derive(Clone, Copy)]
pub struct OutputRect {
    pub px: i32,
    pub py: i32,
    pub pw: i32,
    pub ph: i32,
    pub lx: i32,
    pub ly: i32,
    pub lw: i32,
    pub lh: i32,
}

struct Layout {
    outputs: Vec<OutputRect>,
    /// Logical bounding box (origin + size) of all outputs.
    ox: i32,
    oy: i32,
    ow: i32,
    oh: i32,
}

struct Backend {
    conn: Connection,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    /// The virtual keyboard rejects `key` requests until a keymap is set.
    keymap_set: AtomicBool,
}

impl Input {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width: AtomicI32::new(width.into()),
            height: AtomicI32::new(height.into()),
            start: Instant::now(),
            backend: OnceLock::new(),
            xkb: Mutex::new(None),
            layout: Mutex::new(None),
        }
    }

    /// Installs the virtual-input objects. Called once by the Wayland thread
    /// when the seat and both managers are available.
    pub fn set_devices(
        &self,
        conn: Connection,
        pointer: ZwlrVirtualPointerV1,
        keyboard: ZwpVirtualKeyboardV1,
    ) {
        let _ = self.backend.set(Backend {
            conn,
            pointer,
            keyboard,
            keymap_set: AtomicBool::new(false),
        });
    }

    /// Forwards the compositor keymap to the virtual keyboard. Until this lands,
    /// key events are dropped (the protocol requires a keymap first). Safe to
    /// call before [`set_devices`](Self::set_devices) (it's a no-op then; the
    /// Wayland thread re-forwards once the devices exist).
    pub fn set_keymap(&self, format: u32, fd: BorrowedFd, size: u32) {
        if let Some(b) = self.backend.get() {
            b.keyboard.keymap(format, fd, size);
            b.keymap_set.store(true, Ordering::Release);
            let _ = b.conn.flush();
        }
    }

    /// Builds the modifier-tracking xkb state from the compositor keymap text.
    /// Called alongside [`set_keymap`](Self::set_keymap); see the `xkb` field.
    pub fn set_modifier_keymap(&self, text: &str) {
        let state = Context::new(0)
            .ok()
            .and_then(|ctx| Keymap::new_from_string(ctx, text, KeymapFormat::TextV1, 0).ok())
            .map(State::new);
        if state.is_some() {
            *self.xkb.lock().unwrap() = state;
        }
    }

    /// Replays a key into the modifier-tracking xkb state and forwards the
    /// resulting modifier masks to the virtual keyboard via `modifiers`. Sent
    /// after every key (even non-modifiers); an unchanged mask is a harmless
    /// no-op the compositor diffs away.
    fn update_modifiers(&self, b: &Backend, x_keycode: u8, press: bool) {
        let mut guard = self.xkb.lock().unwrap();
        let Some(state) = guard.as_mut() else { return };
        let dir = if press { KeyDirection::Down } else { KeyDirection::Up };
        // xkb keycode == X keycode == evdev + 8, so pass `detail` unshifted.
        let kc = u32::from(x_keycode);
        // xkbcommon-rs panics ("Key has no valid layout") inside update_key for
        // keycodes present in the keymap but with no symbol groups — which
        // RealVNC can inject. key_get_layout makes the same check it would panic
        // on, so skip such keys (they can't affect modifier state anyway).
        if state.key_get_layout(kc).is_none() {
            return;
        }
        state.update_key(kc, dir);
        let depressed = state.serialize_mods(StateComponent::MODS_DEPRESSED);
        let latched = state.serialize_mods(StateComponent::MODS_LATCHED);
        let locked = state.serialize_mods(StateComponent::MODS_LOCKED);
        let group = state.serialize_layout(StateComponent::LAYOUT_EFFECTIVE) as u32;
        b.keyboard.modifiers(depressed, latched, locked, group);
    }

    /// Updates the screen geometry used to scale absolute motion (called when
    /// RandR resizes the screen).
    pub fn set_geometry(&self, width: u16, height: u16) {
        self.width.store(width.into(), Ordering::Relaxed);
        self.height.store(height.into(), Ordering::Relaxed);
    }

    /// Installs the physical↔logical output layout used to remap absolute
    /// pointer motion. Called by the Wayland thread whenever outputs change.
    pub fn set_layout(&self, outputs: Vec<OutputRect>) {
        let layout = (!outputs.is_empty()).then(|| {
            let ox = outputs.iter().map(|o| o.lx).min().unwrap_or(0);
            let oy = outputs.iter().map(|o| o.ly).min().unwrap_or(0);
            let mx = outputs.iter().map(|o| o.lx + o.lw).max().unwrap_or(0);
            let my = outputs.iter().map(|o| o.ly + o.lh).max().unwrap_or(0);
            Layout { outputs, ox, oy, ow: (mx - ox).max(1), oh: (my - oy).max(1) }
        });
        *self.layout.lock().unwrap() = layout;
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Handles one XTest `FakeInput` event.
    pub fn fake_input(&self, type_: u8, detail: u8, x: i16, y: i16) {
        let Some(b) = self.backend.get() else { return };
        match type_ {
            KEY_PRESS | KEY_RELEASE => {
                if b.keymap_set.load(Ordering::Acquire) {
                    let press = type_ == KEY_PRESS;
                    let evdev = u32::from(detail).wrapping_sub(8);
                    b.keyboard.key(self.time(), evdev, press as u32);
                    self.update_modifiers(b, detail, press);
                }
            }
            BUTTON_PRESS | BUTTON_RELEASE => self.button(b, detail, type_ == BUTTON_PRESS),
            MOTION_NOTIFY => {
                if detail == 0 {
                    self.motion_abs(b, x, y);
                } else {
                    self.motion_rel(b, x.into(), y.into());
                }
            }
            _ => {
                crate::warning!("unknown fake input type {type_}");
                return;
            }
        }
        let _ = b.conn.flush();
    }

    fn button(&self, b: &Backend, button: u8, press: bool) {
        // X scroll buttons (4-7) are momentary; emit one wheel notch on press.
        // Values follow wl_pointer convention (down/right positive); the proxy
        // converts them to evdev wheel events.
        let scroll = match button {
            4 => Some((Axis::VerticalScroll, -1)),   // up
            5 => Some((Axis::VerticalScroll, 1)),    // down
            6 => Some((Axis::HorizontalScroll, -1)), // left
            7 => Some((Axis::HorizontalScroll, 1)),  // right
            _ => None,
        };
        if let Some((axis, discrete)) = scroll {
            if press {
                let t = self.time();
                b.pointer.axis_source(AxisSource::Wheel);
                b.pointer.axis(t, axis, f64::from(discrete) * WHEEL_NOTCH);
                b.pointer.axis_discrete(t, axis, f64::from(discrete) * WHEEL_NOTCH, discrete);
                b.pointer.frame();
            }
            return;
        }
        let code = match button {
            1 => BTN_LEFT,
            2 => BTN_MIDDLE,
            3 => BTN_RIGHT,
            8 => BTN_SIDE,
            9 => BTN_EXTRA,
            _ => {
                crate::warning!("ignoring unsupported X button {button}");
                return;
            }
        };
        let state = if press { ButtonState::Pressed } else { ButtonState::Released };
        b.pointer.button(self.time(), code, state);
        b.pointer.frame();
    }

    fn motion_abs(&self, b: &Backend, x: i16, y: i16) {
        // Map the physical X-screen point back into the compositor's logical
        // space so motion lands correctly under (possibly mixed/fractional)
        // output scaling. The virtual pointer maps the value/extent fraction
        // over the logical output layout, so feeding physical coords with a
        // physical extent would misplace the cursor on scaled outputs.
        if let Some((lx, ly, lw, lh)) = self.to_logical(x, y) {
            b.pointer.motion_absolute(self.time(), lx, ly, lw, lh);
            b.pointer.frame();
            return;
        }
        // Fallback (no layout yet): proportional mapping over the screen size.
        let w = self.width.load(Ordering::Relaxed).max(0);
        let h = self.height.load(Ordering::Relaxed).max(0);
        let cx = i32::from(x).clamp(0, w) as u32;
        let cy = i32::from(y).clamp(0, h) as u32;
        b.pointer.motion_absolute(self.time(), cx, cy, w as u32, h as u32);
        b.pointer.frame();
    }

    /// Converts a physical X-screen point to `(value_x, value_y, extent_x,
    /// extent_y)` in the compositor's logical space, normalised to the logical
    /// bounding-box origin. Returns `None` if no layout is installed.
    fn to_logical(&self, x: i16, y: i16) -> Option<(u32, u32, u32, u32)> {
        let guard = self.layout.lock().unwrap();
        let layout = guard.as_ref()?;
        let (px, py) = (i32::from(x), i32::from(y));
        // Prefer the output whose physical rect contains the point; otherwise
        // (a gap below a shorter output, or out of bounds) pick the nearest by
        // clamped distance so motion still resolves somewhere sensible.
        let inside = layout
            .outputs
            .iter()
            .find(|o| px >= o.px && px < o.px + o.pw && py >= o.py && py < o.py + o.ph);
        let o = inside.or_else(|| {
            layout.outputs.iter().min_by_key(|o| {
                let dx = px - px.clamp(o.px, o.px + o.pw - 1);
                let dy = py - py.clamp(o.py, o.py + o.ph - 1);
                dx * dx + dy * dy
            })
        })?;
        // Local physical offset → local logical offset (lw/pw == 1/scale).
        let off = |p: i32, base: i32, plen: i32, llen: i32| -> i32 {
            let plen = plen.max(1);
            (i64::from((p - base).clamp(0, plen - 1)) * i64::from(llen) / i64::from(plen)) as i32
        };
        let gx = o.lx + off(px, o.px, o.pw, o.lw) - layout.ox;
        let gy = o.ly + off(py, o.py, o.ph, o.lh) - layout.oy;
        Some((
            gx.clamp(0, layout.ow) as u32,
            gy.clamp(0, layout.oh) as u32,
            layout.ow as u32,
            layout.oh as u32,
        ))
    }

    /// Relative motion is forwarded as a relative wl_pointer motion; the proxy
    /// scales it against the extents from the last absolute motion.
    fn motion_rel(&self, b: &Backend, dx: i32, dy: i32) {
        b.pointer.motion(self.time(), f64::from(dx), f64::from(dy));
        b.pointer.frame();
    }
}
