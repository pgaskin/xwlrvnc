//! Builds an X keyboard mapping from the compositor's xkb keymap.
//!
//! X keycode = evdev code + 8, which is what xkb uses too. So a keysym the
//! client looks up resolves to a keycode whose `keycode - 8` is the evdev code
//! XTest injects, and the compositor — running the same keymap, which we forward
//! verbatim — turns it back into the keysym the client meant.

use xkbcommon_rs::xkb_state::{KeyDirection, StateComponent};
use xkbcommon_rs::{Context, Keymap, KeymapFormat, State};

/// X keycode range we expose (evdev 0..=247 shifted by 8).
pub const MIN_KEYCODE: u8 = 8;
pub const MAX_KEYCODE: u8 = 255;

/// Keysyms per keycode: base, shift, level3 (AltGr), shift+level3.
pub const SYMS_PER: u8 = 4;

/// The X keyboard mapping derived from one compositor keymap.
pub struct KeyTable {
    /// Keysyms for keycodes `MIN_KEYCODE..=MAX_KEYCODE`, `SYMS_PER` each.
    pub syms: Vec<u32>,
    /// The `GetModifierMapping` table: 8 modifiers (Shift, Lock, Control,
    /// Mod1..Mod5) of `modmap.len() / 8` X keycodes each, 0 padding.
    pub modmap: Vec<u8>,
}

/// The X modifiers, in `GetModifierMapping` order, by their xkb real-modifier
/// names.
const X_MODIFIERS: [&str; 8] = [
    "Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5",
];

/// Compiles the keymap text into the keysym and modifier tables.
pub fn build(text: &str) -> Option<KeyTable> {
    let ctx = Context::new(0).ok()?;
    let keymap = Keymap::new_from_string(ctx, text, KeymapFormat::TextV1, 0).ok()?;
    let mut state = State::new(keymap.clone());

    let mask = |name| keymap.mod_get_index(name).map_or(0u32, |i| 1u32 << i);
    let shift = mask("Shift");
    let level3 = mask("Mod5"); // AltGr
    let levels = [0, shift, level3, shift | level3];

    let mut syms = Vec::with_capacity((MAX_KEYCODE - MIN_KEYCODE + 1) as usize * SYMS_PER as usize);
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        for &m in &levels {
            state.update_mask(m, 0, 0, 0, 0, 0);
            let sym = state.key_get_one_sym(u32::from(kc)).map_or(0, |s| s.raw());
            syms.push(sym);
        }
    }
    state.update_mask(0, 0, 0, 0, 0, 0);

    // The modifier map comes from the keymap too, so xkb options that move
    // modifiers (swapping Caps and Ctrl, say) show up in X: press each key and
    // see which real modifiers it sets, resetting any lock or latch after.
    let x_mods = X_MODIFIERS.map(mask);
    let mut per_mod: [Vec<u8>; 8] = Default::default();
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        // xkbcommon-rs panics in update_key for a keycode with no groups
        if state.key_get_layout(u32::from(kc)).is_none() {
            continue;
        }
        state.update_key(u32::from(kc), KeyDirection::Down);
        let set = state.serialize_mods(StateComponent::MODS_EFFECTIVE);
        state.update_key(u32::from(kc), KeyDirection::Up);
        state.update_mask(0, 0, 0, 0, 0, 0);
        for (keys, &m) in per_mod.iter_mut().zip(&x_mods) {
            if m != 0 && set & m != 0 {
                keys.push(kc);
            }
        }
    }
    let per = per_mod.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let mut modmap = Vec::with_capacity(8 * per);
    for keys in &per_mod {
        modmap.extend_from_slice(keys);
        modmap.resize(modmap.len() + per - keys.len(), 0);
    }
    Some(KeyTable { syms, modmap })
}

/// Slices the table for a `GetKeyboardMapping(first_keycode, count)` request.
pub fn mapping_slice(table: &[u32], first_keycode: u8, count: u8) -> Vec<u32> {
    let per = SYMS_PER as usize;
    let start = first_keycode.saturating_sub(MIN_KEYCODE) as usize * per;
    (0..count as usize * per)
        .map(|i| table.get(start + i).copied().unwrap_or(0))
        .collect()
}
