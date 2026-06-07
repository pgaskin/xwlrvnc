//! Builds an X keyboard mapping from the compositor's xkb keymap.
//!
//! We report X keycode = evdev code + 8 (which is also what xkb uses), so a key
//! the VNC agent looks up by keysym maps to a keycode whose `keycode - 8` is the
//! evdev code our XTest path injects via the virtual keyboard — and the
//! compositor, using the same keymap (which we forward verbatim to the virtual
//! keyboard), turns that evdev code back into the expected keysym.

use xkbcommon_rs::{Context, Keymap, KeymapFormat, State};

/// X keycode range we expose (evdev 0..=247 shifted by 8).
pub const MIN_KEYCODE: u8 = 8;
pub const MAX_KEYCODE: u8 = 255;
/// Keysyms reported per keycode: base, shift, level3 (AltGr), shift+level3.
pub const SYMS_PER: u8 = 4;

/// Compiles the keymap text and returns the keysym table for keycodes
/// `MIN_KEYCODE..=MAX_KEYCODE`, `SYMS_PER` entries each.
pub fn build(text: &str) -> Option<Vec<u32>> {
    let ctx = Context::new(0).ok()?;
    let keymap = Keymap::new_from_string(ctx, text, KeymapFormat::TextV1, 0).ok()?;
    let mut state = State::new(keymap.clone());

    let mask = |name| keymap.mod_get_index(name).map_or(0u32, |i| 1u32 << i);
    let shift = mask("Shift");
    let level3 = mask("Mod5"); // AltGr
    let levels = [0, shift, level3, shift | level3];

    let mut syms =
        Vec::with_capacity((MAX_KEYCODE - MIN_KEYCODE + 1) as usize * SYMS_PER as usize);
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        for &m in &levels {
            state.update_mask(m, 0, 0, 0, 0, 0);
            let sym = state.key_get_one_sym(u32::from(kc)).map_or(0, |s| s.raw());
            syms.push(sym);
        }
    }
    state.update_mask(0, 0, 0, 0, 0, 0);
    Some(syms)
}

/// Slices the table for a `GetKeyboardMapping(first_keycode, count)` request.
pub fn mapping_slice(table: &[u32], first_keycode: u8, count: u8) -> Vec<u32> {
    let per = SYMS_PER as usize;
    let start = first_keycode.saturating_sub(MIN_KEYCODE) as usize * per;
    (0..count as usize * per)
        .map(|i| table.get(start + i).copied().unwrap_or(0))
        .collect()
}
