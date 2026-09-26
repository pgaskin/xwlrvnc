use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::bridge::clipboard::{MAX_SELECTION_BYTES, Sel, stale_owner_time};
use crate::bridge::event::server_time_ms;
use crate::bridge::x11::SELECTION_FETCH_WINDOW;

/// How long an X owner has to answer a `ConvertSelection` of ours, and how
/// long a chunked transfer may go without a chunk, before the fetch is given
/// up on as though the owner had refused. Xt gives an owner 5 s; this is
/// looser, since giving up on one that would still have answered costs its
/// value, while waiting on one that never will costs only a clipboard that
/// stays stale this long. Nothing waits on it: the connection thread sleeps
/// no longer than this while a fetch is out (see `Connection::run`).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How many unanswered `ConvertSelection`s to remember. Two selections times a
/// couple of rapid re-takes is the realistic worst case; anything near this
/// is a client taking a selection over and over without ever answering for
/// it, and the oldest is the one least likely to still be coming.
const MAX_FETCHES: usize = 32;

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
    /// each fetch gets a property of its own from a ring of them; keying by
    /// selection alone would let the first answer be consumed as though it
    /// were the second's, publishing the older copy and dropping the newer.
    /// The ring is wide enough that a property is not reused inside
    /// [`FETCH_TIMEOUT`], by which time the fetch that had it is resolved, one
    /// way or the other: every fetch begun here is answered, refused, timed
    /// out or displaced, and never merely forgotten.
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
    /// When it was asked, from which the owner has [`FETCH_TIMEOUT`] to answer.
    pub since: Instant,
}

/// An in-flight chunked receive.
struct IncrRecv {
    fetch: Fetch,
    data: Vec<u8>,
    /// When the last chunk came (or the transfer began): a chunked transfer
    /// has [`FETCH_TIMEOUT`] between chunks rather than in all, since a large
    /// value from a slow owner is still coming as long as chunks are.
    progress_at: Instant,
}

pub enum IncrStep {
    More(u32),
    Done(Fetch, Vec<u8>),
    /// The value went past [`MAX_SELECTION_BYTES`]: the transfer is abandoned
    /// where it is, and the fetch is resolved as unanswered.
    TooBig(Fetch),
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

    /// Records a `ConvertSelection` we just issued to an X owner, handing
    /// back the fetches it displaces: one still out on the same property (the
    /// ring has come back around to it), a chunked receive on that property,
    /// and the oldest past [`MAX_FETCHES`]. Each of those is a request that
    /// was never answered, and the caller resolves it as such. A fetch
    /// forgotten silently is an acquisition XFixes watchers never hear of and
    /// a previous owner's value served for it indefinitely, on both sides.
    #[must_use = "the fetches displaced have to be resolved"]
    pub fn begin_fetch(&mut self, fetch: Fetch) -> Vec<Fetch> {
        let property = fetch.property;
        let mut displaced: Vec<Fetch> = self
            .incr
            .remove(&property)
            .map(|i| i.fetch)
            .into_iter()
            .collect();
        displaced.extend(self.fetches.extract_if(.., |f| f.property == property));
        while self.fetches.len() >= MAX_FETCHES {
            displaced.push(self.fetches.remove(0));
        }
        self.fetches.push(fetch);
        displaced
    }

    /// Takes out every fetch whose owner has had [`FETCH_TIMEOUT`] to answer
    /// and has not, and every chunked receive that has gone that long without
    /// a chunk, for the caller to resolve as unanswered.
    pub fn expired(&mut self, now: Instant) -> Vec<Fetch> {
        let timed_out = |since: Instant| now.duration_since(since) >= FETCH_TIMEOUT;
        let mut out: Vec<Fetch> = self
            .fetches
            .extract_if(.., |f| timed_out(f.since))
            .collect();
        out.extend(
            self.incr
                .extract_if(|_, i| timed_out(i.progress_at))
                .map(|(_, i)| i.fetch),
        );
        out
    }

    /// When the fetch or chunked receive with the least time left runs out of
    /// it, if any is out at all.
    pub fn next_deadline(&self) -> Option<Instant> {
        let fetches = self.fetches.iter().map(|f| f.since);
        let incrs = self.incr.values().map(|i| i.progress_at);
        fetches.chain(incrs).min().map(|t| t + FETCH_TIMEOUT)
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
                progress_at: Instant::now(),
            },
        );
    }

    /// Whether a `ChangeProperty` is an INCR chunk for a receive in progress.
    pub fn incr_chunk(&self, window: u32, property: u32) -> bool {
        window == SELECTION_FETCH_WINDOW && self.incr.contains_key(&property)
    }

    /// Handles one INCR chunk. An empty chunk completes the transfer (returning
    /// the assembled value), and a non-empty chunk is accumulated — up to
    /// [`MAX_SELECTION_BYTES`], the same cap the other direction has, past
    /// which the transfer is abandoned rather than the owner allowed to grow us
    /// without limit. Check [`incr_chunk`](Self::incr_chunk) first.
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
            if incr.data.len().saturating_add(chunk.len()) > MAX_SELECTION_BYTES {
                let incr = self.incr.remove(&property).expect("checked just above");
                return IncrStep::TooBig(incr.fetch);
            }
            incr.data.extend_from_slice(&chunk);
            incr.progress_at = Instant::now();
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
            since: Instant::now(),
        }
    }

    /// Begins a fetch that displaces nothing.
    fn begin(s: &mut Selection, f: Fetch) {
        assert!(s.begin_fetch(f).is_empty(), "nothing was displaced");
    }

    /// A moment after every fetch begun so far has run out of time.
    fn later() -> Instant {
        Instant::now() + FETCH_TIMEOUT + Duration::from_secs(1)
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
        begin(&mut s, fetch(Sel::Clipboard, CLIP, P1));
        begin(&mut s, fetch(Sel::Clipboard, CLIP, P2));
        assert_eq!(answered(&mut s, CLIP, P1), Some((Sel::Clipboard, P1)));
        assert_eq!(answered(&mut s, CLIP, P2), Some((Sel::Clipboard, P2)));
    }

    #[test]
    fn two_selections_in_flight_do_not_collide() {
        let mut s = Selection::default();
        begin(&mut s, fetch(Sel::Primary, PRI, P1));
        begin(&mut s, fetch(Sel::Clipboard, CLIP, P2));
        assert_eq!(answered(&mut s, CLIP, P2), Some((Sel::Clipboard, P2)));
        assert_eq!(answered(&mut s, PRI, P1), Some((Sel::Primary, P1)));
    }

    #[test]
    fn a_refusal_matches_the_oldest_request_for_that_selection() {
        // a refusal names no property, and answers come back in order
        let mut s = Selection::default();
        begin(&mut s, fetch(Sel::Clipboard, CLIP, P1));
        begin(&mut s, fetch(Sel::Clipboard, CLIP, P2));
        assert_eq!(answered(&mut s, CLIP, 0), Some((Sel::Clipboard, P1)));
        assert_eq!(answered(&mut s, CLIP, P2), Some((Sel::Clipboard, P2)));
    }

    #[test]
    fn an_answer_to_nothing_is_ignored() {
        let mut s = Selection::default();
        begin(&mut s, fetch(Sel::Clipboard, CLIP, P1));
        assert_eq!(answered(&mut s, CLIP, 999), None, "not our property");
        assert_eq!(answered(&mut s, PRI, P1), None, "not our selection");
        assert_eq!(answered(&mut s, CLIP, P1), Some((Sel::Clipboard, P1)));
    }

    #[test]
    fn a_fetch_whose_property_comes_round_again_is_handed_back() {
        // the property ring has wrapped to a fetch still out: it is not
        // forgotten, it is given back to be resolved, and the answer that
        // names the property from now on is the new fetch's
        let mut s = Selection::default();
        begin(&mut s, fetch(Sel::Clipboard, CLIP, P1));
        let displaced = s.begin_fetch(fetch(Sel::Primary, PRI, P1));
        assert_eq!(
            displaced.iter().map(|f| f.kind).collect::<Vec<_>>(),
            [Sel::Clipboard]
        );
        assert_eq!(answered(&mut s, CLIP, P1), None, "gone");
        assert_eq!(answered(&mut s, PRI, P1), Some((Sel::Primary, P1)));
        // a chunked receive on it likewise
        s.begin_incr(fetch(Sel::Clipboard, CLIP, P2));
        let displaced = s.begin_fetch(fetch(Sel::Primary, PRI, P2));
        assert_eq!(displaced.len(), 1);
        assert!(!s.incr_chunk(SELECTION_FETCH_WINDOW, P2));
    }

    #[test]
    fn owners_that_never_answer_do_not_pile_up() {
        // past the limit the oldest is displaced — handed back, not dropped
        let mut s = Selection::default();
        for i in 0..MAX_FETCHES as u32 {
            begin(&mut s, fetch(Sel::Clipboard, CLIP, 200 + i));
        }
        let displaced = s.begin_fetch(fetch(Sel::Clipboard, CLIP, 200 + MAX_FETCHES as u32));
        assert_eq!(
            displaced.iter().map(|f| f.property).collect::<Vec<_>>(),
            [200],
            "the oldest"
        );
        assert_eq!(s.fetches.len(), MAX_FETCHES);
        assert_eq!(answered(&mut s, CLIP, 200), None);
        // the newest is still answerable
        let newest = 200 + MAX_FETCHES as u32;
        assert_eq!(
            answered(&mut s, CLIP, newest),
            Some((Sel::Clipboard, newest))
        );
    }

    #[test]
    fn an_owner_that_never_answers_runs_out_of_time() {
        // with no deadline, a silent owner's acquisition would never be
        // announced and the previous owner's value never withdrawn
        let mut s = Selection::default();
        assert_eq!(s.next_deadline(), None, "nothing to wait for");
        let f = fetch(Sel::Clipboard, CLIP, P1);
        begin(&mut s, f);
        assert_eq!(s.next_deadline(), Some(f.since + FETCH_TIMEOUT));
        assert!(s.expired(Instant::now()).is_empty(), "not yet");
        assert_eq!(s.expired(later()), [f]);
        assert_eq!(answered(&mut s, CLIP, P1), None, "resolved, so gone");
        assert_eq!(s.next_deadline(), None);
    }

    #[test]
    fn a_chunked_receive_that_stalls_runs_out_of_time() {
        let mut s = Selection::default();
        let f = fetch(Sel::Clipboard, CLIP, P1);
        begin(&mut s, f);
        assert_eq!(answered(&mut s, CLIP, P1), Some((Sel::Clipboard, P1)));
        s.begin_incr(f);
        assert!(
            s.next_deadline().is_some(),
            "the transfer has a deadline too"
        );
        assert!(matches!(
            s.incr_push(P1, b"chunk".to_vec()),
            IncrStep::More(P1)
        ));
        assert!(s.expired(Instant::now()).is_empty());
        assert_eq!(s.expired(later()).len(), 1, "the owner stopped sending");
        assert!(!s.incr_chunk(SELECTION_FETCH_WINDOW, P1), "abandoned");
    }

    #[test]
    fn a_chunked_receive_is_capped() {
        let mut s = Selection::default();
        s.begin_incr(fetch(Sel::Clipboard, CLIP, P1));
        assert!(matches!(
            s.incr_push(P1, vec![0; MAX_SELECTION_BYTES]),
            IncrStep::More(P1)
        ));
        match s.incr_push(P1, b"x".to_vec()) {
            IncrStep::TooBig(f) => assert_eq!(f.property, P1),
            _ => panic!("one byte over the cap was accepted"),
        }
        assert!(!s.incr_chunk(SELECTION_FETCH_WINDOW, P1), "abandoned");
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
