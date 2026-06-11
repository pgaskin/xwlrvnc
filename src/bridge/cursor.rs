//! Shared cursor state written by the Wayland capture thread and read by X
//! connection threads (for XFixes GetCursorImage / GetCursorImageAndName).

use std::sync::Mutex;

/// Current cursor image and pointer, updated whenever the Wayland cursor
/// capture session delivers a new frame or position.
#[derive(Default)]
pub struct CursorState {
    inner: Mutex<CursorInner>,
}

#[derive(Default)]
struct CursorInner {
    /// Monotonically increasing; 0 = no cursor captured yet.
    pub serial: u32,
    pub width: u16,
    pub height: u16,
    /// Hotspot within the cursor image.
    pub xhot: u16,
    pub yhot: u16,
    /// Cursor hotspot position in virtual-screen (root) coordinates.
    pub x: i16,
    pub y: i16,
    /// ARGB pixels (u32 per pixel, native endian = 0xAARRGGBB on little-endian).
    pub image: Vec<u32>,
}

pub struct CursorSnapshot {
    pub serial: u32,
    pub width: u16,
    pub height: u16,
    pub xhot: u16,
    pub yhot: u16,
    pub x: i16,
    pub y: i16,
    pub image: Vec<u32>,
}

impl CursorState {
    /// Replaces the cursor image (new dimensions, hotspot, pixels). Returns the
    /// new serial, which callers should pass to `EventSink::cursor_changed`.
    pub fn update_image(&self, w: u16, h: u16, xhot: u16, yhot: u16, image: Vec<u32>) -> u32 {
        let mut g = self.inner.lock().unwrap();
        g.width = w;
        g.height = h;
        g.xhot = xhot;
        g.yhot = yhot;
        g.image = image;
        g.serial = g.serial.wrapping_add(1).max(1);
        g.serial
    }

    pub fn update_position(&self, x: i16, y: i16) {
        let mut g = self.inner.lock().unwrap();
        g.x = x;
        g.y = y;
    }

    pub fn snapshot(&self) -> CursorSnapshot {
        let g = self.inner.lock().unwrap();
        CursorSnapshot {
            serial: g.serial,
            width: g.width,
            height: g.height,
            xhot: g.xhot,
            yhot: g.yhot,
            x: g.x,
            y: g.y,
            image: g.image.clone(),
        }
    }

    #[allow(dead_code)]
    pub fn serial(&self) -> u32 {
        self.inner.lock().unwrap().serial
    }
}
