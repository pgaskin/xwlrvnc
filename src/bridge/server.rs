use std::sync::{Arc, Mutex};

use crate::bridge::capture::Framebuffer;
use crate::bridge::clipboard::Clipboard;
use crate::bridge::cursor::CursorState;
use crate::bridge::damage::DamageSink;
use crate::bridge::event::EventSink;
use crate::bridge::input::Input;
use crate::bridge::keymap::KeyTable;
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
    /// X keysym and modifier tables built from the compositor keymap (`None`
    /// until received).
    pub keymap: Mutex<Option<KeyTable>>,
}

impl Server {
    pub fn geometry(&self) -> Geometry {
        let s = self.screen.lock().unwrap();
        Geometry {
            width: s.width,
            height: s.height,
        }
    }

    /// Pushes the screen model out to everything derived from it (the
    /// framebuffer size, and the input geometry and output layout), and with
    /// `notify`, tells the X clients that asked. Call after any change to
    /// `screen`, whether the compositor's outputs moved or an X client
    /// reconfigured a CRTC. Returns the resulting screen size.
    pub fn sync_screen(&self, notify: bool) -> Geometry {
        let (geom, timestamp, config_timestamp, rects) = {
            let s = self.screen.lock().unwrap();
            (
                Geometry {
                    width: s.width,
                    height: s.height,
                },
                s.timestamp,
                s.config_timestamp,
                s.layout_rects(),
            )
        };
        self.framebuffer
            .ensure(u32::from(geom.width), u32::from(geom.height));
        self.input.set_layout(rects);
        self.input.set_geometry(geom.width, geom.height);
        if notify {
            self.events
                .screen_changed(geom.width, geom.height, timestamp, config_timestamp);
        }
        geom
    }
}
