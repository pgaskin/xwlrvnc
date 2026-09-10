//! The leaf helpers both capture backends share: [`ShmBuffer`] wraps a
//! memfd-backed `wl_buffer` for the compositor to copy into, and
//! [`pixel_layout`] and friends resolve the byte permutation that turns an
//! arbitrary byte-ordered 8888 `wl_shm` format into the `[blue, green, red]`
//! offsets [`crate::bridge::capture`] wants.

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

// SAFETY: only ever touched on the Wayland thread, which owns the `State` the
// raw mapping pointer rides along inside.
unsafe impl Send for ShmBuffer {}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.map.cast(), self.size) };
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// Byte offsets within a little-endian 4-byte pixel of the `[blue, green, red]`
/// channels, plus alpha if the format has it — the reverse of the DRM fourcc
/// channel order. `None` for packed or float formats (10-bit and such) that need
/// bit unpacking rather than a byte permutation.
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

/// Just the B,G,R source offsets, for blitting `fmt` into the framebuffer.
pub(crate) fn channel_map(fmt: wl_shm::Format) -> Option<crate::bridge::capture::ChannelMap> {
    pixel_layout(fmt).map(|(bgr, _)| bgr)
}

/// [`channel_map`], warning and falling back to the native order for a format
/// we can't byte-permute.
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

/// Creates a memfd-backed wl_shm buffer of the given geometry and format.
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
    // SAFETY: the standard memfd_create + ftruncate + mmap dance
    let fd = unsafe {
        let fd = libc::memfd_create(c"xwlrvnc-capture".as_ptr(), 0);
        if fd < 0 {
            return None;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        if libc::ftruncate(fd.as_fd().as_raw_fd(), size as libc::off_t) < 0 {
            return None;
        }
        fd
    };
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_fd().as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
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
