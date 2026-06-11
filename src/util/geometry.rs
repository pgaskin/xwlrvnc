//! Shared geometry/unit helpers.

use x11rb_protocol::protocol::xproto::Rectangle;

#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub width: u16,
    pub height: u16,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
        }
    }
}

impl std::str::FromStr for Geometry {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let (w, h) = s
            .split_once(['x', 'X'])
            .ok_or_else(|| "expected WIDTHxHEIGHT".to_string())?;
        Ok(Geometry {
            width: w.trim().parse().map_err(|_| "invalid width".to_string())?,
            height: h.trim().parse().map_err(|_| "invalid height".to_string())?,
        })
    }
}

pub fn bbox(rects: &[Rectangle]) -> Option<Rectangle> {
    let mut it = rects.iter().filter(|r| r.width > 0 && r.height > 0);
    let first = it.next()?;
    let (mut x1, mut y1) = (i32::from(first.x), i32::from(first.y));
    let (mut x2, mut y2) = (x1 + i32::from(first.width), y1 + i32::from(first.height));
    for r in it {
        x1 = x1.min(i32::from(r.x));
        y1 = y1.min(i32::from(r.y));
        x2 = x2.max(i32::from(r.x) + i32::from(r.width));
        y2 = y2.max(i32::from(r.y) + i32::from(r.height));
    }
    Some(Rectangle {
        x: x1 as i16,
        y: y1 as i16,
        width: (x2 - x1) as u16,
        height: (y2 - y1) as u16,
    })
}

/// Approximate millimeters for a pixel count at 96 DPI (`25.4 mm/in + 96 px/in`).
pub fn mm(pixels: u16) -> u32 {
    u32::from(pixels) * 254 / 960
}
