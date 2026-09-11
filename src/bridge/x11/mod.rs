//! A minimal X11 server implementing enough for RealVNC to work and for Xlib
//! not to fail.
pub(crate) mod atom;
pub mod conn;
pub mod ext;
mod mit_shm;
mod property;
pub mod randr;
mod selection;
mod targets;
mod window;
mod xfixes;

// we hardcode some ID choices for simplicity

/// First resource ID assigned to clients (each conn gets a non-overlapping
/// allocation after this).
pub const CLIENT_RESOURCE_ID_BASE: u32 = 0x0040_0000;

/// Root window resource ID (arbitrary, must be outside client
/// `resource_id_base`/`mask` in [`conn::setup`]).
pub const ROOT_WINDOW: u32 = 0x0000_016b;

/// Default colormap resource ID (arbitrary, also outside client range).
pub const ROOT_COLORMAP: u32 = 0x0000_0020;

/// TrueColor 24-bit visual ID (arbitrary, also outside client range).
pub const ROOT_VISUAL: u32 = 0x0000_0021;

/// Window ID used to receive selections.
pub const SELECTION_FETCH_WINDOW: u32 = ROOT_WINDOW;

/// First RandR output ID (arbitrary, also outside client range) (leave enough
/// room).
const RANDR_OUTPUT_BASE: u32 = 0x0000_0040;

/// Depth of the root visual.
pub const ROOT_DEPTH: u8 = 24;
