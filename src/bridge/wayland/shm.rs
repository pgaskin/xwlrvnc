//! wl_shm-backed capture buffers and the pixel-format helpers used to convert
//! captured frames into our native XRGB framebuffer.
//!
//! These are the leaf helpers shared by both capture backends: [`ShmBuffer`]
//! wraps a memfd-backed `wl_buffer` the compositor copies into, and the
//! `pixel_layout`/`channel_map`/`blit_channels` functions resolve the byte
//! permutation needed to turn an arbitrary byte-ordered 8888 `wl_shm` format
//! into the `[blue, green, red]` channel offsets [`crate::bridge::capture`] consumes.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wayland_client::QueueHandle;
use wayland_client::protocol::{wl_buffer, wl_shm, wl_shm_pool};

use super::State;

/// A wl_shm-backed buffer the compositor copies a captured frame into.
pub(crate) struct ShmBuffer {
    pub buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    pub map: *mut u8,
    pub size: usize,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: wl_shm::Format,
}

// Only ever touched on the Wayland thread; the raw mapping pointer just needs to
// ride along inside `State` (which the thread owns).
unsafe impl Send for ShmBuffer {}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe { nix::libc::munmap(self.map.cast(), self.size) };
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// Byte offsets, within a little-endian 4-byte pixel, of the `[blue, green,
/// red]` channels and (optionally) the alpha channel. Enough to convert any
/// byte-ordered 8888 wl_shm format to/from our native layout; `None` for
/// packed/float formats (10-bit etc.) that need bit unpacking, not a byte
/// permutation. Offsets are the reverse of the DRM fourcc channel order.
pub(crate) fn pixel_layout(
    fmt: wl_shm::Format,
) -> Option<(crate::bridge::capture::ChannelMap, Option<usize>)> {
    use wl_shm::Format as F;
    Some(match fmt {
        F::Xrgb8888 => ([0, 1, 2], None),    // memory B,G,R,x
        F::Argb8888 => ([0, 1, 2], Some(3)), // memory B,G,R,A
        F::Xbgr8888 => ([2, 1, 0], None),    // memory R,G,B,x
        F::Abgr8888 => ([2, 1, 0], Some(3)), // memory R,G,B,A
        F::Rgbx8888 => ([1, 2, 3], None),    // memory x,B,G,R
        F::Rgba8888 => ([1, 2, 3], Some(0)), // memory A,B,G,R
        F::Bgrx8888 => ([3, 2, 1], None),    // memory x,R,G,B
        F::Bgra8888 => ([3, 2, 1], Some(0)), // memory A,R,G,B
        _ => return None,
    })
}

/// The blit channel map (B,G,R source offsets) for converting a captured buffer
/// in `fmt` into our XRGB framebuffer.
pub(crate) fn channel_map(fmt: wl_shm::Format) -> Option<crate::bridge::capture::ChannelMap> {
    pixel_layout(fmt).map(|(bgr, _)| bgr)
}

/// Resolves the blit channel map for a buffer format, warning once-ish and
/// falling back to the native order for formats we can't byte-permute.
pub(crate) fn blit_channels(
    wl_name: u32,
    fmt: wl_shm::Format,
) -> crate::bridge::capture::ChannelMap {
    channel_map(fmt).unwrap_or_else(|| {
        crate::warning!(
            "output {wl_name} capture format {fmt:?} is not a byte-ordered \
             8888 format; colors may be wrong"
        );
        crate::bridge::capture::XRGB
    })
}

/// Creates a memfd-backed wl_shm buffer of the given geometry/format.
pub(crate) fn create_shm_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
) -> Option<ShmBuffer> {
    let size = stride as usize * height as usize;
    if size == 0 {
        return None;
    }
    // SAFETY: standard memfd_create + ftruncate + mmap dance.
    let fd = unsafe {
        let fd = nix::libc::memfd_create(c"wl-uinput-proxy-capture".as_ptr(), 0);
        if fd < 0 {
            return None;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        if nix::libc::ftruncate(fd.as_fd().as_raw_fd(), size as nix::libc::off_t) < 0 {
            return None;
        }
        fd
    };
    let map = unsafe {
        nix::libc::mmap(
            std::ptr::null_mut(),
            size,
            nix::libc::PROT_READ | nix::libc::PROT_WRITE,
            nix::libc::MAP_SHARED,
            fd.as_fd().as_raw_fd(),
            0,
        )
    };
    if map == nix::libc::MAP_FAILED {
        return None;
    }
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        qh,
        (),
    );
    Some(ShmBuffer {
        buffer,
        pool,
        map: map.cast(),
        size,
        width,
        height,
        stride,
        format,
    })
}
