//! Output coordinate mapping between the physical (X-tiled) screen and the
//! compositor's logical space.
//!
//! The X screen tiles each output's native-resolution capture buffer at a
//! physical position (see [`crate::bridge::x11::screen`]); the compositor's virtual
//! pointer, however, wants absolute motion in *logical* coordinates so it lands
//! correctly under (possibly mixed/fractional) output scaling. [`Layout`] pairs
//! each output's physical and logical rectangles and maps a physical point back
//! into logical space.

/// A single output's physical (X-screen) rect and logical (compositor) rect.
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

/// The per-output rects plus the logical bounding box (origin + size) of all
/// outputs.
pub struct Layout {
    outputs: Vec<OutputRect>,
    ox: i32,
    oy: i32,
    ow: i32,
    oh: i32,
}

impl Layout {
    /// Creates a layout from a list of output rects, or returns None if empty.
    pub fn new(outputs: Vec<OutputRect>) -> Option<Self> {
        if outputs.is_empty() {
            return None;
        }
        let ox = outputs.iter().map(|o| o.lx).min().unwrap_or(0);
        let oy = outputs.iter().map(|o| o.ly).min().unwrap_or(0);
        let mx = outputs.iter().map(|o| o.lx + o.lw).max().unwrap_or(0);
        let my = outputs.iter().map(|o| o.ly + o.lh).max().unwrap_or(0);
        Some(Layout {
            outputs,
            ox,
            oy,
            ow: (mx - ox).max(1),
            oh: (my - oy).max(1),
        })
    }

    /// Converts a physical X-screen point to `(value_x, value_y, extent_x,
    /// extent_y)` in the compositor's logical space, normalised to the logical
    /// bounding-box origin.
    pub fn to_logical(&self, x: i16, y: i16) -> Option<(u32, u32, u32, u32)> {
        let (px, py) = (i32::from(x), i32::from(y));
        // Prefer the output whose physical rect contains the point; otherwise
        // (a gap below a shorter output, or out of bounds) pick the nearest by
        // clamped distance so motion still resolves somewhere sensible.
        let inside = self
            .outputs
            .iter()
            .find(|o| px >= o.px && px < o.px + o.pw && py >= o.py && py < o.py + o.ph);
        let o = inside.or_else(|| {
            self.outputs.iter().min_by_key(|o| {
                let dx = px - px.clamp(o.px, o.px + o.pw - 1);
                let dy = py - py.clamp(o.py, o.py + o.ph - 1);
                dx * dx + dy * dy
            })
        })?;
        // Local physical offset -> local logical offset (lw/pw == 1/scale).
        let off = |p: i32, base: i32, plen: i32, llen: i32| -> i32 {
            let plen = plen.max(1);
            (i64::from((p - base).clamp(0, plen - 1)) * i64::from(llen) / i64::from(plen)) as i32
        };
        let gx = o.lx + off(px, o.px, o.pw, o.lw) - self.ox;
        let gy = o.ly + off(py, o.py, o.ph, o.lh) - self.oy;
        Some((
            gx.clamp(0, self.ow) as u32,
            gy.clamp(0, self.oh) as u32,
            self.ow as u32,
            self.oh as u32,
        ))
    }
}
