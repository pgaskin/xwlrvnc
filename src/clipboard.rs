//! Shared clipboard bridge state between the Wayland data-control client and
//! the X selection handlers.
//!
//! Read direction (Wayland -> X): the Wayland thread records the current offer
//! and its mime types; X threads pipe the data out on demand for
//! `ConvertSelection`.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Mutex;

use wayland_client::backend::ObjectId;
use wayland_client::{Connection, Proxy};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::ExtDataControlOfferV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1;

/// A data-control offer that may come from either ext-data-control-v1 or
/// zwlr-data-control-v1. Both protocols expose the same receive/destroy API.
#[derive(Clone)]
pub enum DataOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl DataOffer {
    pub fn receive(&self, mime: String, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(o) => o.receive(mime, fd),
            Self::Wlr(o) => o.receive(mime, fd),
        }
    }
    pub fn destroy(&self) {
        match self {
            Self::Ext(o) => o.destroy(),
            Self::Wlr(o) => o.destroy(),
        }
    }
    pub fn id(&self) -> ObjectId {
        match self {
            Self::Ext(o) => o.id(),
            Self::Wlr(o) => o.id(),
        }
    }
}

/// The synthetic window we report as the owner of Wayland-backed selections.
pub const OWNER_WINDOW: u32 = 0x0000_016c;

/// Mime types we advertise to Wayland for X-owned text selections.
pub const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sel {
    Clipboard,
    Primary,
}

/// Creates a data-control source for a selection X has taken ownership of.
/// Installed by the Wayland thread; called from X threads.
pub type SourceFactory = Box<dyn Fn(Sel) + Send + Sync>;

#[derive(Default)]
struct Selection {
    offer: Option<DataOffer>,
    mimes: Vec<String>,
    /// Server time (ms) this selection was last acquired — served for the X
    /// TIMESTAMP target so polling clients (vncagent) can detect changes.
    timestamp: u32,
}

#[derive(Default)]
pub struct Clipboard {
    inner: Mutex<Inner>,
    /// Installed by the Wayland thread to push X-owned selections to Wayland.
    factory: Mutex<Option<SourceFactory>>,
}

#[derive(Default)]
struct Inner {
    conn: Option<Connection>,
    clipboard: Selection,
    primary: Selection,
    /// Bumped on every selection change; used as the X selection timestamp.
    serial: u32,
    /// Data X owns and serves to Wayland, keyed by selection.
    x_data: HashMap<Sel, Vec<u8>>,
}

impl Inner {
    fn sel(&self, sel: Sel) -> &Selection {
        match sel {
            Sel::Clipboard => &self.clipboard,
            Sel::Primary => &self.primary,
        }
    }
    fn sel_mut(&mut self, sel: Sel) -> &mut Selection {
        match sel {
            Sel::Clipboard => &mut self.clipboard,
            Sel::Primary => &mut self.primary,
        }
    }
}

impl Clipboard {
    /// Stores the Wayland connection (used to flush `receive` requests issued
    /// from X threads).
    pub fn set_connection(&self, conn: Connection) {
        self.inner.lock().unwrap().conn = Some(conn);
    }

    /// Records a new selection offer (from the Wayland thread). Returns the new
    /// serial. The previous offer for this selection, if any, is returned so the
    /// caller can destroy it.
    pub fn set_offer(
        &self,
        sel: Sel,
        offer: Option<DataOffer>,
        mimes: Vec<String>,
    ) -> (u32, Option<DataOffer>) {
        let mut g = self.inner.lock().unwrap();
        g.serial = g.serial.wrapping_add(1);
        let serial = g.serial;
        let ts = crate::event::server_time_ms();
        let slot = g.sel_mut(sel);
        let old = slot.offer.take();
        slot.offer = offer;
        slot.mimes = mimes;
        slot.timestamp = ts;
        (serial, old)
    }

    /// The server time (ms) this selection was last acquired (for TIMESTAMP).
    pub fn timestamp(&self, sel: Sel) -> u32 {
        self.inner.lock().unwrap().sel(sel).timestamp
    }

    pub fn has(&self, sel: Sel) -> bool {
        self.inner.lock().unwrap().sel(sel).offer.is_some()
    }

    pub fn mimes(&self, sel: Sel) -> Vec<String> {
        self.inner.lock().unwrap().sel(sel).mimes.clone()
    }

    /// Reads the current selection data for `mime` by piping it out of the
    /// compositor. Blocks until the compositor closes its end.
    pub fn read(&self, sel: Sel, mime: &str) -> Option<Vec<u8>> {
        let (conn, offer) = {
            let g = self.inner.lock().unwrap();
            let s = g.sel(sel);
            (g.conn.clone()?, s.offer.clone()?)
        };
        let (mut rx, tx) = std::io::pipe().ok()?;
        offer.receive(mime.to_string(), tx.as_fd());
        conn.flush().ok()?;
        drop(tx); // so we see EOF once the compositor finishes writing
        let mut buf = Vec::new();
        rx.read_to_end(&mut buf).ok()?;
        Some(buf)
    }

    // --- X -> Wayland (write direction) ---

    pub fn set_source_factory(&self, factory: SourceFactory) {
        *self.factory.lock().unwrap() = Some(factory);
    }

    /// Records the data X will serve for `sel` and offers it on Wayland.
    pub fn offer_to_wayland(&self, sel: Sel, data: Vec<u8>) {
        self.inner.lock().unwrap().x_data.insert(sel, data);
        if let Some(factory) = self.factory.lock().unwrap().as_ref() {
            factory(sel);
        }
    }

    /// The data X is serving for `sel` (for a data-control `send`).
    pub fn x_data(&self, sel: Sel) -> Vec<u8> {
        self.inner.lock().unwrap().x_data.get(&sel).cloned().unwrap_or_default()
    }
}
