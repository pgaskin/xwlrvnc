use std::collections::HashMap;

use crate::bridge::clipboard::{Sel, stale_owner_time};
use crate::bridge::event::server_time_ms;
use crate::bridge::x11::SELECTION_FETCH_WINDOW;

/// How many unanswered `ConvertSelection`s to remember. Two selections times a
/// couple of rapid re-takes is the realistic worst case.
const MAX_FETCHES: usize = 8;

/// Per-connection clipboard/selection state machine.
#[derive(Default)]
pub struct Selection {
    /// Owner per selection atom, for the selections we don't bridge to
    /// Wayland; the bridged ones live in
    /// [`Clipboard`](crate::bridge::clipboard::Clipboard). Deliberately not
    /// shared between connections, even though the atoms naming them now are:
    /// a `ConvertSelection` is never forwarded to the owning client, so an
    /// owner another connection can see is an owner nobody can get a value
    /// from.
    owners: HashMap<u32, Owner>,
    /// ConvertSelections we have issued to X owners, oldest first. RealVNC
    /// takes PRIMARY and CLIPBOARD in the same breath, and a selection can be
    /// taken again before the first answer arrives, so several are in flight at
    /// once. Each is matched by the property it was told to use, which is why
    /// every fetch gets its own; keying by selection alone would let the first
    /// answer be consumed as though it were the second's, publishing the older
    /// copy and dropping the newer.
    fetches: Vec<Fetch>,
    /// Chunked receives in progress, keyed by the property they arrive on.
    /// Each fetch uses its own property, so two can run at once.
    incr: HashMap<u32, IncrRecv>,
}

/// The owner of a selection kept on this connection alone: the window, or
/// none, and when that was last set (dix's `lastTimeChanged`).
struct Owner {
    window: u32,
    last_changed: u32,
}

/// An in-flight ConvertSelection to an X selection owner to receive it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fetch {
    pub kind: Sel,
    pub selection: u32,
    pub property: u32,
    /// The owner asked, and when it took the selection: a reply is only
    /// good if that acquisition is still the current one.
    pub owner: u32,
    pub time: u32,
}

/// An in-flight chunked receive.
struct IncrRecv {
    fetch: Fetch,
    data: Vec<u8>,
}

pub enum IncrStep {
    More(u32),
    Done(Fetch, Vec<u8>),
}

impl Selection {
    /// Sets (or with `owner` 0 clears) the owner of `selection` at `time`,
    /// unless X would ignore the request as stale. Returns whether it took.
    pub fn set_owner(&mut self, selection: u32, owner: u32, time: u32) -> bool {
        let last = self.owners.get(&selection).map_or(0, |o| o.last_changed);
        if stale_owner_time(time, server_time_ms(), last) {
            return false;
        }
        self.owners.insert(
            selection,
            Owner {
                window: owner,
                last_changed: time,
            },
        );
        true
    }

    pub fn owner(&self, selection: u32) -> Option<u32> {
        self.owners
            .get(&selection)
            .map(|o| o.window)
            .filter(|&w| w != 0)
    }

    /// Reverts every selection `window` owned to no owner, as X does when the
    /// owner window is destroyed, returning their atoms and last change (which
    /// X leaves as it was).
    pub fn forget_window(&mut self, window: u32) -> Vec<(u32, u32)> {
        let mut gone = Vec::new();
        for (&atom, o) in &mut self.owners {
            if o.window == window {
                o.window = 0;
                gone.push((atom, o.last_changed));
            }
        }
        gone
    }

    /// Records a `ConvertSelection` we just issued to an X owner, dropping any
    /// half-finished chunked receive left on the same property.
    pub fn begin_fetch(&mut self, fetch: Fetch) {
        let property = fetch.property;
        self.incr.remove(&property);
        self.fetches.retain(|f| f.property != property);
        // an owner that never answers must not pile up state; the oldest is the
        // one least likely to still be coming
        if self.fetches.len() >= MAX_FETCHES {
            self.fetches.remove(0);
        }
        self.fetches.push(fetch);
    }

    /// Takes the fetch this `SelectionNotify` answers.
    ///
    /// An answer names the property it was asked to use, so it matches its own
    /// request even when a later one is already outstanding. A refusal names no
    /// property, so it is matched to the oldest request for that selection,
    /// answers arriving in the order they were asked for.
    pub fn take_fetch(&mut self, selection: u32, property: u32) -> Option<Fetch> {
        let at = self
            .fetches
            .iter()
            .position(|f| f.selection == selection && (property == 0 || f.property == property))?;
        Some(self.fetches.remove(at))
    }

    /// Starts an INCR receive for a large fetched value.
    pub fn begin_incr(&mut self, fetch: Fetch) {
        self.incr.insert(
            fetch.property,
            IncrRecv {
                fetch,
                data: Vec::new(),
            },
        );
    }

    /// Whether a `ChangeProperty` is an INCR chunk for a receive in progress.
    pub fn incr_chunk(&self, window: u32, property: u32) -> bool {
        window == SELECTION_FETCH_WINDOW && self.incr.contains_key(&property)
    }

    /// Handles one INCR chunk. An empty chunk completes the transfer (returning
    /// the assembled value), and a non-empty chunk is accumulated. Check
    /// [`incr_chunk`](Self::incr_chunk) first.
    pub fn incr_push(&mut self, property: u32, chunk: Vec<u8>) -> IncrStep {
        if chunk.is_empty() {
            let incr = self
                .incr
                .remove(&property)
                .expect("incr_push without an active INCR receive");
            IncrStep::Done(incr.fetch, incr.data)
        } else {
            let incr = self
                .incr
                .get_mut(&property)
                .expect("incr_push without an active INCR receive");
            incr.data.extend_from_slice(&chunk);
            IncrStep::More(property)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // any distinct atom numbers will do here
    const CLIP: u32 = 83;
    const PRI: u32 = 1;
    const P1: u32 = 100;
    const P2: u32 = 101;

    fn fetch(kind: Sel, selection: u32, property: u32) -> Fetch {
        Fetch {
            kind,
            selection,
            property,
            owner: 0x10,
            time: 1,
        }
    }

    fn answered(s: &mut Selection, selection: u32, property: u32) -> Option<(Sel, u32)> {
        s.take_fetch(selection, property)
            .map(|f| (f.kind, f.property))
    }

    #[test]
    fn an_answer_is_matched_to_its_own_request() {
        // the same selection taken twice before either answer arrives: the
        // first answer must be the first copy, not be consumed as the second's
        let mut s = Selection::default();
        s.begin_fetch(fetch(Sel::Clipboard, CLIP, P1));
        s.begin_fetch(fetch(Sel::Clipboard, CLIP, P2));
        assert_eq!(answered(&mut s, CLIP, P1), Some((Sel::Clipboard, P1)));
        assert_eq!(answered(&mut s, CLIP, P2), Some((Sel::Clipboard, P2)));
    }

    #[test]
    fn two_selections_in_flight_do_not_collide() {
        let mut s = Selection::default();
        s.begin_fetch(fetch(Sel::Primary, PRI, P1));
        s.begin_fetch(fetch(Sel::Clipboard, CLIP, P2));
        assert_eq!(answered(&mut s, CLIP, P2), Some((Sel::Clipboard, P2)));
        assert_eq!(answered(&mut s, PRI, P1), Some((Sel::Primary, P1)));
    }

    #[test]
    fn a_refusal_matches_the_oldest_request_for_that_selection() {
        // a refusal names no property, and answers come back in order
        let mut s = Selection::default();
        s.begin_fetch(fetch(Sel::Clipboard, CLIP, P1));
        s.begin_fetch(fetch(Sel::Clipboard, CLIP, P2));
        assert_eq!(answered(&mut s, CLIP, 0), Some((Sel::Clipboard, P1)));
        assert_eq!(answered(&mut s, CLIP, P2), Some((Sel::Clipboard, P2)));
    }

    #[test]
    fn an_answer_to_nothing_is_ignored() {
        let mut s = Selection::default();
        s.begin_fetch(fetch(Sel::Clipboard, CLIP, P1));
        assert_eq!(answered(&mut s, CLIP, 999), None, "not our property");
        assert_eq!(answered(&mut s, PRI, P1), None, "not our selection");
        assert_eq!(answered(&mut s, CLIP, P1), Some((Sel::Clipboard, P1)));
    }

    #[test]
    fn owners_that_never_answer_do_not_pile_up() {
        let mut s = Selection::default();
        for i in 0..(MAX_FETCHES as u32 * 2) {
            s.begin_fetch(fetch(Sel::Clipboard, CLIP, 200 + i));
        }
        assert_eq!(s.fetches.len(), MAX_FETCHES);
        // the newest is still answerable
        let newest = 200 + (MAX_FETCHES as u32 * 2 - 1);
        assert_eq!(
            answered(&mut s, CLIP, newest),
            Some((Sel::Clipboard, newest))
        );
    }

    #[test]
    fn unbridged_owners_follow_the_dix_time_rules() {
        let mut s = Selection::default();
        let now = server_time_ms();
        assert!(s.set_owner(200, 0x10, now));
        assert_eq!(s.owner(200), Some(0x10));
        // earlier than the last change: ignored
        assert!(!s.set_owner(200, 0x11, now.wrapping_sub(1)));
        assert_eq!(s.owner(200), Some(0x10));
        // the future: ignored
        assert!(!s.set_owner(200, 0x11, server_time_ms().wrapping_add(60_000)));
        // same time: taken, and 0 releases
        assert!(s.set_owner(200, 0, now));
        assert_eq!(s.owner(200), None);
    }

    #[test]
    fn a_destroyed_window_loses_its_unbridged_selections() {
        let mut s = Selection::default();
        let now = server_time_ms();
        s.set_owner(200, 0x10, now);
        s.set_owner(201, 0x11, now);
        assert_eq!(s.forget_window(0x10), [(200, now)]);
        assert_eq!(s.owner(200), None);
        assert_eq!(s.owner(201), Some(0x11));
    }

    #[test]
    fn chunked_receives_are_kept_apart_by_property() {
        let mut s = Selection::default();
        s.begin_incr(fetch(Sel::Clipboard, CLIP, P1));
        s.begin_incr(fetch(Sel::Primary, PRI, P2));
        assert!(s.incr_chunk(SELECTION_FETCH_WINDOW, P1));
        assert!(
            !s.incr_chunk(SELECTION_FETCH_WINDOW, 999),
            "unknown property"
        );
        assert!(
            !s.incr_chunk(SELECTION_FETCH_WINDOW + 1, P1),
            "wrong window"
        );
        assert!(matches!(
            s.incr_push(P1, b"clip".to_vec()),
            IncrStep::More(P1)
        ));
        assert!(matches!(
            s.incr_push(P2, b"pri".to_vec()),
            IncrStep::More(P2)
        ));
        // an empty chunk ends that transfer, and only that one
        match s.incr_push(P1, Vec::new()) {
            IncrStep::Done(f, data) => {
                assert_eq!(f.kind, Sel::Clipboard);
                assert_eq!(data, b"clip");
            }
            _ => panic!("clipboard transfer did not finish"),
        }
        assert!(!s.incr_chunk(SELECTION_FETCH_WINDOW, P1));
        assert!(
            s.incr_chunk(SELECTION_FETCH_WINDOW, P2),
            "primary still going"
        );
    }
}
