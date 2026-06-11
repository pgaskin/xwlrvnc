use std::sync::{Arc, Mutex};

use crate::bridge::capture::Framebuffer;
use crate::bridge::clipboard::Clipboard;
use crate::bridge::cursor::CursorState;
use crate::bridge::damage::DamageSink;
use crate::bridge::event::EventSink;
use crate::bridge::input::Input;
use crate::bridge::x11::randr::Screen;
use crate::config::Config;
use crate::util::Geometry;

/// Shared, connection-independent server state.
pub(crate) struct Server {
    pub config: Config,
    pub screen: Mutex<Screen>,
    pub input: Arc<Input>,
    pub events: EventSink,
    pub clipboard: Clipboard,
    pub framebuffer: Arc<Framebuffer>,
    pub damage: DamageSink,
    pub cursor: CursorState,
    /// X keysym table built from the compositor keymap (`None` until received).
    pub keymap: Mutex<Option<Vec<u32>>>,
}

impl Server {
    pub fn geometry(&self) -> Geometry {
        let s = self.screen.lock().unwrap();
        Geometry {
            width: s.width,
            height: s.height,
        }
    }
}
