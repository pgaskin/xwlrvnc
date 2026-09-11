use std::sync::{Arc, Mutex};

use crate::bridge::capture::Framebuffer;
use crate::bridge::clipboard::{Clipboard, Sel};
use crate::bridge::cursor::CursorState;
use crate::bridge::damage::DamageSink;
use crate::bridge::dynres::DynRes;
use crate::bridge::event::EventSink;
use crate::bridge::input::Input;
use crate::bridge::keymap::KeyTable;
use crate::bridge::x11::atom::{Atoms, XA_PRIMARY};
use crate::bridge::x11::randr::Screen;
use crate::config::Config;
use crate::util::Geometry;

/// Shared, connection-independent server state.
pub(crate) struct Server {
    pub config: Config,
    /// Interned atoms, shared by every connection as X shares them.
    pub atoms: Mutex<Atoms>,
    pub screen: Mutex<Screen>,
    pub input: Arc<Input>,
    pub events: EventSink,
    pub clipboard: Clipboard,
    pub framebuffer: Arc<Framebuffer>,
    pub damage: DamageSink,
    pub cursor: CursorState,
    /// Resolution changes an X client asked for, for the Wayland thread to carry
    /// to the compositor (`-dynres`).
    pub dynres: DynRes,
    /// X keysym and modifier tables built from the compositor keymap (`None`
    /// until received).
    pub keymap: Mutex<Option<KeyTable>>,
}

impl Server {
    /// The atom naming a bridged selection. Every connection sees the same
    /// number for it, so it is also the one to put in an event.
    pub fn selection_atom(&self, sel: Sel) -> u32 {
        match sel {
            Sel::Primary => XA_PRIMARY,
            // interning is idempotent: whoever asks first fixes the number
            Sel::Clipboard => self.atoms.lock().unwrap().intern(b"CLIPBOARD", false),
        }
    }

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
