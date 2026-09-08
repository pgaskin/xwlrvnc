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
    /// takes the fast memcpy path, anything else is shuffled per pixel).
    /// Returns `(changed_bbox, lock_wait_ns, work_ns)`.
    ///
    /// With `compute_damage`, rows are compared and only changed ones copied, and
    /// the bbox of the change comes back in root coords. Without it the whole
    /// overlap is copied blind and the bbox is `None` — for clients polling
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
        compute_damage: bool,
        chan: ChannelMap,
    ) -> (Option<Rectangle>, u64, u64) {
        let t0 = Instant::now();
        let mut fb = self.inner.lock().unwrap();
        let wait_ns = t0.elapsed().as_nanos() as u64;
        let t1 = Instant::now();
        let (fw, fh) = (fb.width as i32, fb.height as i32);
        let dst_stride = fb.width as usize * 4;
        // horizontal overlap of the source with the framebuffer
        let dx0 = ox.max(0);
        let dx1 = (ox + src_width as i32).min(fw);
        if dx1 <= dx0 {
            return (None, wait_ns, t1.elapsed().as_nanos() as u64);
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
        let bbox = (compute_damage && max_y >= min_y).then(|| Rectangle {
            x: dx0 as i16,
            y: min_y as i16,
            width: (dx1 - dx0) as u16,
            height: (max_y - min_y + 1) as u16,
        });
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
        let t0 = Instant::now();
        let fb = self.inner.lock().unwrap();
        let wait_ns = t0.elapsed().as_nanos() as u64;
        let t1 = Instant::now();
        let row_bytes = width as usize * 4;
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
