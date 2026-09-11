//! Shared clipboard state between the Wayland data-control client and the X
//! selection handlers, in both directions.
//!
//! Wayland to X: the Wayland thread records the current offer and its mime
//! types, and pipes the data out of the owning app when an X client converts
//! the selection.
//! X to Wayland: X threads stash the data and post a job for the Wayland
//! thread, which mints a data-control source for it.
//!
//! Nothing here talks to the compositor itself: every request that would goes
//! through [`Jobs`], so the data-control objects stay on the one thread that
//! dispatches them.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::os::fd::BorrowedFd;
use std::sync::{Arc, Mutex, Weak};

use wayland_client::Proxy;
use wayland_client::backend::ObjectId;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::ExtDataControlOfferV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1;
use x11rb_protocol::protocol::{xfixes, xproto};

use crate::bridge::clipjobs::{Job, Jobs};
use crate::bridge::event::{server_time_ms, time_cmp};

/// An offer from either ext-data-control-v1 or zwlr-data-control-v1; the two
/// expose the same receive/destroy API.
#[derive(Clone)]
pub enum DataOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
    /// A stand-in for the tests, which have no compositor to mint the real
    /// thing.
    #[cfg(test)]
    Fake,
}

impl DataOffer {
    pub fn receive(&self, mime: String, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(o) => o.receive(mime, fd),
            Self::Wlr(o) => o.receive(mime, fd),
            #[cfg(test)]
            Self::Fake => {}
        }
    }
    pub fn destroy(&self) {
        match self {
            Self::Ext(o) => o.destroy(),
            Self::Wlr(o) => o.destroy(),
            #[cfg(test)]
            Self::Fake => {}
        }
    }
    pub fn id(&self) -> ObjectId {
        match self {
            Self::Ext(o) => o.id(),
            Self::Wlr(o) => o.id(),
            #[cfg(test)]
            Self::Fake => unreachable!("a fake offer has no protocol object"),
        }
    }
    /// Whether this is the same offer as `other`.
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ext(a), Self::Ext(b)) => a.id() == b.id(),
            (Self::Wlr(a), Self::Wlr(b)) => a.id() == b.id(),
            #[cfg(test)]
            (Self::Fake, Self::Fake) => true,
            _ => false,
        }
    }
}

/// The synthetic window we report as the owner of Wayland-backed selections.
pub const OWNER_WINDOW: u32 = 0x0000_016c;

/// Mime types we advertise to Wayland for X-owned text selections. Only ever
/// text: `UTF8_STRING` is the one target we ask X owners for.
pub const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// The X client owning a bridged selection, and everything we hold for it.
///
/// An X client that believes it owns a selection does not go and re-read it;
/// it waits for the `SelectionClear` the server owes it. RealVNC's clipboard
/// (in `vncserverui`) is one of those: after a paste from the viewer it takes
/// CLIPBOARD, and from then on it drops every `XFixesSelectionNotify` we send
/// it without issuing a single request, so the Wayland-to-viewer direction
/// stays dead until the session restarts.
pub struct XOwner {
    window: u32,
    client: Weak<crate::bridge::event::Client>,
    /// When it took the selection: its `SetSelectionOwner` time, which with
    /// the window tells this acquisition from a later one by the same window.
    time: u32,
    /// What it gave us when we asked it to convert the selection, once it
    /// has; `None` while the fetch is out, and for good if it refused.
    data: Option<Arc<[u8]>>,
}

impl XOwner {
    /// Whether `client` is the connection that owns the selection. X decides
    /// who is owed a `SelectionClear` per client, not per window: taking a
    /// selection you already hold, on a different window, owes you nothing.
    fn owned_by(&self, client: &Arc<crate::bridge::event::Client>) -> bool {
        self.client
            .upgrade()
            .is_some_and(|c| Arc::ptr_eq(&c, client))
    }

    /// Whether this is still the acquisition a fetch was started for. A reply
    /// landing after a Wayland app or another X client took the selection
    /// answers a question nobody is asking any more.
    fn is(&self, window: u32, time: u32) -> bool {
        self.window == window && self.time == time
    }

    /// Sends the owner the `SelectionClear` it is owed, if it is still around.
    fn send_clear(&self, selection: u32, timestamp: u32) {
        let Some(client) = self.client.upgrade() else {
            return;
        };
        crate::cliplog!("SelectionClear to window {:#x} at {timestamp}", self.window);
        crate::bridge::event::send(
            &client,
            &xproto::SelectionClearEvent {
                response_type: xproto::SELECTION_CLEAR_EVENT,
                sequence: 0, // stamped by the client's outbox
                time: timestamp,
                owner: self.window,
                selection,
            },
        );
    }
}

/// Who owns a selection, and so where every answer about it comes from: the
/// window `GetSelectionOwner` reports, the targets we advertise, and the bytes
/// a conversion is served from. One value, so those cannot disagree — they
/// used to be an owner map, a stored offer and a data map, combined by hand at
/// each read site and each of the four places that changed them.
#[derive(Default)]
enum Owner {
    /// Nobody: never taken, given up, or its owner went away.
    #[default]
    None,
    /// A Wayland app, through the offer the compositor announced for it.
    Wayland {
        offer: DataOffer,
        mimes: Vec<String>,
    },
    /// An X client, on whichever connection.
    X(XOwner),
}

impl Owner {
    /// The owner of an announcement from the compositor.
    fn wayland(offer: Option<DataOffer>, mimes: Vec<String>) -> Self {
        match offer {
            Some(offer) => Self::Wayland { offer, mimes },
            None => Self::None,
        }
    }

    /// The window `GetSelectionOwner` answers with: a Wayland app's selection
    /// is ours to serve, so it gets our synthetic window.
    fn window(&self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Wayland { .. } => Some(OWNER_WINDOW),
            Self::X(o) => Some(o.window),
        }
    }

    /// The offer this owner was announced with, to destroy once it is no
    /// longer the selection.
    fn into_offer(self) -> Option<DataOffer> {
        match self {
            Self::Wayland { offer, .. } => Some(offer),
            _ => None,
        }
    }

    fn x(&self) -> Option<&XOwner> {
        match self {
            Self::X(o) => Some(o),
            _ => None,
        }
    }

    fn x_mut(&mut self) -> Option<&mut XOwner> {
        match self {
            Self::X(o) => Some(o),
            _ => None,
        }
    }
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "nobody"),
            Self::Wayland { mimes, .. } => write!(f, "a wayland app ({} mimes)", mimes.len()),
            Self::X(o) => match &o.data {
                Some(d) => write!(
                    f,
                    "x window {:#x} at {} ({} bytes)",
                    o.window,
                    o.time,
                    d.len()
                ),
                None => write!(f, "x window {:#x} at {} (no value yet)", o.window, o.time),
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Sel {
    Clipboard,
    Primary,
}

/// What an X client's `SetSelectionOwner` amounted to.
#[must_use]
pub enum Take {
    /// Ignored, as X does, because its time is in the future or before the
    /// selection's last change; the client is told nothing.
    Stale,
    /// It took effect, with these to carry out.
    Took(Effects),
}

/// What a transition leaves its caller to do, rather than doing it itself:
/// the offers go on the Wayland thread's queue and the events go out through
/// every client's outbox, and neither may happen under the clipboard lock —
/// that lock is held alone, by one method at a time, which is what keeps the X
/// threads, the Wayland thread and the job queue free of any order between
/// them.
#[must_use = "a transition's events have to be emitted for either side to hear about it"]
#[derive(Default)]
pub struct Effects {
    /// Offers the compositor has replaced; the protocol asks us to destroy
    /// them.
    destroy: Vec<DataOffer>,
    /// The X client the selection was taken from, owed a `SelectionClear`.
    cleared: Option<XOwner>,
    /// What XFixes watchers are owed, if anything. An X client's acquisition
    /// is announced only once we know whether there is anything to serve for
    /// it, so the transition that records it emits nothing (see
    /// [`Clipboard::x_fetched`]).
    notify: Option<Notify>,
    /// When the change happened, which the `SelectionClear` carries.
    at: u32,
}

/// An `XFixesSelectionNotify` a transition calls for. `selection_timestamp` is
/// the selection's last change (dix's `lastTimeChanged`), which is the event's
/// own time for a change of owner but stays put when an owner merely went away.
struct Notify {
    subtype: xfixes::SelectionEvent,
    owner: u32,
    timestamp: u32,
    selection_timestamp: u32,
}

impl Effects {
    /// Hands back what the compositor replaced and sends the X events, in the
    /// order clients need them: the old owner hears it lost the selection
    /// before watchers hear that it changed (RealVNC ignores the notification
    /// otherwise, and never asks for the new value).
    ///
    /// The offers are destroyed by the Wayland thread, even when it is the one
    /// applying this: an X thread must not touch them (see
    /// [`Jobs`](crate::bridge::clipjobs::Jobs)), and one path for both is one
    /// less thing to get wrong.
    pub fn apply(self, server: &crate::bridge::Server, sel: Sel) {
        if !self.destroy.is_empty() {
            // read outside the queue lock, unlike `Jobs::post`'s own check.
            // Safe only because `running` is a one-way latch: set true once at
            // startup and never set back while a thread exists to race us. A
            // reconnect path that flipped it would make this a real race.
            if server.clipboard.jobs.running() {
                server.clipboard.jobs.post(Job::Destroy(self.destroy));
            } else {
                // nothing is dispatching: either the setup roundtrips are
                // still running on this very thread, or the compositor is
                // gone and the request goes nowhere. Nobody can be racing us
                // for these, and an offer nobody destroys is a leak.
                for offer in self.destroy {
                    offer.destroy();
                }
            }
        }
        let selection = server.selection_atom(sel);
        if let Some(prev) = self.cleared {
            prev.send_clear(selection, self.at);
        }
        if let Some(n) = self.notify {
            server.events.selection_changed(
                sel,
                selection,
                n.subtype,
                n.owner,
                n.timestamp,
                n.selection_timestamp,
            );
        }
    }
}

/// The value an X owner gave us for a selection, as the data-control source
/// publishing it carries it: a source keeps the bytes it was made with, so a
/// receiver that asked for the old one just before we published a new one
/// gets what it asked for, not the replacement.
#[derive(Clone)]
pub struct Published {
    pub sel: Sel,
    pub data: Arc<[u8]>,
}

/// Publishes a data-control source for `sel` carrying the given value, or with
/// `None` withdraws ours, when X owns a selection whose value we cannot carry.
/// Built and held by the Wayland thread, and only ever called there, out of a
/// [`Job::Publish`].
pub type Publisher = Box<dyn Fn(Sel, Option<Arc<[u8]>>) + Send>;

/// Whether an announcement from the compositor is one we caused, and of which
/// kind, since it carries nothing else to tell it by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Echo {
    /// A source of ours, advertising exactly [`TEXT_MIMES`].
    Source,
    /// Our withdrawal: an announcement of no selection at all.
    Withdrawn,
}

/// Announcements we are still owed for `set_selection` calls of ours that the
/// compositor has not yet echoed. More than this and it is not echoing at all.
const MAX_ECHOES: usize = 16;

/// One selection's state on both sides. Named to avoid confusion with
/// `x11::selection::Selection`, the per-connection X side of the same bridge.
#[derive(Default)]
struct SelState {
    /// Who owns it, and everything that follows from that.
    owner: Owner,
    /// Bumped on every ownership change, so work handed to another thread can
    /// say which owner it was prepared for. A conversion resolves its mime
    /// against the owner's mime list on an X thread and reads the value on the
    /// Wayland thread a pass later; without this the two can be different
    /// owners, and asking the new one for a mime only the old one offered is
    /// refused by the compositor as an instant EOF — which would reach the
    /// requestor as a successful empty paste.
    generation: u64,
    /// When the owner last changed, on the server clock (dix's
    /// `lastTimeChanged`): the `TIMESTAMP` target, the XFixes
    /// `selection_timestamp`, and the floor under `SetSelectionOwner` times.
    /// It belongs to the selection rather than to the owner because X keeps it
    /// when an owner goes away (`dix/selection.c` clears the client and window
    /// and leaves the time alone).
    last_changed: u32,
    /// Announcements the compositor still owes us, oldest first. It announces
    /// the source we published exactly as it would any other app's, so
    /// without this the X owner we just recorded looks superseded. It outlives
    /// any one owner: the withdrawal we send when an owner dies is echoed back
    /// long after the owner record is gone.
    echoes: VecDeque<Echo>,
    /// Whether the compositor's selection is (or is about to be) a source of
    /// ours, and so ours to withdraw. Distinct from an X owner having given us
    /// its value: a new X owner's is unknown until fetched, while the previous
    /// owner's source is still what Wayland shows.
    published: bool,
    /// Whether the data-control device carries this selection at all: a wlr v1
    /// device has no primary selection, so publishing to it is not merely
    /// unheard but impossible, and no echo is ever owed for it.
    publishable: bool,
}

impl SelState {
    /// Moves the selection to a new owner, returning the one it replaces and
    /// tracing the transition for `-cliptrace`.
    fn enter(&mut self, sel: Sel, why: &str, owner: Owner) -> Owner {
        crate::cliplog!("{sel:?}: {} -> {why} -> {owner}", self.owner);
        self.generation = self.generation.wrapping_add(1);
        std::mem::replace(&mut self.owner, owner)
    }

    /// Whether the compositor's announcement of this selection is the echo of
    /// our own last publication (or withdrawal), consuming the record if so. A
    /// source of ours advertises exactly [`TEXT_MIMES`], in order, and a
    /// withdrawal is announced as no offer; anything else is a foreign app's,
    /// however many echoes we are still owed — it won at the compositor, and
    /// ours are still coming.
    fn take_echo(&mut self, sel: Sel, has_offer: bool, mimes: &[String]) -> bool {
        let ours = match self.echoes.front() {
            Some(Echo::Source) => has_offer && mimes.iter().eq(TEXT_MIMES),
            Some(Echo::Withdrawn) => !has_offer,
            None => false,
        };
        if ours {
            self.echoes.pop_front();
            // what the compositor shows now is settled, whatever raced ahead
            self.published = has_offer;
        } else if !self.echoes.is_empty() {
            crate::cliplog!(
                "{sel:?}: a foreign offer arrived ahead of {} of ours still unannounced",
                self.echoes.len()
            );
        }
        ours
    }
}

#[derive(Default)]
pub struct Clipboard {
    inner: Mutex<Inner>,
    /// Everything the compositor has to be told, on its way to the Wayland
    /// thread. Held here rather than beside it in [`Server`](crate::bridge::Server)
    /// so that the transitions below can post without a second handle.
    jobs: Jobs,
}

#[derive(Default)]
struct Inner {
    clipboard: SelState,
    primary: SelState,
    /// The X timestamp of the last selection change on either selection; see
    /// [`next_timestamp`](Inner::next_timestamp).
    last_timestamp: u32,
}

impl Inner {
    /// The X timestamp to stamp a Wayland-side selection change with.
    ///
    /// It has to come from the same clock as every other timestamp we hand out
    /// (the TIMESTAMP target, PropertyNotify), because clients compare them:
    /// RealVNC's clipboard remembers the timestamp it used when it last took
    /// the selection itself (on a paste from the viewer) and from then on
    /// ignores every selection change stamped earlier than that, so a counter
    /// here would silently kill the Wayland-to-viewer direction for good. Two
    /// changes inside one millisecond are pushed apart, since a client that
    /// only takes strictly newer timestamps would drop the second; the server
    /// clock is told, so nothing stamped afterwards falls behind it.
    fn next_timestamp(&mut self) -> u32 {
        let now = server_time_ms();
        let ts = if time_cmp(now, self.last_timestamp).is_gt() {
            now
        } else {
            self.last_timestamp.wrapping_add(1)
        };
        self.last_timestamp = ts;
        crate::bridge::event::note_selection_time(ts);
        ts
    }

    /// Records an X-side change at `time`, which the client chose (from a
    /// `PropertyNotify` of ours, so it is never ahead of the clock).
    fn changed_at(&mut self, sel: Sel, time: u32) {
        if time_cmp(time, self.last_timestamp).is_gt() {
            self.last_timestamp = time;
        }
        self.sel_mut(sel).last_changed = time;
    }

    fn sel(&self, sel: Sel) -> &SelState {
        match sel {
            Sel::Clipboard => &self.clipboard,
            Sel::Primary => &self.primary,
        }
    }
    fn sel_mut(&mut self, sel: Sel) -> &mut SelState {
        match sel {
            Sel::Clipboard => &mut self.clipboard,
            Sel::Primary => &mut self.primary,
        }
    }
}

/// dix's two rules for ignoring a `SetSelectionOwner`: one stamped after the
/// server's clock, or before the selection's last change, is dropped without
/// a word (`dix/selection.c`, `dixSetSelectionOwner`). The second is what
/// makes a slow client's stale request lose to the change that overtook it.
pub fn stale_owner_time(time: u32, now: u32, last_changed: u32) -> bool {
    time_cmp(time, now) == Ordering::Greater || time_cmp(time, last_changed) == Ordering::Less
}

/// Where the bytes answering a conversion come from.
pub enum Value {
    /// Nowhere: nobody owns the selection, or its X owner had nothing we could
    /// carry. The requestor is refused.
    None,
    /// An X owner's value, which is already here.
    Here(Vec<u8>),
    /// A Wayland app's, which has to be piped out of it: the requestor waits
    /// for a [`Job::Receive`] to finish.
    FromWayland,
}

impl Clipboard {
    /// The queue of work for the Wayland thread (which drains it; everyone
    /// else posts to it).
    pub fn jobs(&self) -> &Jobs {
        &self.jobs
    }

    /// Records whether the data-control device the Wayland thread bound
    /// carries `sel` at all; see [`SelState::publishable`].
    pub fn set_publishable(&self, sel: Sel, publishable: bool) {
        self.inner.lock().unwrap().sel_mut(sel).publishable = publishable;
    }

    // --- readers: everything X asks about a selection ---

    /// The window `GetSelectionOwner` answers with for `sel`.
    pub fn owner_window(&self, sel: Sel) -> Option<u32> {
        self.inner.lock().unwrap().sel(sel).owner.window()
    }

    /// When `sel`'s owner last changed, for the TIMESTAMP target and the
    /// XFixes `selection_timestamp`.
    pub fn last_changed(&self, sel: Sel) -> u32 {
        self.inner.lock().unwrap().sel(sel).last_changed
    }

    /// The mime types `sel` can be converted to, which the X targets are
    /// derived from, together with the ownership generation they belong to.
    ///
    /// An X owner is only ever asked for UTF8_STRING, so what we can offer on
    /// its behalf is text and nothing else — and until its value is in (or if
    /// it refused) nothing at all, rather than text we cannot produce.
    ///
    /// The two are read under one lock so that a caller which acts on the mimes
    /// later, on another thread, can tell whether the owner changed underneath
    /// it; see [`offer_at`](Self::offer_at).
    pub fn convertible(&self, sel: Sel) -> (Vec<String>, u64) {
        let g = self.inner.lock().unwrap();
        let state = g.sel(sel);
        let mimes = match &state.owner {
            Owner::None => Vec::new(),
            Owner::Wayland { mimes, .. } => mimes.clone(),
            Owner::X(o) if o.data.is_some() => {
                TEXT_MIMES.iter().map(|m| (*m).to_string()).collect()
            }
            Owner::X(_) => Vec::new(),
        };
        (mimes, state.generation)
    }

    /// The offer a [`Job::Receive`](crate::bridge::clipjobs::Job) has to ask,
    /// or `None` if a Wayland app no longer owns `sel`, or a *different* one
    /// now does than the conversion was prepared for — either way the X client
    /// asked before the change and is refused.
    ///
    /// Wayland thread only: it is the offer's own thread, so nothing can
    /// destroy it between here and the `receive`.
    pub fn offer_at(&self, sel: Sel, generation: u64) -> Option<DataOffer> {
        let g = self.inner.lock().unwrap();
        let state = g.sel(sel);
        match &state.owner {
            Owner::Wayland { offer, .. } if state.generation == generation => Some(offer.clone()),
            _ => None,
        }
    }

    /// [`convertible`](Self::convertible) for a caller acting on the answer
    /// straight away, which has no use for the generation.
    pub fn mimes(&self, sel: Sel) -> Vec<String> {
        self.convertible(sel).0
    }

    /// Where a conversion of `sel` gets its bytes.
    ///
    /// An X owner's value is already here; reading it back out of the
    /// compositor would round-trip into our own source and serve the previous
    /// selection until the compositor announced the new one. A Wayland app's
    /// has to be piped out of it, which only the Wayland thread can start.
    pub fn value(&self, sel: Sel) -> Value {
        match &self.inner.lock().unwrap().sel(sel).owner {
            Owner::None => Value::None,
            Owner::X(o) => match o.data.as_deref() {
                Some(data) => Value::Here(data.to_vec()),
                None => Value::None,
            },
            Owner::Wayland { .. } => Value::FromWayland,
        }
    }

    // --- transitions: the only things that change who owns a selection ---

    /// The compositor announced a selection for `sel`: a Wayland app's, or the
    /// echo of something we published ourselves.
    pub fn wayland_announced(
        &self,
        sel: Sel,
        offer: Option<DataOffer>,
        mimes: Vec<String>,
    ) -> Effects {
        let mut fx = Effects::default();
        let mut g = self.inner.lock().unwrap();
        if g.sel_mut(sel).take_echo(sel, offer.is_some(), &mimes) {
            // Nothing changed for X: the X owner we published this for was
            // announced to watchers when it took the selection, and a second
            // notification would name a foreign owner that does not exist.
            if matches!(g.sel(sel).owner, Owner::X(_)) {
                crate::cliplog!(
                    "{sel:?}: the compositor echoed our own source; {} still owns it",
                    g.sel(sel).owner
                );
                // its value is served from the owner record, so an offer
                // pointing back at our own source is of use to nobody
                fx.destroy.extend(offer);
                return fx;
            }
            // published for an owner a foreign app displaced before the
            // compositor got to our source: whoever won the race, this is
            // still the truth about what the compositor shows now
            let prev = g.sel_mut(sel).enter(
                sel,
                "our own source, with no X owner left to hold it",
                Owner::wayland(offer, mimes),
            );
            fx.destroy.extend(prev.into_offer());
            return fx;
        }
        let at = g.next_timestamp();
        let window = if offer.is_some() { OWNER_WINDOW } else { 0 };
        let state = g.sel_mut(sel);
        state.last_changed = at;
        // whatever we had published is not what the compositor shows now, so
        // it is not ours to withdraw either
        state.published = false;
        let why = if offer.is_some() {
            "a wayland app took it"
        } else {
            "the compositor cleared it"
        };
        let prev = state.enter(sel, why, Owner::wayland(offer, mimes));
        fx.at = at;
        fx.notify = Some(Notify {
            subtype: xfixes::SelectionEvent::SET_SELECTION_OWNER,
            owner: window,
            timestamp: at,
            selection_timestamp: at,
        });
        match prev {
            Owner::X(o) => fx.cleared = Some(o),
            other => fx.destroy.extend(other.into_offer()),
        }
        fx
    }

    /// An X client took `sel` on `window` at `time`, unless X would ignore the
    /// request. Watchers are told once the fetch that follows resolves, not
    /// here: until then there is nothing to serve for it.
    pub fn x_took(
        &self,
        sel: Sel,
        window: u32,
        client: &Arc<crate::bridge::event::Client>,
        time: u32,
    ) -> Take {
        let mut g = self.inner.lock().unwrap();
        if stale_owner_time(time, server_time_ms(), g.sel(sel).last_changed) {
            return Take::Stale;
        }
        let owner = Owner::X(XOwner {
            window,
            client: Arc::downgrade(client),
            time,
            data: None,
        });
        // the previous owner's value goes with it: the new one's is unknown
        // until fetched, and serving the old one as it would be a lie
        let prev = g.sel_mut(sel).enter(sel, "an x client took it", owner);
        g.changed_at(sel, time);
        let mut fx = Effects {
            at: time,
            ..Effects::default()
        };
        match prev {
            Owner::X(o) if !o.owned_by(client) => fx.cleared = Some(o),
            // a Wayland app's offer is spent — X serves this owner's value now
            // — but its clipboard is not ours to clear on the compositor
            other => fx.destroy.extend(other.into_offer()),
        }
        Take::Took(fx)
    }

    /// An X client gave `sel` up at `time` (`SetSelectionOwner` with no
    /// window), unless X would ignore the request. What we published for the
    /// owner goes with it: nobody owns the selection now.
    pub fn x_released(&self, sel: Sel, time: u32) -> Take {
        let mut fx = Effects {
            at: time,
            ..Effects::default()
        };
        {
            let mut g = self.inner.lock().unwrap();
            if stale_owner_time(time, server_time_ms(), g.sel(sel).last_changed) {
                return Take::Stale;
            }
            // X lets any client clear any selection (`dix/selection.c` checks
            // nothing but the time), but a Wayland app's clipboard is not ours
            // to wipe, so only an X owner is actually released here
            if matches!(g.sel(sel).owner, Owner::X(_))
                && let Owner::X(o) = g.sel_mut(sel).enter(sel, "released", Owner::None)
            {
                // X tells the old owner even when it is the same client, since
                // the selection now has no owner at all
                fx.cleared = Some(o);
            }
            g.changed_at(sel, time);
        }
        if fx.cleared.is_some() {
            self.withdraw(sel);
        }
        fx.notify = Some(Notify {
            subtype: xfixes::SelectionEvent::SET_SELECTION_OWNER,
            owner: 0,
            timestamp: server_time_ms(),
            selection_timestamp: time,
        });
        Take::Took(fx)
    }

    /// The X owner that took `sel` at `time` gave us its value: it goes to
    /// both sides, and watchers hear of the acquisition at last. A value from
    /// an owner that has since been displaced is dropped, and nobody told.
    pub fn x_fetched(&self, sel: Sel, window: u32, time: u32, data: Vec<u8>) -> Option<Effects> {
        let data: Arc<[u8]> = data.into();
        {
            let mut g = self.inner.lock().unwrap();
            let Some(owner) = g.sel_mut(sel).owner.x_mut().filter(|o| o.is(window, time)) else {
                crate::cliplog!("{sel:?}: owner {window:#x} answered after losing it; dropped");
                return None;
            };
            owner.data = Some(data.clone());
        }
        self.publish(sel, Some(data));
        Some(Self::acquired(window, time))
    }

    /// The X owner that took `sel` at `time` would not give us its value:
    /// withdraws what we published for the owner before it, so that neither
    /// side keeps handing out the *previous* clipboard item for a selection
    /// somebody else now owns (which reads as "copy stopped working" and never
    /// corrects itself). Withdrawing is at least true: there is a selection
    /// and we cannot carry it. Only what we put there, though — we only ever
    /// ask for UTF8_STRING, so any X owner holding something else (an image,
    /// say) refuses, and wiping a Wayland app's own clipboard for that would
    /// lose user data that is still perfectly good.
    pub fn x_fetch_failed(&self, sel: Sel, window: u32, time: u32) -> Option<Effects> {
        let current = self
            .inner
            .lock()
            .unwrap()
            .sel(sel)
            .owner
            .x()
            .is_some_and(|o| o.is(window, time));
        if !current {
            return None;
        }
        self.withdraw(sel);
        Some(Self::acquired(window, time))
    }

    /// Forgets the X owner of `sel` if it is `client` (on `window`, if given),
    /// which is going away, and withdraws what we published for it. X reverts
    /// a dead owner's selections to `None` and tells XFixes watchers with
    /// `subtype`, leaving the selection's timestamp as it was.
    pub fn x_owner_gone(
        &self,
        sel: Sel,
        client: &Arc<crate::bridge::event::Client>,
        window: Option<u32>,
        subtype: xfixes::SelectionEvent,
    ) -> Option<Effects> {
        let last_changed = {
            let mut g = self.inner.lock().unwrap();
            let state = g.sel_mut(sel);
            let ours = state
                .owner
                .x()
                .is_some_and(|o| o.owned_by(client) && window.is_none_or(|w| w == o.window));
            if !ours {
                return None;
            }
            state.enter(sel, "its owner went away", Owner::None);
            state.last_changed
        };
        self.withdraw(sel);
        Some(Effects {
            at: last_changed,
            notify: Some(Notify {
                subtype,
                owner: 0,
                timestamp: server_time_ms(),
                selection_timestamp: last_changed,
            }),
            ..Effects::default()
        })
    }

    /// The notification watchers are owed for an X client's acquisition, which
    /// waits until we know whether there is anything to serve for it: X
    /// forwards their conversions to the owner, while we answer them from what
    /// we fetched. It carries the owner's own window and the time it used, as
    /// X would have sent at `SetSelectionOwner`.
    fn acquired(window: u32, time: u32) -> Effects {
        Effects {
            at: time,
            notify: Some(Notify {
                subtype: xfixes::SelectionEvent::SET_SELECTION_OWNER,
                owner: window,
                timestamp: server_time_ms(),
                selection_timestamp: time,
            }),
            ..Effects::default()
        }
    }

    // --- talking to the compositor ---

    /// Publishes `data` for `sel`, or withdraws our source with `None`, and
    /// records the announcement the compositor then owes us. The record goes
    /// in before the job is posted: the echo can arrive on the Wayland thread
    /// before this one gets the lock back, and would then be taken for a
    /// foreign app displacing the X owner.
    fn publish(&self, sel: Sel, data: Option<Arc<[u8]>>) {
        let echo = if data.is_some() {
            Echo::Source
        } else {
            Echo::Withdrawn
        };
        {
            let mut g = self.inner.lock().unwrap();
            let state = g.sel_mut(sel);
            if !state.publishable {
                crate::cliplog!("{sel:?}: the compositor has no such selection to publish to");
                return;
            }
            if state.echoes.len() >= MAX_ECHOES {
                state.echoes.pop_front();
            }
            state.echoes.push_back(echo);
            state.published = data.is_some();
        }
        if !self.jobs.post(Job::Publish(sel, data)) {
            // nobody is going to tell the compositor, so it owes us nothing
            let mut g = self.inner.lock().unwrap();
            let state = g.sel_mut(sel);
            if state.echoes.back() == Some(&echo) {
                state.echoes.pop_back();
            }
            state.published = false;
        }
    }

    /// Withdraws what we published for `sel`, if the compositor still shows
    /// it. Only ever ours: see [`x_fetch_failed`](Self::x_fetch_failed).
    fn withdraw(&self, sel: Sel) {
        if !self.inner.lock().unwrap().sel(sel).published {
            return;
        }
        crate::cliplog!("{sel:?}: withdrawing our Wayland source");
        self.publish(sel, None);
    }

    /// Whether `offer` is still the one the compositor announced for `sel`. A
    /// request on an offer it has replaced is dropped on the floor, which
    /// reads as an instant EOF, and that must not reach a requestor as an
    /// empty selection.
    pub fn is_current(&self, sel: Sel, offer: &DataOffer) -> bool {
        match &self.inner.lock().unwrap().sel(sel).owner {
            Owner::Wayland { offer: cur, .. } => cur.same(offer),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::bridge::event::Client;

    fn new_client() -> Arc<Client> {
        let (a, _b) = UnixStream::pair().unwrap();
        Client::new(a).unwrap()
    }

    /// The bytes an X requestor would be served for `sel`, which is `None`
    /// when there is nothing to serve and the conversion is refused.
    fn served(c: &Clipboard, sel: Sel) -> Option<Vec<u8>> {
        match c.value(sel) {
            Value::Here(data) => Some(data),
            Value::None => None,
            Value::FromWayland => panic!("a wayland app owns {sel:?}"),
        }
    }

    fn text_mimes() -> Vec<String> {
        TEXT_MIMES.iter().map(|m| (*m).to_string()).collect()
    }

    /// A clipboard with a Wayland thread to post to, which `publishable` says
    /// whether the compositor's device carries both selections (a wlr v1
    /// device has no primary selection). What it publishes is read back out of
    /// the job queue, which nothing drains here.
    fn clipboard(publishable: bool) -> Clipboard {
        let c = Clipboard::default();
        c.jobs().set_running(true);
        c.set_publishable(Sel::Clipboard, true);
        c.set_publishable(Sel::Primary, publishable);
        c
    }

    /// An announcement whose effects the test does not look at.
    fn announced(c: &Clipboard, sel: Sel, mimes: &[String]) {
        drop(announce(c, sel, mimes));
    }

    /// A Wayland app announcing `mimes` for `sel`, as the compositor would.
    fn announce(c: &Clipboard, sel: Sel, mimes: &[String]) -> Effects {
        c.wayland_announced(sel, Some(DataOffer::Fake), mimes.to_vec())
    }

    /// A transition that must have taken, whose effects the test does not
    /// look at.
    fn taken(take: Take) {
        drop(took(take));
    }

    /// The effects of a transition that must have taken.
    fn took(take: Take) -> Effects {
        match take {
            Take::Took(fx) => fx,
            Take::Stale => panic!("the transition was rejected as stale"),
        }
    }

    impl Effects {
        /// The window and time an `XFixesSelectionNotify` would carry, for the
        /// tests that only care about that much of it.
        fn notified(&self) -> Option<(u32, u32)> {
            self.notify
                .as_ref()
                .map(|n| (n.owner, n.selection_timestamp))
        }
    }

    #[test]
    fn dix_time_rules() {
        // in the future: ignored
        assert!(stale_owner_time(101, 100, 50));
        // before the last change: ignored
        assert!(stale_owner_time(49, 100, 50));
        // at the last change, or anywhere up to now: taken
        assert!(!stale_owner_time(50, 100, 50));
        assert!(!stale_owner_time(100, 100, 50));
        assert!(!stale_owner_time(75, 100, 50));
        // and across the clock wrap
        assert!(!stale_owner_time(1, 2, u32::MAX));
        assert!(stale_owner_time(u32::MAX, 2, 1));
    }

    #[test]
    fn a_stale_set_selection_owner_is_ignored() {
        let c = clipboard(true);
        let client = new_client();
        let now = server_time_ms();
        assert!(
            took(c.x_took(Sel::Clipboard, 0x10, &client, now))
                .cleared
                .is_none()
        );
        assert_eq!(c.owner_window(Sel::Clipboard), Some(0x10));
        assert_eq!(c.last_changed(Sel::Clipboard), now);
        // a request from before that change loses to it
        assert!(matches!(
            c.x_took(Sel::Clipboard, 0x11, &client, now.wrapping_sub(1)),
            Take::Stale
        ));
        assert_eq!(c.owner_window(Sel::Clipboard), Some(0x10));
        // and one from the future is not believed either
        let future = server_time_ms().wrapping_add(60_000);
        assert!(matches!(
            c.x_took(Sel::Clipboard, 0x11, &client, future),
            Take::Stale
        ));
        assert!(matches!(c.x_released(Sel::Clipboard, future), Take::Stale));
        assert_eq!(c.owner_window(Sel::Clipboard), Some(0x10));
    }

    #[test]
    fn a_wayland_change_is_stamped_after_the_x_owners_time() {
        let c = clipboard(true);
        let client = new_client();
        let t = server_time_ms();
        taken(c.x_took(Sel::Clipboard, 0x10, &client, t));
        let fx = c.wayland_announced(Sel::Clipboard, None, Vec::new());
        let ts = c.last_changed(Sel::Clipboard);
        assert!(time_cmp(ts, t).is_gt());
        assert_eq!(fx.notified(), Some((0, ts)), "and watchers are told");
        // and the clock will not hand out anything earlier, so the next
        // PropertyNotify-derived SetSelectionOwner is not stale on arrival
        assert!(time_cmp(server_time_ms(), ts).is_ge());
        taken(c.x_took(Sel::Clipboard, 0x10, &client, server_time_ms()));
    }

    #[test]
    fn a_new_owner_has_no_value_until_fetched() {
        let c = clipboard(true);
        let client = new_client();
        let t = server_time_ms();
        taken(c.x_took(Sel::Clipboard, 0x10, &client, t));
        assert!(c.mimes(Sel::Clipboard).is_empty(), "nothing to offer yet");
        assert!(
            c.x_fetched(Sel::Clipboard, 0x10, t, b"one".to_vec())
                .is_some()
        );
        assert_eq!(c.mimes(Sel::Clipboard), text_mimes());
        // taken again (a second copy), a moment later: the first value is
        // not the second's
        crate::bridge::event::note_selection_time(t.wrapping_add(1));
        let t2 = server_time_ms();
        assert!(time_cmp(t2, t).is_gt());
        taken(c.x_took(Sel::Clipboard, 0x10, &client, t2));
        assert!(
            served(&c, Sel::Clipboard).is_none(),
            "the old value would be a lie"
        );
        assert_eq!(c.owner_window(Sel::Clipboard), Some(0x10));
        // the first fetch's late answer is not the current owner's
        assert!(
            c.x_fetched(Sel::Clipboard, 0x10, t, b"one".to_vec())
                .is_none()
        );
        assert!(served(&c, Sel::Clipboard).is_none());
        let fx = c
            .x_fetched(Sel::Clipboard, 0x10, t2, b"two".to_vec())
            .expect("the current owner's answer");
        assert_eq!(
            fx.notified(),
            Some((0x10, t2)),
            "watchers hear of the acquisition, with the owner's own window"
        );
        assert_eq!(served(&c, Sel::Clipboard).as_deref(), Some(&b"two"[..]));
    }

    #[test]
    fn taking_it_again_on_another_window_owes_nothing() {
        // dix sends the SelectionClear per client, not per window
        let c = clipboard(true);
        let client = new_client();
        let other = new_client();
        taken(c.x_took(Sel::Clipboard, 0x10, &client, server_time_ms()));
        let fx = took(c.x_took(Sel::Clipboard, 0x11, &client, server_time_ms()));
        assert!(fx.cleared.is_none(), "the same client keeps its clear");
        let fx = took(c.x_took(Sel::Clipboard, 0x20, &other, server_time_ms()));
        assert_eq!(
            fx.cleared.map(|o| o.window),
            Some(0x11),
            "another client displaces it, and is owed the news"
        );
    }

    #[test]
    fn a_reply_after_a_wayland_takeover_is_dropped() {
        let c = clipboard(true);
        let client = new_client();
        let t = server_time_ms();
        taken(c.x_took(Sel::Clipboard, 0x10, &client, t));
        announced(&c, Sel::Clipboard, &["text/html".to_string()]);
        assert!(
            c.x_fetched(Sel::Clipboard, 0x10, t, b"late".to_vec())
                .is_none()
        );
        assert!(
            c.jobs().publishes().is_empty(),
            "nothing published over the app's"
        );
        assert!(
            c.x_fetch_failed(Sel::Clipboard, 0x10, t).is_none(),
            "nor withdrawn"
        );
    }

    #[test]
    fn the_echo_of_our_own_source_is_recognised_and_changes_nothing() {
        let c = clipboard(true);
        let client = new_client();
        let t = server_time_ms();
        taken(c.x_took(Sel::Clipboard, 0x10, &client, t));
        c.x_fetched(Sel::Clipboard, 0x10, t, b"hello".to_vec());
        assert_eq!(c.jobs().publishes().as_slice(), [(Sel::Clipboard, Some(5))]);
        // the announce is recognised by its fingerprint, and leaves the X
        // owner (and its value, and its time) exactly as it was
        let fx = announce(&c, Sel::Clipboard, &text_mimes());
        assert!(fx.notified().is_none() && fx.cleared.is_none());
        assert_eq!(fx.destroy.len(), 1, "the offer is of no use to us");
        assert_eq!(
            c.owner_window(Sel::Clipboard),
            Some(0x10),
            "the X owner stays"
        );
        assert_eq!(c.last_changed(Sel::Clipboard), t, "and so does its time");
        assert_eq!(
            served(&c, Sel::Clipboard).as_deref(),
            Some(&b"hello"[..]),
            "served from the owner, not read back out of the compositor"
        );
        // and only once: nothing more is owed, so a second one is a foreign
        // app that happens to advertise the same mimes
        let fx = announce(&c, Sel::Clipboard, &text_mimes());
        assert!(fx.notified().is_some() && fx.cleared.is_some());
    }

    /// An X owner that has just taken `sel`, for the tests that go on to
    /// publish for it.
    fn take(c: &Clipboard, sel: Sel, client: &Arc<Client>) -> u32 {
        let t = server_time_ms();
        taken(c.x_took(sel, 0x10, client, t));
        t
    }

    #[test]
    fn a_foreign_offer_is_not_mistaken_for_our_echo() {
        let c = clipboard(true);
        let client = new_client();
        let t = take(&c, Sel::Clipboard, &client);
        c.x_fetched(Sel::Clipboard, 0x10, t, b"ours".to_vec());
        let foreign = vec!["text/plain".to_string(), "text/html".to_string()];
        let fx = announce(&c, Sel::Clipboard, &foreign);
        assert!(
            fx.cleared.is_some(),
            "wrong mimes: a foreign app won the race at the compositor"
        );
        assert_eq!(c.owner_window(Sel::Clipboard), Some(OWNER_WINDOW));
        assert_eq!(c.mimes(Sel::Clipboard), foreign);
        // nor is an announcement of nothing at all, while a source is owed
        assert!(
            c.wayland_announced(Sel::Clipboard, None, Vec::new())
                .notified()
                .is_some()
        );
        // ours is still owed, and still recognised when it comes; with no X
        // owner left to attribute it to, it is simply what the compositor
        // shows now
        let fx = announce(&c, Sel::Clipboard, &text_mimes());
        assert!(fx.notified().is_none(), "our own source, announced late");
        assert_eq!(c.owner_window(Sel::Clipboard), Some(OWNER_WINDOW));
    }

    #[test]
    fn each_publication_expects_its_own_echo() {
        let c = clipboard(true);
        let client = new_client();
        let t = take(&c, Sel::Clipboard, &client);
        c.x_fetched(Sel::Clipboard, 0x10, t, b"one".to_vec());
        let t = take(&c, Sel::Clipboard, &client);
        c.x_fetched(Sel::Clipboard, 0x10, t, b"two".to_vec());
        assert!(
            announce(&c, Sel::Clipboard, &text_mimes())
                .notified()
                .is_none()
        );
        assert!(
            announce(&c, Sel::Clipboard, &text_mimes())
                .notified()
                .is_none()
        );
        assert!(
            announce(&c, Sel::Clipboard, &text_mimes())
                .notified()
                .is_some(),
            "a third announce is somebody else's"
        );
    }

    #[test]
    fn a_refusing_owner_gets_the_previous_value_withdrawn() {
        let c = clipboard(true);
        let client = new_client();
        let t = take(&c, Sel::Clipboard, &client);
        c.x_fetched(Sel::Clipboard, 0x10, t, b"ours".to_vec());
        announced(&c, Sel::Clipboard, &text_mimes());
        // another X client takes it and refuses to convert: what we published
        // for the first is withdrawn, so neither side serves it as the second's
        let other = new_client();
        let t2 = server_time_ms();
        taken(c.x_took(Sel::Clipboard, 0x20, &other, t2));
        assert!(served(&c, Sel::Clipboard).is_none());
        assert_eq!(
            c.x_fetch_failed(Sel::Clipboard, 0x20, t2)
                .map(|fx| fx.notified()),
            Some(Some((0x20, t2))),
            "watchers are owed the news"
        );
        assert_eq!(c.jobs().publishes().last(), Some(&(Sel::Clipboard, None)));
        // the withdrawal comes back as an announcement of nothing
        let fx = c.wayland_announced(Sel::Clipboard, None, Vec::new());
        assert!(fx.notified().is_none(), "our own withdrawal");
        assert_eq!(
            c.owner_window(Sel::Clipboard),
            Some(0x20),
            "the X owner stays"
        );
        // nothing to withdraw twice
        assert!(c.x_fetch_failed(Sel::Clipboard, 0x20, t2).is_some());
        assert_eq!(c.jobs().publishes().len(), 2);
    }

    #[test]
    fn nothing_is_owed_when_the_compositor_was_not_told() {
        // a wlr v1 device has no primary selection to publish to
        let c = clipboard(false);
        let client = new_client();
        let t = take(&c, Sel::Primary, &client);
        c.x_fetched(Sel::Primary, 0x10, t, b"ours".to_vec());
        assert!(
            announce(&c, Sel::Primary, &text_mimes())
                .notified()
                .is_some(),
            "no echo was owed, so this is somebody else's"
        );
        // and there is nothing on the compositor to withdraw either
        let t = take(&c, Sel::Primary, &client);
        assert!(
            c.x_fetch_failed(Sel::Primary, 0x10, t).is_some(),
            "the owner is still current"
        );
        assert!(
            c.jobs().publishes().is_empty(),
            "nothing was ever posted for a selection the device does not carry"
        );
    }

    #[test]
    fn a_dead_owner_is_forgotten_and_its_value_withdrawn() {
        let c = clipboard(true);
        let client = new_client();
        let other = new_client();
        let t = server_time_ms();
        let close = xfixes::SelectionEvent::SELECTION_CLIENT_CLOSE;
        taken(c.x_took(Sel::Clipboard, 0x10, &client, t));
        c.x_fetched(Sel::Clipboard, 0x10, t, b"ours".to_vec());
        assert!(
            c.x_owner_gone(Sel::Clipboard, &other, None, close)
                .is_none(),
            "not its owner"
        );
        assert!(
            c.x_owner_gone(Sel::Clipboard, &client, Some(0x11), close)
                .is_none(),
            "not its window"
        );
        let fx = c
            .x_owner_gone(Sel::Clipboard, &client, Some(0x10), close)
            .expect("its own window");
        assert_eq!(fx.notified(), Some((0, t)), "the time stays, as in X");
        assert_eq!(c.owner_window(Sel::Clipboard), None);
        assert_eq!(c.jobs().publishes().last(), Some(&(Sel::Clipboard, None)));
        assert_eq!(c.last_changed(Sel::Clipboard), t);
        assert!(
            c.x_owner_gone(Sel::Clipboard, &client, None, close)
                .is_none(),
            "already gone"
        );
    }

    #[test]
    fn releasing_withdraws_and_owes_a_clear() {
        let c = clipboard(true);
        let client = new_client();
        let t = server_time_ms();
        taken(c.x_took(Sel::Clipboard, 0x10, &client, t));
        c.x_fetched(Sel::Clipboard, 0x10, t, b"ours".to_vec());
        let now = server_time_ms();
        let fx = took(c.x_released(Sel::Clipboard, now));
        assert_eq!(fx.notified(), Some((0, now)));
        assert_eq!(
            fx.cleared.map(|o| o.window),
            Some(0x10),
            "X tells the owner even when it is the same client"
        );
        assert_eq!(c.owner_window(Sel::Clipboard), None);
        assert_eq!(c.last_changed(Sel::Clipboard), now);
        assert_eq!(c.jobs().publishes().last(), Some(&(Sel::Clipboard, None)));
        // releasing an unowned selection is a change too, but withdraws nothing
        let fx = took(c.x_released(Sel::Clipboard, now));
        assert!(fx.cleared.is_none());
        assert_eq!(c.jobs().publishes().len(), 2);
    }

    #[test]
    fn a_wayland_apps_clipboard_is_not_ours_to_clear() {
        // any X client may release any selection (dix checks nothing but the
        // time), but that must not wipe what a Wayland app is offering
        let c = clipboard(true);
        let mimes = vec!["text/plain".to_string()];
        announced(&c, Sel::Clipboard, &mimes);
        let fx = took(c.x_released(Sel::Clipboard, server_time_ms()));
        assert!(fx.cleared.is_none());
        assert_eq!(c.owner_window(Sel::Clipboard), Some(OWNER_WINDOW));
        assert_eq!(c.mimes(Sel::Clipboard), mimes);
        assert!(c.jobs().publishes().is_empty(), "nothing withdrawn");
    }

    #[test]
    fn a_wayland_takeover_drops_the_x_owner_and_its_data() {
        let c = clipboard(true);
        let client = new_client();
        let t = take(&c, Sel::Clipboard, &client);
        c.x_fetched(Sel::Clipboard, 0x10, t, b"ours".to_vec());
        let fx = announce(&c, Sel::Clipboard, &["text/html".to_string()]);
        assert_eq!(fx.notified().map(|(w, _)| w), Some(OWNER_WINDOW));
        let prev = fx.cleared.expect("there was an owner");
        assert_eq!(prev.window, 0x10);
        assert!(prev.owned_by(&client));
        assert!(
            matches!(c.value(Sel::Clipboard), Value::FromWayland),
            "the app's value, not the X owner's"
        );
    }

    #[test]
    fn a_conversion_prepared_for_one_owner_is_refused_after_a_takeover() {
        // the mime is resolved against the owner's list on an X thread and the
        // value is read on the Wayland thread a pass later. Asking a new owner
        // for a mime only the old one offered is denied by the compositor as an
        // instant EOF, which would reach the requestor as an empty paste that
        // looks successful, so the generation has to make them disagree.
        let c = clipboard(true);
        let _ = announce(&c, Sel::Clipboard, &["text/html".to_string()]);
        let (mimes, generation) = c.convertible(Sel::Clipboard);
        assert_eq!(mimes, ["text/html"]);
        assert!(
            c.offer_at(Sel::Clipboard, generation).is_some(),
            "the owner the conversion was prepared for"
        );

        // another app takes it before the job runs
        let _ = announce(&c, Sel::Clipboard, &["text/plain".to_string()]);
        assert!(
            c.offer_at(Sel::Clipboard, generation).is_none(),
            "a different owner must not answer the old one's conversion"
        );
        let (_, after) = c.convertible(Sel::Clipboard);
        assert_ne!(after, generation, "the generation moved with the owner");
        assert!(c.offer_at(Sel::Clipboard, after).is_some());
    }

    #[test]
    fn an_x_owner_displaces_a_wayland_apps_offer() {
        let c = clipboard(true);
        let client = new_client();
        announced(&c, Sel::Clipboard, &["text/html".to_string()]);
        let fx = took(c.x_took(Sel::Clipboard, 0x10, &client, server_time_ms()));
        assert!(fx.cleared.is_none(), "no X client lost anything");
        assert_eq!(fx.destroy.len(), 1, "the offer it replaced is spent");
        assert_eq!(c.owner_window(Sel::Clipboard), Some(0x10));
    }
}
