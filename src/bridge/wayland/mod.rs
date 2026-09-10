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
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use crate::bridge::Server;
use crate::bridge::clipboard::{self, DataOffer, Sel};
use crate::util::Transform;

mod capture;
mod data_control;
mod input;
mod output;
mod output_config;
mod registry;
mod shm;
use capture::Capture;
use data_control::{DataDevice, PendingSend};
use input::{KeyboardBackend, PointerBackend};
use output::OutputAcc;
use output_config::OutputConfig;
use shm::{ShmBuffer, blit_channels, channel_map, create_shm_buffer, pixel_layout};

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
    server.clipboard.set_connection(conn.clone());

    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut state = State {
        server,
        outputs: HashMap::new(),
        xdg_output_mgr: None,
        seat: None,
        pending_seats: HashMap::new(),
        seat_caps: HashMap::new(),
        keyboard: None,
        pointer: None,
        ext_manager: None,
        wlr_manager: None,
        device: None,
        offer_mimes: HashMap::new(),
        pending_sends: Vec::new(),
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
            if state.server.config.outmgr_wants_wlr() && state.output_config.is_none() {
                crate::warning!(
                    "dynamic resolution is unavailable: the compositor has no \
                     zwlr_output_manager_v1 (clients' resolution changes will fail)"
                );
            }
        }
    }
    let wake_fd = state.server.dynres.wake_fd();

    std::thread::spawn(move || {
        // A manual loop rather than blocking_dispatch, so capture can self-pace:
        // each pass issues whatever is due, then waits on the socket only until
        // the next capture is scheduled.
        let mut pfds: Vec<libc::pollfd> = Vec::new();
        loop {
            if let Err(e) = queue.dispatch_pending(&mut state) {
                crate::warning!("wayland dispatch failed, continuing: {e}");
            }
            // before the tick, so a stalled clipboard send makes progress on
            // every pass however we got here
            state.flush_pending_sends();
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
            pfds.push(libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            // wake as soon as a stalled send's receiver drains its pipe, rather
            // than waiting out the capture interval
            pfds.extend(state.pending_send_fds().map(|fd| libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            }));
            // wake as soon as an X client parks a resolution change (-dynres),
            // rather than after the capture interval
            pfds.push(libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            });
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
            if n > 0 && pfds.last().is_some_and(|p| p.revents & libc::POLLIN != 0) {
                state.server.dynres.drain_wake();
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
    seat: Option<wl_seat::WlSeat>,
    /// Seats bound but not chosen, held alive for their `name` event while we
    /// wait to match `-seat NAME`. Keyed by registry name.
    pending_seats: HashMap<u32, wl_seat::WlSeat>,
    /// The last `capabilities` from every bound seat, keyed by registry name.
    /// Kept for the ones not yet chosen, since the event may land before the
    /// `name` that selects the seat.
    seat_caps: HashMap<u32, wl_seat::Capability>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>, // passive, only for cursor sessions
    ext_manager: Option<ExtDataControlManagerV1>, // clipboard, preferred
    wlr_manager: Option<ZwlrDataControlManagerV1>, // clipboard, fallback
    device: Option<DataDevice>,
    offer_mimes: HashMap<ObjectId, Vec<String>>, // keyed by the offer's object id
    /// X-owned selections still being written to a receiving app's pipe. See
    /// [`State::queue_send`].
    pending_sends: Vec<PendingSend>,
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
    /// Hash of the last keymap text loaded, to ignore re-broadcasts. Forwarding a
    /// keymap to our virtual keyboard makes the compositor re-emit it to our
    /// passive wl_keyboard (sway does, when we're the only input device), and
    /// without dedup that loops until it runs out of in-flight fds
    /// (ETOOMANYREFS).
    keymap_hash: Option<u64>,
}

impl State {
    /// The compositor is gone: nothing will answer a resolution change any
    /// more, so fail the one in progress and make the X side refuse the rest
    /// outright rather than wait out its timeout each time.
    fn wayland_lost(&mut self) {
        self.dynres_fail("the wayland connection was lost");
        self.server.dynres.set_available(false);
    }

    /// Commits to a seat, wiring up clipboard and virtual input, and creating
    /// its passive keyboard (for the keymap) and pointer (for cursor sessions)
    /// for whatever capabilities it has announced so far. Any other pending
    /// seats are dropped.
    fn select_seat(
        &mut self,
        name: u32,
        seat: wl_seat::WlSeat,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
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
            wl_seat::Event::Name { name: seat_name }
                if state.seat.is_none()
                    && state.server.config.seat.as_deref() == Some(seat_name.as_str()) =>
            {
                if let Some(seat) = state.pending_seats.remove(&name) {
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
