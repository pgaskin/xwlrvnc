//! `ext-transient-seat-v1` client, for `-seat transient`.
//!
//! Asks the compositor for a seat of our own and puts the virtual input
//! devices on it, so remote input has its own focus, cursor and keyboard
//! state instead of sharing the local seat's. The compositor creates the seat
//! as a fresh `wl_seat` global and names it in `ready`; the global and the
//! event can land in either order, so both paths call
//! [`State::adopt_transient_seat`] and whichever comes second wins.
//!
//! Only input moves to the new seat. Selections are per seat, so the clipboard
//! stays on the first real seat (falling back to ours if the compositor has
//! none), which is what the apps on screen are using.
//!
//! Without the protocol, or if the compositor denies the seat, this falls back
//! to the first seat with a warning rather than leaving input dead.

use wayland_protocols::ext::transient_seat::v1::client::ext_transient_seat_manager_v1::ExtTransientSeatManagerV1;
use wayland_protocols::ext::transient_seat::v1::client::ext_transient_seat_v1::{
    self, ExtTransientSeatV1,
};

use super::*;

pub(super) struct TransientSeat {
    mgr: ExtTransientSeatManagerV1,
    seat: ExtTransientSeatV1,
    /// The registry name of the `wl_seat` the compositor created, from `ready`.
    global: Option<u32>,
}

impl State {
    /// Binds the manager and asks for the seat straight away.
    pub(super) fn bind_transient_seat_manager(
        &mut self,
        mgr: ExtTransientSeatManagerV1,
        qh: &QueueHandle<Self>,
    ) {
        let seat = mgr.create(qh, ());
        self.transient = Some(TransientSeat {
            mgr,
            seat,
            global: None,
        });
    }

    /// The initial globals have all arrived: with `-seat transient` and no
    /// manager among them, fall back now rather than wait forever.
    pub(super) fn transient_seat_settled(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        if matches!(self.seat_mode, SeatMode::Transient) && self.transient.is_none() {
            self.transient_fallback(
                "the compositor has no ext_transient_seat_manager_v1",
                conn,
                qh,
            );
        }
    }

    /// The registry name of our seat's `wl_seat`, once the compositor said.
    pub(super) fn transient_global(&self) -> Option<u32> {
        self.transient.as_ref()?.global
    }

    /// Gives up on the transient seat and behaves as if `-seat` was not given.
    fn transient_fallback(&mut self, why: &str, conn: &Connection, qh: &QueueHandle<Self>) {
        crate::warning!("cannot create a transient seat: {why}; using the first seat instead");
        self.seat_mode = SeatMode::First;
        if let Some(t) = self.transient.take() {
            t.seat.destroy();
            t.mgr.destroy();
        }
        if self.seat.is_none()
            && let Some(&name) = self.pending_seats.keys().min()
            && let Some(seat) = self.pending_seats.remove(&name)
        {
            self.select_seat(name, seat, conn, qh);
        }
    }

    /// A `wl_seat` global was announced while in transient mode (it is already
    /// in `pending_seats`): adopt it if it is ours.
    pub(super) fn transient_seat_announced(
        &mut self,
        name: u32,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if self.transient_global() == Some(name) {
            self.adopt_transient_seat(name, conn, qh);
        }
    }

    /// `ready`: the seat exists. Pick the clipboard seat from the real ones
    /// first, since selecting ours would otherwise claim it, then adopt ours if
    /// its global has already arrived.
    fn transient_ready(&mut self, global: u32, conn: &Connection, qh: &QueueHandle<Self>) {
        let Some(t) = &mut self.transient else { return };
        t.global = Some(global);
        crate::log!("using a transient wayland seat for input (wl_seat global {global})");
        if self.clipboard_seat.is_none()
            && let Some(&real) = self.pending_seats.keys().filter(|&&n| n != global).min()
        {
            self.clipboard_seat = self.pending_seats.get(&real).cloned();
        }
        self.adopt_transient_seat(global, conn, qh);
    }

    /// Selects our seat for input, once both its global and `ready` are in.
    fn adopt_transient_seat(&mut self, global: u32, conn: &Connection, qh: &QueueHandle<Self>) {
        if let Some(seat) = self.pending_seats.remove(&global) {
            self.select_seat(global, seat, conn, qh);
        }
    }

    /// The compositor removed our seat's global: the transient seat object is
    /// inert now, and so are the virtual devices on it.
    pub(super) fn transient_seat_removed(&mut self) {
        crate::warning!("the compositor removed the transient seat; input is disabled");
    }
}

impl Dispatch<ExtTransientSeatV1, ()> for State {
    fn event(
        state: &mut Self,
        _seat: &ExtTransientSeatV1,
        event: ext_transient_seat_v1::Event,
        _: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_transient_seat_v1::Event::Ready { global_name } => {
                state.transient_ready(global_name, conn, qh);
            }
            ext_transient_seat_v1::Event::Denied => {
                state.transient_fallback("the compositor denied it", conn, qh);
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore ExtTransientSeatManagerV1);
