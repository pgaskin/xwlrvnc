//! A minimal X11 server: just enough of the protocol to back `vncagent-x11`.
//!
//! We don't draw anything or manage real windows. Input requests (XTest) are
//! turned into wlr virtual-pointer / zwp virtual-keyboard requests, screen
//! contents come from wlr-screencopy, and the clipboard is bridged to the
//! Wayland data-control protocol. Everything else is answered with the smallest
//! plausible reply to keep Xlib happy.

pub mod conn;
pub mod ext;
pub mod screen;
pub mod setup;
pub mod wire;

/// Root window resource id. Chosen outside the client resource-id range
/// (`resource_id_base`/`mask` in [`setup`]) so it can never collide.
pub const ROOT_WINDOW: u32 = 0x0000_016b;
/// Default colormap resource id (also outside the client range).
pub const ROOT_COLORMAP: u32 = 0x0000_0020;
/// TrueColor 24-bit visual id reported in the connection setup.
pub const ROOT_VISUAL: u32 = 0x0000_0021;
/// Depth of the root visual.
pub const ROOT_DEPTH: u8 = 24;

/// Current screen geometry. Filled in from the Wayland output once the capture
/// backend is wired up; until then this default is what we report.
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
    /// Parses `WIDTHxHEIGHT`, e.g. `1920x1080`.
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
