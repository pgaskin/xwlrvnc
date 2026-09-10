//! The shared virtual framebuffer. Captured outputs are composited in at their
//! layout positions, and X `GetImage`/MIT-SHM reads pull rectangles back out.
//!
//! Pixels are 32-bit little-endian XRGB (byte order B, G, R, x), which is both
//! wl_shm `Xrgb8888`/`Argb8888` and our X TrueColor visual (depth 24, bpp 32,
//! R=0xff0000 G=0xff00 B=0xff, LSBFirst), so the common case needs no
//! conversion.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use x11rb_protocol::protocol::xproto::Rectangle;

use crate::util::Transform;

/// Blue, green and red source byte offsets within a 4-byte little-endian pixel,
/// enough to convert any byte-ordered 8888 wl_shm format into our framebuffer.
pub type ChannelMap = [usize; 3];

/// Identity map for `Xrgb8888`/`Argb8888` (memory order B,G,R,x), the fast path.
pub const XRGB: ChannelMap = [0, 1, 2];

#[derive(Default)]
pub struct Framebuffer {
    inner: Mutex<Fb>,
    last_read_ms: AtomicU64, // `now_ms()` of the last read, 0 if never read
}

#[derive(Default)]
struct Fb {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

/// Milliseconds since the first call: a cheap monotonic clock for the read gate.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

impl Framebuffer {
    /// Whether a client has read pixels within the last `ms` milliseconds.
    pub fn read_within(&self, ms: u64) -> bool {
        let last = self.last_read_ms.load(Ordering::Relaxed);
        last != 0 && now_ms().saturating_sub(last) < ms
    }

    /// When the last read happened, for pacing no-damage capture to the rate the
    /// client actually polls. 0 if never read.
    pub fn last_read_ms(&self) -> u64 {
        self.last_read_ms.load(Ordering::Relaxed)
    }

    /// Resizes the virtual screen, clearing it if the size changed.
    pub fn ensure(&self, width: u32, height: u32) {
        let mut fb = self.inner.lock().unwrap();
        if fb.width != width || fb.height != height {
            fb.width = width;
            fb.height = height;
            fb.data = vec![0; width as usize * height as usize * 4];
        }
    }

    /// Composites a captured output into the framebuffer at `(ox, oy)`, where
    /// `src` is `src_height` rows of `src_stride` bytes in `chan` order ([`XRGB`]
    /// takes the fast memcpy path, anything else is shuffled per pixel), and
    /// `transform` is what the compositor applies to that buffer to put it on
    /// screen (so a rotated output's buffer lands rotated, in the transformed
    /// rectangle). Returns `(changed_bbox, lock_wait_ns, work_ns)`.
    ///
    /// With `compute_damage`, pixels are compared and only changed ones copied,
    /// and the bbox of the change comes back in root coords. Without it the
    /// whole overlap is copied blind and the bbox is `None` — for clients polling
    /// `GetImage`/`ShmGetImage` with no DAMAGE object, the compare reads both
    /// buffers in full every frame to produce a bbox nobody reads.
    ///
    /// **Why plain `copy` and not `copy_with_damage`:** niri delivers
    /// `copy_with_damage` frames on its own damage-gated repaint schedule, with
    /// latency spiking to hundreds of ms on a mostly-still screen (which reads as
    /// stutter) and cursor-plane movement missed entirely. Plain `copy` answers
    /// immediately and steadily.
    #[allow(clippy::too_many_arguments)]
    pub fn blit_diff(
        &self,
        ox: i32,
        oy: i32,
        src_width: u32,
        src_height: u32,
        src_stride: u32,
        src: &[u8],
        y_invert: bool,
        transform: Transform,
        compute_damage: bool,
        chan: ChannelMap,
    ) -> (Option<Rectangle>, u64, u64) {
        let t0 = Instant::now();
        let mut fb = self.inner.lock().unwrap();
        let wait_ns = t0.elapsed().as_nanos() as u64;
        let t1 = Instant::now();
        let bbox = if transform == Transform::Normal {
            blit_rows(
                &mut fb,
                ox,
                oy,
                src_width,
                src_height,
                src_stride,
                src,
                y_invert,
                compute_damage,
                chan,
            )
        } else {
            blit_transformed(
                &mut fb,
                ox,
                oy,
                src_width,
                src_height,
                src_stride,
                src,
                y_invert,
                transform,
                compute_damage,
                chan,
            )
        };
        (bbox, wait_ns, t1.elapsed().as_nanos() as u64)
    }

    /// [`read_rect_into`](Self::read_rect_into) with a freshly allocated buffer,
    /// for the non-shm `GetImage` path whose reply owns the returned `Vec`.
    pub fn read_rect(&self, x: i32, y: i32, width: u32, height: u32) -> Vec<u8> {
        let row_bytes = width as usize * 4;
        let mut out = vec![0u8; row_bytes * height as usize];
        self.read_rect_into(x, y, width, height, &mut out);
        out
    }

    /// Reads a rectangle into `dst` as `height` rows of `width*4` bytes — exactly
    /// the ZPixmap depth-24/bpp-32 layout for our visual. Every byte of `dst` is
    /// written: one memcpy per row for the in-bounds overlap, zeroes for any
    /// margin outside it.
    ///
    /// The shm path points `dst` straight at the client's shared segment, so a
    /// full-screen read (what vncagent does ~20/sec) is one memcpy per row out of
    /// the framebuffer and nothing else.
    pub fn read_rect_into(&self, x: i32, y: i32, width: u32, height: u32, dst: &mut [u8]) {
        self.last_read_ms.store(now_ms(), Ordering::Relaxed);
        let row_bytes = width as usize * 4;
        // A zero-width rect has no rows at all. X answers such a request with an
        // ordinary reply carrying an empty image rather than an error (checked
        // against Xorg), and the row count below would divide by zero.
        if row_bytes == 0 {
            dst.fill(0);
            return;
        }
        let t0 = Instant::now();
        let fb = self.inner.lock().unwrap();
        let wait_ns = t0.elapsed().as_nanos() as u64;
        let t1 = Instant::now();
        let stride = fb.width as usize * 4;
        // horizontal overlap of the requested rect with the framebuffer
        let sx0 = x.max(0);
        let sx1 = (x + width as i32).min(fb.width as i32);
        let (copy_n, dst_col) = if sx1 > sx0 {
            ((sx1 - sx0) as usize * 4, (sx0 - x) as usize * 4)
        } else {
            (0, 0)
        };
        // only whole rows that fit in `dst` are written, so a short `dst` (a
        // smaller shm segment than requested) just truncates
        let rows = (dst.len() / row_bytes).min(height as usize);
        for row in 0..rows {
            let drow = &mut dst[row * row_bytes..row * row_bytes + row_bytes];
            let sy = y + row as i32;
            if copy_n == 0 || sy < 0 || sy >= fb.height as i32 {
                drow.fill(0);
                continue;
            }
            let s = sy as usize * stride + sx0 as usize * 4;
            if s + copy_n <= fb.data.len() {
                drow[..dst_col].fill(0);
                drow[dst_col..dst_col + copy_n].copy_from_slice(&fb.data[s..s + copy_n]);
                drow[dst_col + copy_n..].fill(0);
            } else {
                drow.fill(0);
            }
        }
        let bytes = rows * row_bytes;
        drop(fb);
        crate::bridge::profile::read(bytes, wait_ns, t1.elapsed().as_nanos() as u64);
    }
}

/// The untransformed blit: rows map to rows, so each is one compare and one
/// memcpy (or a per-pixel shuffle for a foreign byte order).
#[allow(clippy::too_many_arguments)]
fn blit_rows(
    fb: &mut Fb,
    ox: i32,
    oy: i32,
    src_width: u32,
    src_height: u32,
    src_stride: u32,
    src: &[u8],
    y_invert: bool,
    compute_damage: bool,
    chan: ChannelMap,
) -> Option<Rectangle> {
    let (fw, fh) = (fb.width as i32, fb.height as i32);
    let dst_stride = fb.width as usize * 4;
    // horizontal overlap of the source with the framebuffer
    let dx0 = ox.max(0);
    let dx1 = (ox + src_width as i32).min(fw);
    if dx1 <= dx0 {
        return None;
    }
    let n = (dx1 - dx0) as usize * 4;
    let src_col = (dx0 - ox) as usize * 4;
    // y-range of changed rows; x is always the full overlap, and over-reporting
    // it is harmless since vncagent re-reads the whole screen on any damage
    let (mut min_y, mut max_y) = (i32::MAX, i32::MIN);
    for row in 0..src_height as i32 {
        let dy = oy + row;
        if dy < 0 || dy >= fh {
            continue;
        }
        let srow = if y_invert {
            src_height as i32 - 1 - row
        } else {
            row
        };
        let s = srow as usize * src_stride as usize + src_col;
        let d = dy as usize * dst_stride + dx0 as usize * 4;
        if s + n > src.len() || d + n > fb.data.len() {
            continue;
        }
        if chan == XRGB {
            // native order, so skip the expensive compare when nobody wants
            // damage, otherwise compare the whole row and copy if changed
            if compute_damage && fb.data[d..d + n] == src[s..s + n] {
                continue;
            }
            fb.data[d..d + n].copy_from_slice(&src[s..s + n]);
        } else {
            // foreign byte order, so shuffle each pixel into XRGB, tracking
            // per-row change so damage reporting still works
            let [bi, gi, ri] = chan;
            let mut changed = false;
            let mut p = 0;
            while p < n {
                let (si, di) = (s + p, d + p);
                let (b, g, r) = (src[si + bi], src[si + gi], src[si + ri]);
                if !compute_damage
                    || fb.data[di] != b
                    || fb.data[di + 1] != g
                    || fb.data[di + 2] != r
                {
                    fb.data[di] = b;
                    fb.data[di + 1] = g;
                    fb.data[di + 2] = r;
                    fb.data[di + 3] = 0xff;
                    changed = true;
                }
                p += 4;
            }
            if !changed {
                continue;
            }
        }
        min_y = min_y.min(dy);
        max_y = max_y.max(dy);
    }
    (compute_damage && max_y >= min_y).then(|| Rectangle {
        x: dx0 as i16,
        y: min_y as i16,
        width: (dx1 - dx0) as u16,
        height: (max_y - min_y + 1) as u16,
    })
}

/// The rotated/flipped blit: every source pixel goes through `transform` to
/// its own place in the framebuffer. A source row still maps to a straight line
/// (a row or a column of the destination), so the transform is evaluated once
/// per row and walked by a fixed step. Rotated outputs are uncommon and the
/// per-pixel cost is a few ns, so this is not vectorised like the row path.
#[allow(clippy::too_many_arguments)]
fn blit_transformed(
    fb: &mut Fb,
    ox: i32,
    oy: i32,
    src_width: u32,
    src_height: u32,
    src_stride: u32,
    src: &[u8],
    y_invert: bool,
    transform: Transform,
    compute_damage: bool,
    chan: ChannelMap,
) -> Option<Rectangle> {
    let (fw, fh) = (fb.width as i32, fb.height as i32);
    let dst_stride = fb.width as usize * 4;
    let (sw, sh) = (src_width as i32, src_height as i32);
    if sw <= 0 || sh <= 0 {
        return None;
    }
    let [bi, gi, ri] = chan;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for by in 0..sh {
        let srow = if y_invert { sh - 1 - by } else { by };
        let s0 = srow as usize * src_stride as usize;
        if s0 + sw as usize * 4 > src.len() {
            continue;
        }
        let (x0, y0) = transform.point(0, by, sw, sh);
        let (x1, y1) = transform.point(1, by, sw, sh);
        let (step_x, step_y) = (x1 - x0, y1 - y0);
        for bx in 0..sw {
            let dx = ox + x0 + bx * step_x;
            let dy = oy + y0 + bx * step_y;
            if dx < 0 || dx >= fw || dy < 0 || dy >= fh {
                continue;
            }
            let si = s0 + bx as usize * 4;
            let di = dy as usize * dst_stride + dx as usize * 4;
            let (b, g, r) = (src[si + bi], src[si + gi], src[si + ri]);
            if compute_damage && fb.data[di] == b && fb.data[di + 1] == g && fb.data[di + 2] == r {
                continue;
            }
            fb.data[di] = b;
            fb.data[di + 1] = g;
            fb.data[di + 2] = r;
            fb.data[di + 3] = 0xff;
            min_x = min_x.min(dx);
            max_x = max_x.max(dx);
            min_y = min_y.min(dy);
            max_y = max_y.max(dy);
        }
    }
    (compute_damage && max_x >= min_x).then(|| Rectangle {
        x: min_x as i16,
        y: min_y as i16,
        width: (max_x - min_x + 1) as u16,
        height: (max_y - min_y + 1) as u16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `w`x`h` XRGB source where pixel (x, y) has blue = x, green = y.
    fn source(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                v[i] = x as u8;
                v[i + 1] = y as u8;
                v[i + 2] = 0x80;
                v[i + 3] = 0xff;
            }
        }
        v
    }

    fn pixel(fb: &Framebuffer, x: i32, y: i32) -> (u8, u8) {
        let out = fb.read_rect(x, y, 1, 1);
        (out[0], out[1])
    }

    #[test]
    fn rotate90_lands_in_the_transformed_rect() {
        let fb = Framebuffer::default();
        fb.ensure(10, 10);
        let src = source(4, 2);
        let (bbox, _, _) =
            fb.blit_diff(3, 5, 4, 2, 16, &src, false, Transform::Rotate90, true, XRGB);
        // a 4x2 buffer occupies a 2x4 rect at (3, 5)
        let b = bbox.unwrap();
        assert_eq!((b.x, b.y, b.width, b.height), (3, 5, 2, 4));
        // clockwise: buffer top-left (0,0) -> screen (1,0) within that rect
        assert_eq!(pixel(&fb, 4, 5), (0, 0));
        // buffer bottom-left (0,1) -> screen (0,0)
        assert_eq!(pixel(&fb, 3, 5), (0, 1));
        // buffer top-right (3,0) -> screen (1,3)
        assert_eq!(pixel(&fb, 4, 8), (3, 0));
        // nothing outside the rect was touched
        assert_eq!(pixel(&fb, 2, 5), (0, 0));
        assert_eq!(pixel(&fb, 5, 5), (0, 0));
    }

    #[test]
    fn transformed_blit_reports_only_the_changed_bbox() {
        let fb = Framebuffer::default();
        fb.ensure(4, 4);
        let mut src = source(4, 2);
        fb.blit_diff(
            0,
            0,
            4,
            2,
            16,
            &src,
            false,
            Transform::Rotate270,
            true,
            XRGB,
        );
        let (unchanged, _, _) = fb.blit_diff(
            0,
            0,
            4,
            2,
            16,
            &src,
            false,
            Transform::Rotate270,
            true,
            XRGB,
        );
        assert!(unchanged.is_none());
        // change buffer pixel (3, 0): under 270 (counter-clockwise) it is screen (0, 0)
        src[3 * 4 + 2] = 0x00;
        let (bbox, _, _) = fb.blit_diff(
            0,
            0,
            4,
            2,
            16,
            &src,
            false,
            Transform::Rotate270,
            true,
            XRGB,
        );
        let b = bbox.unwrap();
        assert_eq!((b.x, b.y, b.width, b.height), (0, 0, 1, 1));
    }

    #[test]
    fn y_invert_is_applied_before_the_transform() {
        let fb = Framebuffer::default();
        fb.ensure(4, 4);
        let src = source(4, 2);
        fb.blit_diff(
            0,
            0,
            4,
            2,
            16,
            &src,
            true,
            Transform::Rotate180,
            false,
            XRGB,
        );
        // y_invert makes the last source row the first, then 180 flips it back
        // to the bottom and mirrors x: buffer (0, 1) -> (3, 1)
        assert_eq!(pixel(&fb, 3, 1), (0, 1));
        assert_eq!(pixel(&fb, 3, 0), (0, 0));
    }

    #[test]
    fn normal_blit_is_unaffected() {
        let fb = Framebuffer::default();
        fb.ensure(6, 6);
        let src = source(4, 2);
        let (bbox, _, _) = fb.blit_diff(1, 1, 4, 2, 16, &src, false, Transform::Normal, true, XRGB);
        let b = bbox.unwrap();
        assert_eq!((b.x, b.y, b.width, b.height), (1, 1, 4, 2));
        assert_eq!(pixel(&fb, 1, 1), (0, 0));
        assert_eq!(pixel(&fb, 4, 2), (3, 1));
    }
}
