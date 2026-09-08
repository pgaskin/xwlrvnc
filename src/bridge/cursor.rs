use std::sync::Mutex;

/// Cursor image and position, written by the Wayland capture thread and read by
/// X connection threads for XFixes `GetCursorImage`/`GetCursorImageAndName`.
#[derive(Default)]
pub struct CursorState {
    inner: Mutex<CursorInner>,
}

#[derive(Default)]
struct CursorInner {
    serial: u32, // monotonic, 0 if nothing captured yet
    width: u16,
    height: u16,
    xhot: u16, // hotspot within the image
    yhot: u16,
    x: i16, // hotspot position in root coordinates
    y: i16,
    image: Vec<u32>, // ARGB, native endian (0xAARRGGBB on little-endian)
}

/// A consistent copy of the cursor, taken under the lock.
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
    /// Replaces the image, returning the new serial for
    /// [`EventSink::cursor_changed`](crate::bridge::event::EventSink::cursor_changed).
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
