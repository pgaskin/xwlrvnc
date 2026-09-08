use std::collections::HashMap;

use crate::bridge::{clipboard::Sel, x11::SELECTION_FETCH_WINDOW};

/// Per-connection clipboard/selection state machine.
#[derive(Default)]
pub struct Selection {
    /// Owner window per selection atom, for the selections we don't bridge to
    /// Wayland; the bridged ones live in
    /// [`Clipboard`](crate::bridge::clipboard::Clipboard). These can't be shared
    /// since atoms are interned per connection.
    owners: HashMap<u32, u32>,
    pending_fetch: Option<Fetch>,
    incr_recv: Option<IncrRecv>,
}

/// An in-flight ConvertSelection to an X selection owner to receive it.
struct Fetch {
    kind: Sel,
    property: u32,
}

/// An in-flight chunked receive.
struct IncrRecv {
    kind: Sel,
    property: u32,
    data: Vec<u8>,
}

pub enum IncrStep {
    More(u32),
    Done(Sel, Vec<u8>),
}

impl Selection {
    pub fn set_owner(&mut self, selection: u32, owner: u32) {
        self.owners.insert(selection, owner);
    }

    pub fn clear_owner(&mut self, selection: u32) {
        self.owners.remove(&selection);
    }

    pub fn owner(&self, selection: u32) -> Option<u32> {
        self.owners.get(&selection).copied()
    }

    /// Records a `ConvertSelection` we just issued to an X owner, cancelling any
    /// stale INCR receive.
    pub fn begin_fetch(&mut self, kind: Sel, property: u32) {
        self.pending_fetch = Some(Fetch { kind, property });
        self.incr_recv = None;
    }

    /// Takes the pending fetch's `(kind, property)`, if any.
    pub fn take_fetch(&mut self) -> Option<(Sel, u32)> {
        self.pending_fetch.take().map(|f| (f.kind, f.property))
    }

    /// Starts an INCR receive for a large fetched value.
    pub fn begin_incr(&mut self, kind: Sel, property: u32) {
        self.incr_recv = Some(IncrRecv {
            kind,
            property,
            data: Vec::new(),
        });
    }

    /// Whether a `ChangeProperty` is an INCR chunk for the in-flight receive.
    pub fn incr_chunk(&self, window: u32, property: u32) -> bool {
        self.incr_recv
            .as_ref()
            .is_some_and(|i| window == SELECTION_FETCH_WINDOW && property == i.property)
    }

    /// Handles one INCR chunk. An empty chunk completes the transfer (returning
    /// the assembled value), and a non-empty chunk is accumulated. Check
    /// [`incr_chunk`](Self::incr_chunk) first.
    pub fn incr_push(&mut self, chunk: Vec<u8>) -> IncrStep {
        if chunk.is_empty() {
            let incr = self
                .incr_recv
                .take()
                .expect("incr_push without an active INCR receive");
            IncrStep::Done(incr.kind, incr.data)
        } else {
            let incr = self
                .incr_recv
                .as_mut()
                .expect("incr_push without an active INCR receive");
            incr.data.extend_from_slice(&chunk);
            IncrStep::More(incr.property)
        }
    }
}
