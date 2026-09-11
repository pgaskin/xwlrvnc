//! Wayland client: mirrors the compositor's outputs into our RandR model, feeds
//! its frames to capture, and bridges the clipboard over ext-data-control-v1 or
//! the wlr fallback.
//!
//! Runs its own event-queue thread. Output changes update the shared
//! [`Screen`](crate::bridge::x11::randr::Screen), clipboard offers update the
//! shared [`Clipboard`](crate::bridge::clipboard::Clipboard), and both notify
//! the X clients that asked.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use wayland_client::backend::ObjectId;
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::bridge::Server;
use crate::bridge::clipboard::{self, DataOffer, Sel};
use crate::config::SeatChoice;
use crate::util::Transform;

mod capture;
mod data_control;
mod input;
mod output;
mod output_config;
mod output_power;
mod registry;
mod shm;
mod transient_seat;
use capture::Capture;
use data_control::{DataDevice, PendingReceive, PendingSend};
use input::{KeyboardBackend, PointerBackend};
use output::OutputAcc;
use output_config::OutputConfig;
use output_power::OutputPower;
use shm::{ShmBuffer, blit_channels, channel_map, create_shm_buffer, pixel_layout};
use transient_seat::TransientSeat;

/// Connects and synchronously reads the initial output geometry, so the X server
/// reports the real size before the wrapped binary starts, then dispatches on a
/// background thread. With no compositor reachable we log and keep the default
/// geometry.
pub fn spawn(server: Arc<Server>) {
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(e) => {
            crate::warning!("no wayland compositor ({e}); using default geometry");
            return;
        }
    };

    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let seat_mode = match server.config.seat_choice() {
        SeatChoice::First => SeatMode::First,
        SeatChoice::Named(n) => SeatMode::Named(n.to_string()),
        SeatChoice::Transient => SeatMode::Transient,
    };
    let mut state = State {
        server,
        outputs: HashMap::new(),
        xdg_output_mgr: None,
        seat_mode,
        seat: None,
        clipboard_seat: None,
        transient: None,
        has_transient_mgr: false,
        pending_seats: HashMap::new(),
        seat_names: HashMap::new(),
        seat_caps: HashMap::new(),
        keyboard: None,
        pointer: None,
        ext_manager: None,
        wlr_manager: None,
        device: None,
        publish: None,
        offer_mimes: HashMap::new(),
        pending_sends: Vec::new(),
        pending_receives: Vec::new(),
        shm: None,
        screencopy: None,
        ext_source_mgr: None,
        ext_capture_mgr: None,
        capture: Capture::None,
        registry_settled: false,
        pointer_backend: None,
        keyboard_backend: None,
        virtual_input_ready: false,
        capture_active: false,
        vkbd_keymap: None,
        keymap_hash: None,
        output_config: None,
        power_mgr: None,
    };

    // settle the registry globals, and the events they trigger, before the
    // caller spawns the wrapped binary; the first roundtrip delivers every
    // initial global, so the capture backend is chosen after it with all of
    // them known rather than swapped as they arrive
    for i in 0..3 {
        if let Err(e) = queue.roundtrip(&mut state) {
            crate::warning!("wayland setup failed: {e}; using default geometry");
            return;
        }
        if i == 0 {
            state.registry_settled = true;
            state.maybe_start_captures(&qh);
            state.transient_seat_settled(&conn, &qh);
            state.output_power_settled();
            if state.server.config.outmgr_wants_wlr() && state.output_config.is_none() {
                crate::warning!(
                    "dynamic resolution is unavailable: the compositor has no \
                     zwlr_output_manager_v1 (clients' resolution changes will fail)"
                );
            }
        }
    }
    // the seats and their names have all landed by now; say so if none was
    // usable rather than leave input silently dead
    state.warn_if_no_seat();
    let dynres_wake = state.server.dynres.wake_fd();
    let clip_wake = state.server.clipboard.jobs().wake_fd();
    std::thread::spawn(move || {
        // Marks the window in which the X threads have somewhere to post their
        // clipboard work. Outside it they are told there is nobody to do it and
        // refuse, rather than leaving requestors waiting for an answer that
        // would never come — and because it is a guard, that holds however this
        // thread ends, including unwinding from a panic in a Dispatch impl or a
        // capture tick.
        struct Draining(Arc<Server>);
        impl Draining {
            fn new(server: Arc<Server>) -> Self {
                server.clipboard.jobs().set_running(true);
                Self(server)
            }
        }
        impl Drop for Draining {
            fn drop(&mut self) {
                self.0.clipboard.jobs().set_running(false);
            }
        }
        let _draining = Draining::new(state.server.clone());

        // A manual loop rather than blocking_dispatch, so capture can self-pace:
        // each pass issues whatever is due, then waits on the socket only until
        // the next capture is scheduled.
        let mut pfds: Vec<libc::pollfd> = Vec::new();
        loop {
            if let Err(e) = queue.dispatch_pending(&mut state) {
                crate::warning!("wayland dispatch failed, continuing: {e}");
            }
            // after the dispatch, so an offer a selection change replaced is
            // destroyed in the same pass that replaced it
            state.run_clip_jobs(&conn);
            // before the tick, so a stalled clipboard transfer makes progress
            // on every pass however we got here
            state.flush_pending_sends();
            state.flush_pending_receives();
            state.tick_dynres(&qh);
            let timeout = state.tick_captures(&qh);
            if let Err(e) = conn.flush() {
                crate::warning!(
                    "wayland connection lost on flush: {e}; screen capture has stopped"
                );
                state.wayland_lost();
                return;
            }
            let Some(guard) = conn.prepare_read() else {
                continue; // events already queued, go dispatch them
            };
            pfds.clear();
            // the fixed three come first, so their revents are at known
            // indices; everything after them is a clipboard transfer's pipe
            pfds.push(libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            // wake as soon as an X client parks a resolution change (-dynres),
            // rather than after the capture interval
            pfds.push(libc::pollfd {
                fd: dynres_wake,
                events: libc::POLLIN,
                revents: 0,
            });
            // likewise for clipboard work an X thread posted: a paste waits on
            // it, so it must not sit in the queue for a capture interval
            pfds.push(libc::pollfd {
                fd: clip_wake,
                events: libc::POLLIN,
                revents: 0,
            });
            // wake as soon as a stalled send's receiver drains its pipe, or a
            // value we are reading for an X requestor arrives
            pfds.extend(state.pending_send_fds().map(|fd| libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            }));
            pfds.extend(state.pending_receive_fds().map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            }));
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
            if n > 0 && pfds[1].revents & libc::POLLIN != 0 {
                state.server.dynres.drain_wake();
            }
            if n > 0 && pfds[2].revents & libc::POLLIN != 0 {
                state.server.clipboard.jobs().drain_wake();
            }
            if n > 0 && pfds[0].revents & libc::POLLIN != 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        crate::warning!(
                            "wayland connection lost on read: {e}; screen capture has stopped"
                        );
                        state.wayland_lost();
                        return;
                    }
                }
            }
            // on timeout or error, drop the guard and loop round to tick again
        }
    });
}

/// Everything the event queue dispatches against: the bound globals, the
/// per-output accumulators, and the backends negotiated out of them.
struct State {
    server: Arc<Server>,
    outputs: HashMap<u32, OutputAcc>, // keyed by registry name
    /// Learns each output's logical position and size, for laying scaled outputs
    /// out correctly.
    xdg_output_mgr: Option<ZxdgOutputManagerV1>,
    /// How the input seat is chosen; reverts to `First` if a transient seat
    /// cannot be had.
    seat_mode: SeatMode,
    /// The seat the virtual input devices go on, and whose keymap and pointer
    /// (for cursor sessions) we follow.
    seat: Option<wl_seat::WlSeat>,
    /// The seat the clipboard follows. The same as `seat`, except with
    /// `-seat transient`, where it is the first real seat: selections are per
    /// seat, and the apps on screen use the real one.
    clipboard_seat: Option<wl_seat::WlSeat>,
    /// The transient seat request, with `-seat transient`.
    transient: Option<TransientSeat>,
    /// Whether the compositor advertised `ext_transient_seat_manager_v1`,
    /// whether or not we bound it, for the hint when no seat is usable.
    has_transient_mgr: bool,
    /// Seats bound but not chosen, held alive while we wait for the one to
    /// select (its `name` event for `-seat NAME`, or the transient seat's
    /// `ready`). Keyed by registry name.
    pending_seats: HashMap<u32, wl_seat::WlSeat>,
    /// The `name` of every bound seat, keyed by registry name, for the
    /// message when `-seat NAME` matches none of them.
    seat_names: HashMap<u32, String>,
    /// The last `capabilities` from every bound seat, keyed by registry name.
    /// Kept for the ones not yet chosen, since the event may land before the
    /// `name` that selects the seat.
    seat_caps: HashMap<u32, wl_seat::Capability>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>, // passive, only for cursor sessions
    ext_manager: Option<ExtDataControlManagerV1>, // clipboard, preferred
    wlr_manager: Option<ZwlrDataControlManagerV1>, // clipboard, fallback
    device: Option<DataDevice>,
    /// Publishes an X-owned selection on that device, or withdraws ours. Only
    /// ever called from this thread, out of a `Publish` job.
    publish: Option<clipboard::Publisher>,
    offer_mimes: HashMap<ObjectId, Vec<String>>, // keyed by the offer's object id
    /// X-owned selections still being written to a receiving app's pipe. See
    /// [`State::queue_send`].
    pending_sends: Vec<PendingSend>,
    /// Wayland-owned selections still being read for a waiting X requestor.
    /// See [`State::start_receive`].
    pending_receives: Vec<PendingReceive>,
    shm: Option<wl_shm::WlShm>,
    screencopy: Option<ZwlrScreencopyManagerV1>, // capture, fallback
    ext_source_mgr: Option<ExtOutputImageCaptureSourceManagerV1>, // capture, preferred
    ext_capture_mgr: Option<ExtImageCopyCaptureManagerV1>,
    capture: Capture,       // the one in use; ext supersedes screencopy
    registry_settled: bool, // the initial globals have all arrived
    capture_active: bool,   // as of the last tick, for start/stop logging
    /// Backends that mint the virtual pointer/keyboard once a seat exists.
    pointer_backend: Option<Box<dyn PointerBackend>>,
    keyboard_backend: Option<Box<dyn KeyboardBackend>>,
    virtual_input_ready: bool, // whether those devices have been created
    /// The compositor keymap as a duped fd and size, kept because the keymap
    /// event can arrive either side of the virtual keyboard existing.
    vkbd_keymap: Option<(u32, OwnedFd, u32)>,
    /// The wlr-output-management client, bound only with `-dynres`, through
    /// which clients' resolution changes reach the compositor.
    output_config: Option<OutputConfig>,
    /// Wakes outputs the compositor put to sleep while a client is watching;
    /// see [`output_power`].
    power_mgr: Option<ZwlrOutputPowerManagerV1>,
    /// Hash of the last keymap text loaded, to ignore re-broadcasts. Forwarding a
    /// keymap to our virtual keyboard makes the compositor re-emit it to our
    /// passive wl_keyboard (sway does, when we're the only input device), and
    /// without dedup that loops until it runs out of in-flight fds
    /// (ETOOMANYREFS).
    keymap_hash: Option<u64>,
}

/// See [`SeatChoice`]; owned, since the state outlives the config borrow.
enum SeatMode {
    First,
    Named(String),
    Transient,
}

impl State {
    /// A `wl_seat` global arrived (already bound as `seat`): select it, or hold
    /// it until we know whether it is the one.
    fn seat_announced(
        &mut self,
        name: u32,
        seat: wl_seat::WlSeat,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match self.seat_mode {
            // take the first one we see
            SeatMode::First => {
                if self.seat.is_none() {
                    self.select_seat(name, seat, conn, qh);
                }
            }
            // wait for the seat's `name` event to match -seat NAME
            SeatMode::Named(_) => {
                self.pending_seats.insert(name, seat);
            }
            // wait for the transient seat's `ready` to name its global
            SeatMode::Transient => {
                self.pending_seats.insert(name, seat);
                self.transient_seat_announced(name, conn, qh);
            }
        }
    }

    /// After setup: no seat was selected, so input will not work. A transient
    /// seat is still on its way at this point (its own path warns if that
    /// fails), so only the modes that wait on the compositor's seats speak.
    fn warn_if_no_seat(&self) {
        if self.seat.is_some() {
            return;
        }
        let hint = if self.has_transient_mgr {
            " (try -seat transient)"
        } else {
            ""
        };
        match &self.seat_mode {
            SeatMode::First => {
                crate::warning!("the compositor has no wl_seat; input is disabled{hint}");
            }
            SeatMode::Named(wanted) => {
                let mut names: Vec<&str> = self.seat_names.values().map(String::as_str).collect();
                names.sort_unstable();
                let names = if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                };
                crate::warning!(
                    "no wayland seat named {wanted:?} (seats: {names}); input is disabled{hint}"
                );
            }
            SeatMode::Transient => {}
        }
    }

    /// The compositor is gone: nothing will answer a resolution change any
    /// more, so fail the one in progress and make the X side refuse the rest
    /// outright rather than wait out its timeout each time.
    fn wayland_lost(&mut self) {
        self.dynres_fail("the wayland connection was lost");
        self.server.dynres.set_available(false);
        // nothing will carry a clipboard job over any more: what is queued is
        // refused now, and what is posted later is refused on the spot, rather
        // than leaving X requestors waiting for an answer forever
        self.server.clipboard.jobs().set_running(false);
    }

    /// Commits to a seat for input, wiring up virtual input and creating its
    /// passive keyboard (for the keymap) and pointer (for cursor sessions) for
    /// whatever capabilities it has announced so far. The clipboard follows it
    /// too unless a seat was already chosen for that. Any other pending seats
    /// are dropped.
    fn select_seat(
        &mut self,
        name: u32,
        seat: wl_seat::WlSeat,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if self.clipboard_seat.is_none() {
            self.clipboard_seat = Some(seat.clone());
        }
        self.seat = Some(seat);
        self.pending_seats.clear();
        self.try_init_device(conn, qh);
        self.try_init_virtual_input(conn, qh);
        if let Some(caps) = self.seat_caps.get(&name).copied() {
            self.sync_seat_devices(caps, qh);
        }
    }

    /// Creates the passive keyboard and pointer on the chosen seat once it has
    /// the matching capability. Asking before then is a protocol error
    /// (`missing_capability`), which would kill the whole connection; a headless
    /// compositor with no input devices only gains the capabilities once our
    /// virtual devices exist. Each is created once and kept across capability
    /// changes.
    fn sync_seat_devices(&mut self, caps: wl_seat::Capability, qh: &QueueHandle<Self>) {
        let Some(seat) = &self.seat else { return };
        if caps.contains(wl_seat::Capability::Keyboard) && self.keyboard.is_none() {
            self.keyboard = Some(seat.get_keyboard(qh, ()));
        }
        if caps.contains(wl_seat::Capability::Pointer) && self.pointer.is_none() {
            let pointer = seat.get_pointer(qh, ());
            // the ext backend opens cursor sessions against the seat pointer, so
            // hand it over in case that backend was built first
            self.capture.set_pointer(pointer.clone(), qh);
            self.pointer = Some(pointer);
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        &name: &u32,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(caps),
            } => {
                state.seat_caps.insert(name, caps);
                if state.seat.as_ref() == Some(seat) {
                    state.sync_seat_devices(caps, qh);
                }
            }
            // the name matters for matching -seat NAME; once a seat is chosen
            // the rest are dropped and this no-ops
            wl_seat::Event::Name { name: seat_name } => {
                let wanted = matches!(&state.seat_mode, SeatMode::Named(n) if *n == seat_name);
                state.seat_names.insert(name, seat_name.clone());
                if wanted
                    && state.seat.is_none()
                    && let Some(seat) = state.pending_seats.remove(&name)
                {
                    crate::log!("using wayland seat {seat_name:?}");
                    state.select_seat(name, seat, conn, qh);
                }
            }
            _ => {}
        }
    }
}
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore wl_pointer::WlPointer);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
