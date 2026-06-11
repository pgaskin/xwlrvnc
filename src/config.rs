//! The runtime configuration: backend choices and the wrapped command, parsed
//! X11-style from the command line. The parser, help text and `ArgEnum` trait
//! live in [`crate::util::args`]; this module just defines the option enums and
//! the [`Config`] fields they're generated from.

use crate::util::{ArgEnum, Geometry};

define_config! {
    pub struct Config {
        /// display number
        display: Option<u32> = value("number"),
        /// write the display number to this fd once the server is ready
        displayfd: Option<u32> = value("fd"),
        /// screen capture protocol.
        screen: ScreenType = choice("auto"),
        /// clipboard protocol
        clipboard: ClipboardType = choice("auto"),
        /// cursor source (if none, it's baked into the screen capture)
        cursor: CursorType = choice("auto"),
        /// use the wayland seat with this name
        seat: Option<String> = value("name"),
        /// fallback screen size before outputs are known
        geometry: Option<Geometry> = value("WxH"),
        /// cap the screen capture frame rate (default is optimized for vncagent-x11)
        fps: Option<u32> = value("n"),
        /// don't advertise the DAMAGE extension (force clients to poll)
        nodamage: bool = flag,
        /// don't bridge the PRIMARY selection (middle-click paste of selected text)
        noprimary: bool = flag,
        /// run until stopped without launching a command
        nowrap: bool = flag,
        /// profile the screen capture
        profile: bool = flag,
        /// trace requests to stderr
        xtrace: bool = flag,
        /// suppress informational logging
        quiet: bool = flag,
        /// log extra detail
        verbose: bool = flag,
    }
}

impl ArgEnum for CursorType {
    const VARIANTS: &'static [&'static str] = &["auto", "none"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "none" => Self::Baked,
            _ => return None,
        })
    }
}

impl ArgEnum for ScreenType {
    const VARIANTS: &'static [&'static str] = &["auto", "none", "screencopy", "imagecopy"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "none" => Self::None,
            "screencopy" => Self::Screencopy,
            "imagecopy" => Self::Imagecopy,
            _ => return None,
        })
    }
}

impl ArgEnum for ClipboardType {
    const VARIANTS: &'static [&'static str] = &["auto", "none", "wlr", "ext"];
    fn from_arg(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "none" => Self::None,
            "wlr" => Self::Wlr,
            "ext" => Self::Ext,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorType {
    Auto,
    Baked,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenType {
    Auto,
    None,
    Screencopy,
    Imagecopy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardType {
    Auto,
    None,
    Wlr,
    Ext,
}

impl ScreenType {
    pub fn wants_screencopy(self) -> bool {
        matches!(self, Self::Auto | Self::Screencopy)
    }
    pub fn wants_imagecopy(self) -> bool {
        matches!(self, Self::Auto | Self::Imagecopy)
    }
}

impl ClipboardType {
    pub fn wants_wlr(self) -> bool {
        matches!(self, Self::Auto | Self::Wlr)
    }
    pub fn wants_ext(self) -> bool {
        matches!(self, Self::Auto | Self::Ext)
    }
}

impl Config {
    pub fn damage(&self) -> bool {
        !self.nodamage
    }
}
