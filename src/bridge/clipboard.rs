//! Shared clipboard state between the Wayland data-control client and the X
//! selection handlers, in both directions.
//!
//! Wayland to X: the Wayland thread records the current offer and its mime
//! types, and X threads pipe the data out on demand for `ConvertSelection`.
//! X to Wayland: X threads stash the data and call the source factory the
//! Wayland thread installed, which mints a data-control source for it.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::{Arc, Mutex, Weak};

use wayland_client::backend::ObjectId;
use wayland_client::{Connection, Proxy};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::ExtDataControlOfferV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1;
use x11rb_protocol::protocol::xproto;

use crate::bridge::event::Client;

/// An offer from either ext-data-control-v1 or zwlr-data-control-v1; the two
/// expose the same receive/destroy API.
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

/// The X client owning a bridged selection, so we can notify it when it loses
/// it.
pub struct XOwner {
    window: u32,
    /// The selection atom as interned on that client's connection (our atoms
    /// are per-connection).
    atom: u32,
    client: Weak<Client>,
}

impl XOwner {
    pub fn window(&self) -> u32 {
        self.window
    }

    pub fn send_clear(&self, timestamp: u32) {
        let Some(client) = self.client.upgrade() else {
            return;
        };
        crate::bridge::event::send(
            &client,
            &xproto::SelectionClearEvent {
                response_type: xproto::SELECTION_CLEAR_EVENT,
                sequence: 0, // stamped by the client's outbox
                time: timestamp,
                owner: self.window,
                selection: self.atom,
            },
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sel {
    Clipboard,
    Primary,
}

/// Mints a data-control source for a selection X has taken ownership of.
/// Installed by the Wayland thread, called from X threads.
pub type SourceFactory = Box<dyn Fn(Sel) + Send + Sync>;

/// What Wayland is currently offering for one selection. Named to avoid
/// confusion with `x11::selection::Selection`, the X side of the same bridge.
#[derive(Default)]
struct Offered {
    offer: Option<DataOffer>,
    mimes: Vec<String>,
    timestamp: u32, // server time it was acquired, for the X TIMESTAMP target
}

#[derive(Default)]
pub struct Clipboard {
    inner: Mutex<Inner>,
    factory: Mutex<Option<SourceFactory>>,
}

#[derive(Default)]
struct Inner {
    conn: Option<Connection>,
    clipboard: Offered,
    primary: Offered,
    /// The X timestamp of the last selection change, using the server clock
    /// (see [`next_timestamp`](Inner::next_timestamp)).
    last_timestamp: u32,
    x_data: HashMap<Sel, Vec<u8>>, // data X owns and serves to Wayland
    /// X client owning each bridged selection, if owned. Server-global (for
    /// RealVNC, vncagent and vncserverui are separate connections and it must
    /// be consistent).
    x_owner: HashMap<Sel, XOwner>,
    /// Published to Wayland, still awaiting the compositor's selection event
    /// for it. See [`take_self_published`](Clipboard::take_self_published).
    self_published: HashSet<Sel>,
}

impl Inner {
    /// The X timestamp for the next selection change.
    ///
    /// Since clients may compare them, it has to come from the same clock as
    /// the other timestamps (e.g., the TIMESTAMP target, PropertyNotify).
    ///
    /// RealVNC's clipboard remembers the timestamp when it last took the
    /// selection itself (i.e., when pasting from the viewer) and ignores
    /// selection changes with an earlier timestamp.
    fn next_timestamp(&mut self) -> u32 {
        let ts = crate::bridge::event::server_time_ms().max(self.last_timestamp.wrapping_add(1));
        self.last_timestamp = ts;
        ts
    }

    fn sel(&self, sel: Sel) -> &Offered {
        match sel {
            Sel::Clipboard => &self.clipboard,
            Sel::Primary => &self.primary,
        }
    }
    fn sel_mut(&mut self, sel: Sel) -> &mut Offered {
        match sel {
            Sel::Clipboard => &mut self.clipboard,
            Sel::Primary => &mut self.primary,
        }
    }
}

impl Clipboard {
    /// Stores the connection used to flush `receive` requests issued from X
    /// threads.
    pub fn set_connection(&self, conn: Connection) {
        self.inner.lock().unwrap().conn = Some(conn);
    }

    /// Records a new offer, returning the X timestamp it was acquired at (for
    /// the TIMESTAMP target and the XFixes notification alike) and the offer it
    /// replaced (which the caller destroys).
    pub fn set_offer(
        &self,
        sel: Sel,
        offer: Option<DataOffer>,
        mimes: Vec<String>,
    ) -> (u32, Option<DataOffer>) {
        let mut g = self.inner.lock().unwrap();
        let ts = g.next_timestamp();
        let slot = g.sel_mut(sel);
        let old = slot.offer.take();
        slot.offer = offer;
        slot.mimes = mimes;
        slot.timestamp = ts;
        (ts, old)
    }

    /// When this selection was last acquired, for the TIMESTAMP target.
    pub fn timestamp(&self, sel: Sel) -> u32 {
        self.inner.lock().unwrap().sel(sel).timestamp
    }

    pub fn has(&self, sel: Sel) -> bool {
        self.inner.lock().unwrap().sel(sel).offer.is_some()
    }

    pub fn mimes(&self, sel: Sel) -> Vec<String> {
        self.inner.lock().unwrap().sel(sel).mimes.clone()
    }

    /// Pipes the current selection data for `mime` out of the compositor,
    /// blocking until it closes its end.
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

    // --- X to Wayland ---

    pub fn set_source_factory(&self, factory: SourceFactory) {
        *self.factory.lock().unwrap() = Some(factory);
    }

    /// Stashes the data X will serve for `sel` and offers it on Wayland.
    pub fn offer_to_wayland(&self, sel: Sel, data: Vec<u8>) {
        {
            let mut g = self.inner.lock().unwrap();
            g.x_data.insert(sel, data);
            g.self_published.insert(sel);
        }
        if let Some(factory) = self.factory.lock().unwrap().as_ref() {
            factory(sel);
        }
    }

    /// Records the X client owning `sel`, returning the old owner (which must
    /// be sent a SelectionClear).
    pub fn set_x_owner(
        &self,
        sel: Sel,
        window: u32,
        atom: u32,
        client: &Arc<Client>,
    ) -> Option<XOwner> {
        let owner = XOwner {
            window,
            atom,
            client: Arc::downgrade(client),
        };
        self.inner.lock().unwrap().x_owner.insert(sel, owner)
    }

    /// Drops the X owner of `sel`, when it releases the selection or Wayland
    /// takes the selection, returning the owner (so it can be notified).
    pub fn clear_x_owner(&self, sel: Sel) -> Option<XOwner> {
        self.inner.lock().unwrap().x_owner.remove(&sel)
    }

    /// The window of the X client owning `sel`, if not owned by Wayland.
    pub fn x_owner(&self, sel: Sel) -> Option<u32> {
        self.inner
            .lock()
            .unwrap()
            .x_owner
            .get(&sel)
            .map(XOwner::window)
    }

    /// Whether we published `sel` ourselves and have not yet accounted for it,
    /// clearing the record. The compositor announces the source we published
    /// exactly as it would any other app's, so without this the X owner we just
    /// recorded looks superseded. A foreign copy arriving first clears the
    /// record instead, which resolves on the next change.
    pub fn take_self_published(&self, sel: Sel) -> bool {
        self.inner.lock().unwrap().self_published.remove(&sel)
    }

    /// The data X is serving for `sel`, for a data-control `send`.
    pub fn x_data(&self, sel: Sel) -> Vec<u8> {
        self.inner
            .lock()
            .unwrap()
            .x_data
            .get(&sel)
            .cloned()
            .unwrap_or_default()
    }
}
