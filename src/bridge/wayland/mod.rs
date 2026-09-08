//! Wayland client: mirrors the compositor's outputs into our RandR model and
//! bridges the clipboard via ext-data-control-v1 (preferred) or the wlr
//! fallback.
//!
//! Runs its own event-queue thread. Output changes update the shared
//! [`Screen`](crate::bridge::x11::randr::Screen); clipboard offers update the shared
//! [`Clipboard`](crate::bridge::clipboard::Clipboard) and notify X clients.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;

use wayland_client::backend::ObjectId;
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{
    self, ZxdgOutputV1,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;

use crate::bridge::Server;
use crate::bridge::clipboard::{self, DataOffer, Sel};

mod capture;
mod data_control;
mod input;
mod output;
mod shm;
use capture::Capture;
use data_control::DataDevice;
use input::{KeyboardBackend, PointerBackend};
use shm::{ShmBuffer, blit_channels, channel_map, create_shm_buffer, pixel_layout};

/// Connects, synchronously reads the initial output geometry (so the X server
/// reports the real size before the wrapped binary starts), then keeps
/// dispatching on a background thread. If no compositor is reachable we log and
/// the X server keeps its default geometry.
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
        keyboard: None,
        pointer: None,
        ext_manager: None,
        wlr_manager: None,
        device: None,
        offer_mimes: HashMap::new(),
        shm: None,
        screencopy: None,
        ext_source_mgr: None,
        ext_capture_mgr: None,
        capture: Capture::None,
        pointer_backend: None,
        keyboard_backend: None,
        virtual_input_ready: false,
        capture_active: false,
        vkbd_keymap: None,
        keymap_hash: None,
    };

    // Settle the registry globals and the resulting events before we return
    // (and the caller spawns the wrapped binary).
    for _ in 0..3 {
        if queue.roundtrip(&mut state).is_err() {
            return;
        }
    }

    std::thread::spawn(move || {
        // Manual event loop (instead of blocking_dispatch) so we can self-pace
        // capture requests: each iteration issues any due captures, then waits on
        // the Wayland socket only until the next capture is scheduled.
        loop {
            if queue.dispatch_pending(&mut state).is_err() {
                return;
            }
            let timeout = state.tick_captures(&qh);
            if conn.flush().is_err() {
                return;
            }
            let Some(guard) = conn.prepare_read() else {
                continue; // events already queued; dispatch them
            };
            let mut pfd = [nix::libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            }];
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            let n = unsafe { nix::libc::poll(pfd.as_mut_ptr(), 1, ms) };
            if n > 0 && pfd[0].revents & nix::libc::POLLIN != 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        crate::warning!("wayland read stopped: {e}");
                        return;
                    }
                }
            }
            // On timeout/error we simply drop the guard and loop to tick again.
        }
    });
}

struct State {
    server: Arc<Server>,
    /// Per-output accumulated info, keyed by registry name.
    outputs: HashMap<u32, OutputAcc>,
    /// xdg-output manager, used to learn each output's logical position/size
    /// (for correct physical layout of scaled outputs).
    xdg_output_mgr: Option<ZxdgOutputManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    /// Seats bound but not yet chosen, kept alive to receive their `name` event
    /// while we wait to match `-seat NAME` (keyed by registry name).
    pending_seats: HashMap<u32, wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// Passive pointer, held only so we can pass it to create_pointer_cursor_session.
    pointer: Option<wl_pointer::WlPointer>,
    /// ext-data-control-v1 manager (preferred).
    ext_manager: Option<ExtDataControlManagerV1>,
    /// zwlr-data-control-v1 manager (fallback).
    wlr_manager: Option<ZwlrDataControlManagerV1>,
    device: Option<DataDevice>,
    /// Mime types collected per data offer, keyed by the offer's object id.
    offer_mimes: HashMap<ObjectId, Vec<String>>,
    shm: Option<wl_shm::WlShm>,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    /// ext-image-copy-capture-v1 managers (preferred over wlr screencopy).
    ext_source_mgr: Option<ExtOutputImageCaptureSourceManagerV1>,
    ext_capture_mgr: Option<ExtImageCopyCaptureManagerV1>,
    /// The negotiated screen-capture backend (wlr or ext).
    capture: Capture,
    /// Backends that can mint the virtual pointer/keyboard once a seat exists.
    pointer_backend: Option<Box<dyn PointerBackend>>,
    keyboard_backend: Option<Box<dyn KeyboardBackend>>,
    /// Whether the virtual pointer/keyboard objects have been created.
    virtual_input_ready: bool,
    /// Tracks whether capture was enabled on the last tick (for start/stop logs).
    capture_active: bool,
    /// The compositor keymap (duped fd + size), kept so we can forward it to the
    /// virtual keyboard whenever it is created (the keymap event may arrive
    /// before or after the device exists).
    vkbd_keymap: Option<(u32, OwnedFd, u32)>,
    /// Hash of the last keymap text we loaded, used to ignore re-broadcasts.
    /// Forwarding a keymap to our virtual keyboard makes the compositor re-emit
    /// the same keymap to our passive wl_keyboard (e.g., on sway if we're the
    /// only input device); without dedup that feeds back into an infinite
    /// forward loop that exhausts in-flight fds (ETOOMANYREFS).
    keymap_hash: Option<u64>,
}

#[derive(Default)]
struct OutputAcc {
    proxy: Option<wl_output::WlOutput>,
    /// xdg-output proxy (created once the manager is available).
    xdg: Option<ZxdgOutputV1>,
    /// wl_output geometry position (logical, used as a scale-1 fallback).
    x: i32,
    y: i32,
    /// Physical mode resolution (wl_output Mode event).
    width: i32,
    height: i32,
    refresh_mhz: i32,
    /// Integer scale from wl_output (0 = unset). Only used to approximate the
    /// logical size when xdg-output is unavailable; xdg-output is preferred
    /// because it reports the true (possibly fractional) logical size.
    scale: i32,
    /// Logical position/size from xdg-output, if received.
    logical_x: Option<i32>,
    logical_y: Option<i32>,
    logical_width: Option<i32>,
    logical_height: Option<i32>,
    name: Vec<u8>,
}

impl State {
    /// Commits to a seat: creates its passive keyboard (for the keymap) and
    /// pointer (for cursor sessions), then wires up clipboard + virtual input.
    /// Drops any other pending seats.
    fn select_seat(&mut self, seat: wl_seat::WlSeat, conn: &Connection, qh: &QueueHandle<Self>) {
        self.keyboard = Some(seat.get_keyboard(qh, ()));
        let pointer = seat.get_pointer(qh, ());
        // The ext capture backend opens cursor sessions against the seat pointer;
        // hand it over now in case the backend was created before the seat (e.g.
        // shm/managers arrived first).
        self.capture.set_pointer(pointer.clone());
        self.pointer = Some(pointer);
        self.seat = Some(seat);
        self.pending_seats.clear();
        self.try_init_device(conn, qh);
        self.try_init_virtual_input(conn, qh);
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == wl_output::WlOutput::interface().name {
                    let output =
                        registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
                    state.outputs.entry(name).or_default().proxy = Some(output);
                    state.ensure_xdg_output(name, qh);
                } else if interface == ZxdgOutputManagerV1::interface().name {
                    state.xdg_output_mgr = Some(registry.bind::<ZxdgOutputManagerV1, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    ));
                    let names: Vec<u32> = state.outputs.keys().copied().collect();
                    for n in names {
                        state.ensure_xdg_output(n, qh);
                    }
                } else if interface == wl_shm::WlShm::interface().name {
                    state.shm =
                        Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), qh, ()));
                    state.maybe_start_captures(qh);
                } else if interface == ExtOutputImageCaptureSourceManagerV1::interface().name
                    && state.server.config.screen.wants_imagecopy()
                {
                    state.ext_source_mgr =
                        Some(registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ));
                    state.maybe_start_captures(qh);
                } else if interface == ExtImageCopyCaptureManagerV1::interface().name
                    && state.server.config.screen.wants_imagecopy()
                {
                    state.ext_capture_mgr =
                        Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ));
                    state.maybe_start_captures(qh);
                } else if interface == ZwlrScreencopyManagerV1::interface().name
                    && state.server.config.screen.wants_screencopy()
                {
                    state.screencopy = Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    ));
                    state.maybe_start_captures(qh);
                } else if interface == wl_seat::WlSeat::interface().name {
                    let seat =
                        registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, name);
                    if state.server.config.seat.is_none() {
                        // No -seat: use the first seat we see.
                        if state.seat.is_none() {
                            state.select_seat(seat, conn, qh);
                        }
                    } else {
                        // Wait for the seat's `name` event to match -seat NAME.
                        state.pending_seats.insert(name, seat);
                    }
                } else if interface == ExtDataControlManagerV1::interface().name
                    && state.server.config.clipboard.wants_ext()
                {
                    let mgr = registry.bind::<ExtDataControlManagerV1, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    );
                    state.ext_manager = Some(mgr);
                    state.try_init_device(conn, qh);
                } else if interface == ZwlrDataControlManagerV1::interface().name
                    && state.server.config.clipboard.wants_wlr()
                {
                    let mgr = registry.bind::<ZwlrDataControlManagerV1, _, _>(
                        name,
                        version.min(2),
                        qh,
                        (),
                    );
                    state.wlr_manager = Some(mgr);
                    state.try_init_device(conn, qh);
                } else if interface == ZwlrVirtualPointerManagerV1::interface().name {
                    let mgr = registry.bind::<ZwlrVirtualPointerManagerV1, _, _>(
                        name,
                        version.min(2),
                        qh,
                        (),
                    );
                    state.pointer_backend = Some(Box::new(mgr));
                    state.try_init_virtual_input(conn, qh);
                } else if interface == ZwpVirtualKeyboardManagerV1::interface().name {
                    let mgr = registry.bind::<ZwpVirtualKeyboardManagerV1, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    );
                    state.keyboard_backend = Some(Box::new(mgr));
                    state.try_init_virtual_input(conn, qh);
                }
            }
            wl_registry::Event::GlobalRemove { name } if state.outputs.contains_key(&name) => {
                state.remove_output(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for State {
    fn event(
        state: &mut Self,
        _seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        &name: &u32,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // We only care about the seat's name, to match -seat NAME. Once a seat is
        // chosen the rest are dropped, so this no-ops afterwards.
        if let wl_seat::Event::Name { name: seat_name } = event
            && state.seat.is_none()
            && state.server.config.seat.as_deref() == Some(seat_name.as_str())
            && let Some(seat) = state.pending_seats.remove(&name)
        {
            crate::log!("using wayland seat {seat_name:?}");
            state.select_seat(seat, conn, qh);
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
