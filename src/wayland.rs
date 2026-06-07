//! Wayland client: mirrors the compositor's outputs into our RandR model and
//! bridges the clipboard via ext-data-control-v1 (preferred) or the wlr
//! fallback.
//!
//! Runs its own event-queue thread. Output changes update the shared
//! [`Screen`](crate::x11::screen::Screen); clipboard offers update the shared
//! [`Clipboard`](crate::clipboard::Clipboard) and notify X clients.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wayland_client::backend::ObjectId;
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{
    self, ZxdgOutputV1,
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self as ext_frame_v1, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options as CaptureOptions,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_cursor_session_v1::{
    self as ext_cursor_session_v1, ExtImageCopyCaptureCursorSessionV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self as ext_session_v1, ExtImageCopyCaptureSessionV1,
};
use x11rb_protocol::protocol::xproto::Rectangle;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::{
    self as ext_device_v1, ExtDataControlDeviceV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{
    self as ext_offer_v1, ExtDataControlOfferV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{
    self as ext_source_v1, ExtDataControlSourceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::{
    self, ZwlrDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::{
    self, ZwlrDataControlSourceV1,
};

use crate::clipboard::{self, DataOffer, Sel};
use crate::x11::conn::Server;

/// Opcode of the `data_offer` event on both data-control device interfaces
/// (creates a child offer object); same value for wlr and ext.
const DATA_OFFER_OPCODE: u16 = 0;

/// wl_keyboard/virtual_keyboard keymap format for an xkb v1 text keymap.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// Capture pacing (see [`State::tick_captures`]). We self-pace plain `copy`
/// requests rather than rely on `copy_with_damage` (see [`Framebuffer::blit_diff`]).
///
/// The interval per output adapts to the actual change rate: a frame that changed
/// resets it to `FAST_INTERVAL`; an unchanged frame grows it geometrically toward
/// `SLOW_INTERVAL`. So continuously-changing content captures at the fast cap,
/// while sparse change (a blinking caret, a clock) backs off to a few fps and a
/// still screen reaches the slow rate — each capture costs a full-frame diff, so
/// not capturing faster than the screen changes is the whole point.
///
/// `FAST_INTERVAL` is 30fps on purpose: vncagent only consumes ~20 reads/s, so a
/// higher capture rate is wasted work the client never sees.
const FAST_INTERVAL: Duration = Duration::from_millis(33); // ~30fps cap when active
const SLOW_INTERVAL: Duration = Duration::from_millis(250); // ~4fps when still
const DISABLED_INTERVAL: Duration = Duration::from_millis(100);
/// A client is "watching" if it read pixels within this window.
const READ_GATE_MS: u64 = 1000;

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
        captures: HashMap::new(),
        vptr_manager: None,
        vkbd_manager: None,
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

/// The live data-control device, whichever protocol we negotiated. The inner
/// proxy is kept alive (not read) so the compositor doesn't destroy the device.
#[allow(dead_code)]
enum DataDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
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
    /// Per-output capture context, keyed by registry name.
    captures: HashMap<u32, CaptureCtx>,
    vptr_manager: Option<ZwlrVirtualPointerManagerV1>,
    vkbd_manager: Option<ZwpVirtualKeyboardManagerV1>,
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

/// A wl_shm-backed buffer the compositor copies a captured frame into.
struct ShmBuffer {
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    map: *mut u8,
    size: usize,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
}

// Only ever touched on the Wayland thread; the raw mapping pointer just needs to
// ride along inside `State` (which the thread owns).
unsafe impl Send for ShmBuffer {}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe { nix::libc::munmap(self.map.cast(), self.size) };
        self.buffer.destroy();
        self.pool.destroy();
    }
}

struct CaptureCtx {
    x: i32,
    y: i32,
    buffer: Option<ShmBuffer>,
    /// A capture has been requested and we're awaiting its `ready`/`failed`.
    in_flight: bool,
    /// Earliest time the next capture for this output should be requested.
    next_at: Instant,
    /// When the in-flight capture was requested (latency profiling).
    req_at: Option<Instant>,

    // --- wlr-screencopy-v1 path ---
    /// Pending frame format from the `buffer` event, applied on `buffer_done`.
    pending: Option<(wl_shm::Format, u32, u32, u32)>,
    y_invert: bool,
    /// Current adaptive capture interval (resets to fast on change, backs off
    /// toward slow on unchanged frames).
    interval: Duration,
    /// `Framebuffer::last_read_ms` seen at the previous blit (no-damage pacing).
    last_read_seen: u64,

    // --- ext-image-copy-capture-v1 path ---
    source: Option<ExtImageCaptureSourceV1>,
    session: Option<ExtImageCopyCaptureSessionV1>,
    /// True once the session has sent its `done` event after constraint negotiation.
    session_ready: bool,
    /// Buffer geometry advertised by the session's `buffer_size` event.
    session_size: Option<(u32, u32)>,
    /// Shm format to use, picked from the session's `shm_format` events.
    session_format: Option<wl_shm::Format>,
    /// Damage rects accumulated from the current frame's `damage` events
    /// (buffer coords; converted to root coords before reporting to DAMAGE).
    frame_damage: Vec<(i32, i32, i32, i32)>,
    /// Per-output cursor capture state (ext path only).
    cursor_cap: Option<CursorCap>,
}

/// Cursor image capture for one output (ext-image-copy-capture-v1).
///
/// The cursor image is identical on all outputs; only the position changes.
/// Each output's cursor session contributes position updates when the pointer
/// is on that output (signalled by `enter`/`leave`).
struct CursorCap {
    /// The `ext_image_copy_capture_cursor_session_v1` proxy — delivers
    /// `enter`, `leave`, `position`, and `hotspot` events. Kept alive so
    /// the compositor continues sending events; not read directly.
    #[allow(dead_code)]
    cursor_session: ExtImageCopyCaptureCursorSessionV1,
    /// The image-capture sub-session from `get_capture_session()`.
    cap_session: Option<ExtImageCopyCaptureSessionV1>,
    cap_ready: bool,
    cap_size: Option<(u32, u32)>,
    cap_format: Option<wl_shm::Format>,
    buf: Option<ShmBuffer>,
    in_flight: bool,
    /// Whether the pointer is currently on this output.
    present: bool,
    /// Hotspot position within the cursor image.
    hotspot: (i32, i32),
    /// Pointer hotspot in output-local buffer coordinates (None = leave).
    pos: Option<(i32, i32)>,
    frame_damage: Vec<(i32, i32, i32, i32)>,
}

/// User-data marker distinguishing cursor capture sessions/frames from screen
/// capture sessions/frames (both use `ExtImageCopyCaptureSessionV1` /
/// `ExtImageCopyCaptureFrameV1` as the Wayland type).
struct CursorSessionUD(u32); // inner = wl_name

impl State {
    /// Commits to a seat: creates its passive keyboard (for the keymap) and
    /// pointer (for cursor sessions), then wires up clipboard + virtual input.
    /// Drops any other pending seats.
    fn select_seat(&mut self, seat: wl_seat::WlSeat, conn: &Connection, qh: &QueueHandle<Self>) {
        self.keyboard = Some(seat.get_keyboard(qh, ()));
        self.pointer = Some(seat.get_pointer(qh, ()));
        self.seat = Some(seat);
        self.pending_seats.clear();
        self.try_init_device(conn, qh);
        self.try_init_virtual_input(conn, qh);
    }

    fn try_init_device(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        // If we already have an ext device, nothing to do — it's already optimal.
        if matches!(self.device, Some(DataDevice::Ext(_))) {
            return;
        }
        let Some(seat) = &self.seat else { return };

        if let Some(mgr) = &self.ext_manager {
            // ext-data-control-v1 is preferred; replace any existing wlr device.
            crate::log!("using ext-data-control-v1");
            let device = mgr.get_data_device(seat, qh, ());
            let mgr = mgr.clone();
            let dev = device.clone();
            let qh = qh.clone();
            let conn = conn.clone();
            self.server.clipboard.set_source_factory(Box::new(move |sel| {
                let source = mgr.create_data_source(&qh, sel);
                for m in clipboard::TEXT_MIMES {
                    source.offer((*m).to_string());
                }
                match sel {
                    Sel::Clipboard => dev.set_selection(Some(&source)),
                    Sel::Primary => dev.set_primary_selection(Some(&source)),
                }
                let _ = conn.flush();
            }));
            self.device = Some(DataDevice::Ext(device));
        } else if self.device.is_none() {
            let Some(mgr) = &self.wlr_manager else { return };
            crate::log!("using zwlr-data-control-v1");
            let device = mgr.get_data_device(seat, qh, ());
            let mgr = mgr.clone();
            let dev = device.clone();
            let qh = qh.clone();
            let conn = conn.clone();
            self.server.clipboard.set_source_factory(Box::new(move |sel| {
                let source = mgr.create_data_source(&qh, sel);
                for m in clipboard::TEXT_MIMES {
                    source.offer((*m).to_string());
                }
                match sel {
                    Sel::Clipboard => dev.set_selection(Some(&source)),
                    Sel::Primary if dev.version() >= 2 => dev.set_primary_selection(Some(&source)),
                    Sel::Primary => {}
                }
                let _ = conn.flush();
            }));
            self.device = Some(DataDevice::Wlr(device));
        }
    }

    /// Creates the virtual pointer + keyboard once the seat and both managers
    /// are available, and forwards any keymap we've already seen.
    fn try_init_virtual_input(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        if self.virtual_input_ready {
            return;
        }
        let (Some(seat), Some(vptr), Some(vkbd)) =
            (&self.seat, &self.vptr_manager, &self.vkbd_manager)
        else {
            return;
        };
        let pointer = vptr.create_virtual_pointer(Some(seat), qh, ());
        let keyboard = vkbd.create_virtual_keyboard(seat, qh, ());
        self.server.input.set_devices(conn.clone(), pointer, keyboard);
        if let Some((format, fd, size)) = &self.vkbd_keymap {
            self.server.input.set_keymap(*format, fd.as_fd(), *size);
        }
        self.virtual_input_ready = true;
    }

    /// Records a clipboard/primary selection change and notifies X clients.
    fn update_selection(&mut self, sel: Sel, offer: Option<DataOffer>) {
        // With -noprimary, ignore the Wayland PRIMARY selection entirely (just
        // tidy up the offer the compositor handed us).
        if sel == Sel::Primary && self.server.config.noprimary {
            if let Some(o) = offer {
                self.offer_mimes.remove(&o.id());
                o.destroy();
            }
            return;
        }
        let mimes = offer
            .as_ref()
            .and_then(|o| self.offer_mimes.remove(&o.id()))
            .unwrap_or_default();
        let owner = if offer.is_some() { clipboard::OWNER_WINDOW } else { 0 };
        let (serial, old) = self.server.clipboard.set_offer(sel, offer, mimes);
        if let Some(old) = old {
            old.destroy();
        }
        self.server.events.selection_changed(sel, owner, serial);
    }

    /// The capture interval at the fast cap, honouring `-fps` (default ~30fps).
    fn fast_interval(&self) -> Duration {
        self.server
            .config
            .fps
            .map(|f| Duration::from_millis(1000 / u64::from(f.max(1))))
            .unwrap_or(FAST_INTERVAL)
    }

    /// Creates the xdg-output for an output once both the output proxy and the
    /// xdg-output manager exist. Idempotent.
    fn ensure_xdg_output(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        let Some(mgr) = &self.xdg_output_mgr else { return };
        let Some(acc) = self.outputs.get_mut(&wl_name) else { return };
        if acc.xdg.is_some() {
            return;
        }
        let Some(output) = &acc.proxy else { return };
        acc.xdg = Some(mgr.get_xdg_output(output, qh, wl_name));
    }

    fn apply_output(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        let Some(acc) = self.outputs.get(&wl_name) else {
            return;
        };
        if acc.width <= 0 || acc.height <= 0 {
            return;
        }
        // Logical rect drives the layout topology: prefer xdg-output (accurate
        // under fractional scaling). Without it, fall back to the wl_output
        // geometry position and a logical size derived from the integer scale
        // (mode / scale) — correct for unscaled and integer-scaled outputs;
        // fractional scales still need xdg-output.
        let scale = acc.scale.max(1);
        let lx = acc.logical_x.unwrap_or(acc.x);
        let ly = acc.logical_y.unwrap_or(acc.y);
        let lw = acc.logical_width.unwrap_or(acc.width / scale);
        let lh = acc.logical_height.unwrap_or(acc.height / scale);
        let (changed, geom, ts) = {
            let mut s = self.server.screen.lock().unwrap();
            let changed = s.sync_wayland_output(
                wl_name,
                &acc.name,
                lx,
                ly,
                lw,
                lh,
                acc.width as u16,
                acc.height as u16,
                acc.refresh_mhz.max(0) as u32,
            );
            (changed, (s.width, s.height), (s.timestamp, s.config_timestamp))
        };
        self.server.framebuffer.ensure(u32::from(geom.0), u32::from(geom.1));
        // A relayout can shift other outputs' physical positions too, so refresh
        // every capture context from the recomputed layout, not just this one.
        let positions: Vec<(u32, (i32, i32))> = {
            let s = self.server.screen.lock().unwrap();
            // Feed the physical↔logical layout to the input path so absolute
            // pointer motion maps to logical coordinates (correct under scaling).
            let rects = s
                .layout_rects()
                .into_iter()
                .map(|(px, py, pw, ph, lx, ly, lw, lh)| crate::input::OutputRect {
                    px, py, pw, ph, lx, ly, lw, lh,
                })
                .collect();
            self.server.input.set_layout(rects);
            self.captures
                .keys()
                .filter_map(|&n| s.physical_pos(n).map(|p| (n, p)))
                .collect()
        };
        for (n, (px, py)) in positions {
            if let Some(ctx) = self.captures.get_mut(&n) {
                ctx.x = px;
                ctx.y = py;
            }
        }
        if changed {
            self.server.input.set_geometry(geom.0, geom.1);
            self.server.events.screen_changed(geom.0, geom.1, ts.0, ts.1);
            crate::log!("outputs changed; virtual screen {}x{}", geom.0, geom.1);
        }
        self.maybe_start_captures(qh);
    }

    /// Ensures a capture context exists for every output once shm + a screencopy
    /// protocol (wlr or ext) is available. Actual capture requests are issued,
    /// paced, by [`tick_captures`](Self::tick_captures).
    fn maybe_start_captures(&mut self, qh: &QueueHandle<Self>) {
        let has_wlr = self.shm.is_some() && self.screencopy.is_some();
        let has_ext = self.shm.is_some()
            && self.ext_source_mgr.is_some()
            && self.ext_capture_mgr.is_some();
        if !has_wlr && !has_ext {
            return;
        }
        let now = Instant::now();
        let fast = self.fast_interval();
        let names: Vec<u32> = self
            .outputs
            .iter()
            .filter(|(n, acc)| acc.proxy.is_some() && !self.captures.contains_key(n))
            .map(|(n, _)| *n)
            .collect();
        if !names.is_empty() && self.captures.is_empty() {
            crate::log!(
                "using {} for screen capture",
                if has_ext { "ext-image-copy-capture-v1" } else { "zwlr-screencopy-v1" }
            );
        }
        for name in names {
            // Physical position once the output has been synced; apply_output
            // corrects it on the first wl_output `done` otherwise.
            let (x, y) = self
                .server
                .screen
                .lock()
                .unwrap()
                .physical_pos(name)
                .unwrap_or((0, 0));
            self.captures.insert(name, CaptureCtx {
                x,
                y,
                buffer: None,
                in_flight: false,
                next_at: now,
                req_at: None,
                pending: None,
                y_invert: false,
                interval: fast,
                last_read_seen: 0,
                source: None,
                session: None,
                session_ready: false,
                session_size: None,
                session_format: None,
                cursor_cap: None,
                frame_damage: Vec::new(),
            });
        }
        // For the ext path, create sessions for any output that doesn't have one
        // yet. This handles both new outputs and the case where ext managers
        // arrive after outputs were already registered.
        if has_ext {
            let needs_session: Vec<u32> = self
                .captures
                .iter()
                .filter(|(_, c)| c.session.is_none())
                .map(|(n, _)| *n)
                .collect();
            for name in needs_session {
                self.create_ext_session(name, qh);
            }
        }
    }

    /// Whether any client is currently watching the screen (has a DAMAGE object
    /// or read pixels recently). We don't capture otherwise.
    fn capture_enabled(&self) -> bool {
        self.server.damage.active() || self.server.framebuffer.read_within(READ_GATE_MS)
    }

    /// Issues capture requests for any outputs that are due, and returns how long
    /// to wait before the loop should tick again. Called once per loop iteration.
    fn tick_captures(&mut self, qh: &QueueHandle<Self>) -> Duration {
        let has_ext = self.ext_source_mgr.is_some() && self.ext_capture_mgr.is_some();
        let enabled = self.shm.is_some()
            && (self.screencopy.is_some() || has_ext)
            && self.capture_enabled();
        if enabled != self.capture_active {
            self.capture_active = enabled;
            crate::vlog!("screen capture {}", if enabled { "started" } else { "stopped" });
        }
        if !enabled {
            return DISABLED_INTERVAL;
        }
        let now = Instant::now();
        let mut wait = SLOW_INTERVAL;
        let due: Vec<u32> = self
            .captures
            .iter()
            .filter(|(_, c)| !c.in_flight)
            .filter_map(|(n, c)| {
                if c.next_at <= now {
                    Some(*n)
                } else {
                    wait = wait.min(c.next_at - now);
                    None
                }
            })
            .collect();
        for name in due {
            if has_ext {
                self.start_capture_ext(name, qh);
            } else {
                self.start_capture(name, qh);
            }
        }
        wait
    }

    fn start_capture(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        let Some(acc) = self.outputs.get(&wl_name) else { return };
        let (Some(output), Some(sc)) = (acc.proxy.clone(), self.screencopy.clone()) else {
            return;
        };
        // overlay_cursor = 1: render the cursor into the captured image so it's
        // visible to the remote (we report a transparent XFixes cursor so the
        // VNC agent doesn't draw a second one on top).
        sc.capture_output(1, &output, qh, wl_name);
        if let Some(ctx) = self.captures.get_mut(&wl_name) {
            ctx.in_flight = true;
            ctx.req_at = Some(Instant::now());
        }
    }

    /// Creates the shm buffer (if needed) and asks the compositor to copy.
    fn copy_frame(&mut self, wl_name: u32, frame: &ZwlrScreencopyFrameV1, qh: &QueueHandle<Self>) {
        let Some(shm) = self.shm.clone() else { return };
        let Some(ctx) = self.captures.get_mut(&wl_name) else { return };
        let Some((format, w, h, stride)) = ctx.pending.take() else { return };
        let stale = ctx
            .buffer
            .as_ref()
            .is_none_or(|b| b.width != w || b.height != h || b.stride != stride || b.format != format);
        if stale {
            ctx.buffer = create_shm_buffer(&shm, qh, format, w, h, stride);
        }
        if let Some(buf) = &ctx.buffer {
            // Always plain `copy` — we compute damage ourselves by diffing in
            // blit_diff (see its comment for why we avoid copy_with_damage).
            frame.copy(&buf.buffer);
        }
    }

    /// Blits a completed capture (diffing to find what changed), reports the
    /// change as DAMAGE, and schedules the next capture for this output.
    fn frame_ready(&mut self, wl_name: u32, frame: &ZwlrScreencopyFrameV1, _qh: &QueueHandle<Self>) {
        // Only diff (the expensive per-frame compare) when a client actually has
        // a DAMAGE object; otherwise we just refresh the framebuffer for polled
        // GetImage/ShmGetImage reads and skip computing damage nobody consumes.
        let compute_damage = self.server.damage.active();
        let (mut bbox, mut blit_wait, mut blit_work) = (None, 0u64, 0u64);
        if let Some(ctx) = self.captures.get(&wl_name)
            && let Some(b) = &ctx.buffer
        {
            let slice = unsafe { std::slice::from_raw_parts(b.map, b.size) };
            (bbox, blit_wait, blit_work) = self.server.framebuffer.blit_diff(
                ctx.x,
                ctx.y,
                b.width,
                b.height,
                b.stride,
                slice,
                ctx.y_invert,
                compute_damage,
                blit_channels(wl_name, b.format),
            );
        }
        let read_ms = self.server.framebuffer.last_read_ms();
        let fast = self.fast_interval();
        let now = Instant::now();
        if let Some(ctx) = self.captures.get_mut(&wl_name) {
            ctx.in_flight = false;
            let req_latency = ctx.req_at.take().map_or(0, |t| t.elapsed().as_nanos() as u64);
            // Pace to the rate the consumer actually wants frames, then back off
            // geometrically toward the slow rate. With damage that signal is a
            // pixel change; without damage (the client polls reads, and we can't
            // tell what changed without the compare we deliberately skip) it is
            // whether the client read our last frame — so we don't keep blitting
            // 25MB frames the client never pulls. The read-gate stops capture
            // entirely once the client stops reading.
            let keep_fast = if compute_damage {
                bbox.is_some()
            } else {
                let consumed = read_ms != ctx.last_read_seen;
                ctx.last_read_seen = read_ms;
                consumed
            };
            ctx.interval = if keep_fast {
                fast
            } else {
                (ctx.interval * 3 / 2).min(SLOW_INTERVAL)
            };
            ctx.next_at = now + ctx.interval;
            crate::prof::frame(blit_wait, blit_work, req_latency);
        }
        if let Some(r) = bbox {
            let geom = {
                let s = self.server.screen.lock().unwrap();
                (s.width, s.height)
            };
            self.server.damage.add_damage(&[r], geom);
        }
        frame.destroy();
    }

    fn frame_failed(&mut self, wl_name: u32, frame: &ZwlrScreencopyFrameV1, _qh: &QueueHandle<Self>) {
        crate::warning!("capture failed for output {wl_name}");
        if let Some(ctx) = self.captures.get_mut(&wl_name) {
            ctx.in_flight = false;
            ctx.req_at = None;
            ctx.next_at = Instant::now() + SLOW_INTERVAL; // back off, then retry
        }
        frame.destroy();
    }

    // --- ext-image-copy-capture-v1 helpers ---

    fn create_ext_session(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        let Some(source_mgr) = self.ext_source_mgr.clone() else { return };
        let Some(cap_mgr) = self.ext_capture_mgr.clone() else { return };
        let Some(output) = self.outputs.get(&wl_name).and_then(|a| a.proxy.clone()) else {
            return;
        };
        let source = source_mgr.create_source(&output, qh, ());
        // `-cursor baked` composites the cursor into the screen frames (we then
        // report a transparent XFixes cursor so the agent doesn't double-draw);
        // otherwise it's captured separately and delivered via XFixes.
        let bake = self.server.config.cursor == crate::config::CursorType::Baked;
        let options = if bake { CaptureOptions::PaintCursors } else { CaptureOptions::empty() };
        let session = cap_mgr.create_session(&source, options, qh, wl_name);

        // Cursor session: one per output so we get position events for whichever
        // output the pointer is currently on. Skipped when baking.
        let cursor_cap = self.pointer.as_ref().filter(|_| !bake).map(|ptr| {
            let cursor_session = cap_mgr.create_pointer_cursor_session(&source, ptr, qh, wl_name);
            let cap_session = cursor_session.get_capture_session(qh, CursorSessionUD(wl_name));
            CursorCap {
                cursor_session,
                cap_session: Some(cap_session),
                cap_ready: false,
                cap_size: None,
                cap_format: None,
                buf: None,
                in_flight: false,
                present: false,
                hotspot: (0, 0),
                pos: None,
                frame_damage: Vec::new(),
            }
        });

        if let Some(ctx) = self.captures.get_mut(&wl_name) {
            ctx.source = Some(source);
            ctx.session = Some(session);
            ctx.session_ready = false;
            ctx.session_size = None;
            ctx.session_format = None;
            ctx.cursor_cap = cursor_cap;
            ctx.next_at = Instant::now() + SLOW_INTERVAL; // wait for Done events
        }
    }

    fn start_capture_ext(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        // Determine the action without holding a mutable borrow.
        enum Action {
            NeedSession,
            WaitConstraints,
            Capture { session: ExtImageCopyCaptureSessionV1, w: u32, h: u32, fmt: wl_shm::Format },
        }
        let action = {
            let Some(ctx) = self.captures.get(&wl_name) else { return };
            if ctx.in_flight { return; }
            if ctx.session.is_none() {
                Action::NeedSession
            } else if !ctx.session_ready {
                Action::WaitConstraints
            } else {
                match (ctx.session.clone(), ctx.session_size, ctx.session_format) {
                    (Some(s), Some((w, h)), Some(fmt)) => Action::Capture { session: s, w, h, fmt },
                    _ => return,
                }
            }
        };
        match action {
            Action::NeedSession => {
                self.create_ext_session(wl_name, qh);
                // next_at already set to SLOW_INTERVAL by create_ext_session
            }
            Action::WaitConstraints => {
                if let Some(ctx) = self.captures.get_mut(&wl_name) {
                    ctx.next_at = Instant::now() + SLOW_INTERVAL;
                }
            }
            Action::Capture { session, w, h, fmt } => {
                let Some(shm) = self.shm.clone() else { return };
                let stride = w * 4; // Xrgb8888: 4 bytes per pixel
                let ctx = self.captures.get_mut(&wl_name).unwrap();
                let stale = ctx.buffer.as_ref().is_none_or(|b| b.width != w || b.height != h);
                if stale {
                    ctx.buffer = create_shm_buffer(&shm, qh, fmt, w, h, stride);
                }
                if let Some(buf) = &ctx.buffer {
                    let frame = session.create_frame(qh, wl_name);
                    frame.attach_buffer(&buf.buffer);
                    frame.damage_buffer(0, 0, w as i32, h as i32);
                    frame.capture();
                    ctx.in_flight = true;
                    ctx.req_at = Some(Instant::now());
                    ctx.frame_damage.clear();
                }
            }
        }
    }

    fn ext_frame_ready(&mut self, wl_name: u32, frame: &ExtImageCopyCaptureFrameV1, qh: &QueueHandle<Self>) {
        let compute_damage = self.server.damage.active();
        let (mut blit_wait, mut blit_work) = (0u64, 0u64);
        if let Some(ctx) = self.captures.get(&wl_name)
            && let Some(b) = &ctx.buffer
        {
            let slice = unsafe { std::slice::from_raw_parts(b.map, b.size) };
            // Blit the frame into the shared framebuffer; skip the diff since
            // the compositor gives us damage rects directly.
            let (_, w, wk) = self.server.framebuffer.blit_diff(
                ctx.x, ctx.y, b.width, b.height, b.stride, slice, false, false,
                blit_channels(wl_name, b.format),
            );
            blit_wait = w;
            blit_work = wk;
        }
        // Convert compositor damage (buffer-local coords) to virtual-screen coords.
        let compositor_rects: Vec<Rectangle>;
        if compute_damage {
            compositor_rects = self.captures.get(&wl_name).map_or(Vec::new(), |ctx| {
                ctx.frame_damage.iter().map(|&(x, y, w, h)| Rectangle {
                    x: ctx.x as i16 + x as i16,
                    y: ctx.y as i16 + y as i16,
                    width: w.max(0) as u16,
                    height: h.max(0) as u16,
                }).collect()
            });
        } else {
            compositor_rects = Vec::new();
        }
        let now = Instant::now();
        if let Some(ctx) = self.captures.get_mut(&wl_name) {
            ctx.in_flight = false;
            ctx.frame_damage.clear();
            let req_latency = ctx.req_at.take().map_or(0, |t| t.elapsed().as_nanos() as u64);
            ctx.next_at = now; // immediately due for the next tick
            crate::prof::frame(blit_wait, blit_work, req_latency);
        }
        if compute_damage && !compositor_rects.is_empty() {
            let geom = { let s = self.server.screen.lock().unwrap(); (s.width, s.height) };
            self.server.damage.add_damage(&compositor_rects, geom);
        }
        frame.destroy();
        // Immediately requeue so the compositor can hold the next frame request
        // until content changes (the equivalent of our self-paced wlr loop).
        if self.capture_enabled() {
            self.start_capture_ext(wl_name, qh);
        }
    }

    fn ext_frame_failed(
        &mut self,
        wl_name: u32,
        reason: WEnum<ext_frame_v1::FailureReason>,
        frame: &ExtImageCopyCaptureFrameV1,
        qh: &QueueHandle<Self>,
    ) {
        let reason_str = match reason {
            WEnum::Value(ext_frame_v1::FailureReason::BufferConstraints) => "buffer-constraints",
            WEnum::Value(ext_frame_v1::FailureReason::Stopped) => "session-stopped",
            _ => "unknown",
        };
        crate::vlog!("ext capture frame failed ({reason_str}) for output {wl_name}");
        if let Some(ctx) = self.captures.get_mut(&wl_name) {
            ctx.in_flight = false;
            ctx.req_at = None;
            ctx.next_at = Instant::now() + SLOW_INTERVAL;
            // On buffer-constraints (e.g. the output resized) the session re-sends
            // its constraints via its own `done` and we just need a buffer that
            // matches them. Drop the stale buffer so the paced retry recreates it
            // against the current `session_size`. Do NOT clear `session_ready` /
            // `session_size` here: the re-negotiation `done` may already have been
            // delivered, and clearing it would leave us waiting for a `done` that
            // never comes — stalling capture for good.
            if matches!(reason, WEnum::Value(ext_frame_v1::FailureReason::BufferConstraints)) {
                ctx.buffer = None;
            }
        }
        frame.destroy();
        // tick_captures → start_capture_ext recreates the buffer and retries.
        let _ = qh;
    }

    // --- cursor capture helpers (ext path) ---

    fn start_cursor_cap_ext(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        let (cap_session, w, h, fmt) = {
            let Some(ctx) = self.captures.get(&wl_name) else { return };
            let Some(cc) = &ctx.cursor_cap else { return };
            if cc.in_flight || !cc.present || !cc.cap_ready { return; }
            match (cc.cap_session.clone(), cc.cap_size, cc.cap_format) {
                (Some(s), Some((w, h)), Some(fmt)) => (s, w, h, fmt),
                _ => return,
            }
        };
        let Some(shm) = self.shm.clone() else { return };
        let stride = w * 4;
        let ctx = self.captures.get_mut(&wl_name).unwrap();
        let cc = ctx.cursor_cap.as_mut().unwrap();
        let stale = cc.buf.as_ref().is_none_or(|b| b.width != w || b.height != h);
        if stale {
            cc.buf = create_shm_buffer(&shm, qh, fmt, w, h, stride);
        }
        if let Some(buf) = &cc.buf {
            let frame = cap_session.create_frame(qh, CursorSessionUD(wl_name));
            frame.attach_buffer(&buf.buffer);
            frame.damage_buffer(0, 0, w as i32, h as i32);
            frame.capture();
            cc.in_flight = true;
            cc.frame_damage.clear();
        }
    }

    fn cursor_frame_ready(&mut self, wl_name: u32, frame: &ExtImageCopyCaptureFrameV1, qh: &QueueHandle<Self>) {
        let serial = {
            let Some(ctx) = self.captures.get(&wl_name) else { frame.destroy(); return };
            let Some(cc) = &ctx.cursor_cap else { frame.destroy(); return };
            let Some(buf) = &cc.buf else { frame.destroy(); return };
            let pixel_count = (buf.width * buf.height) as usize;
            // SAFETY: shm mapping, pixel_count * 4 == buf.size, 4-byte aligned.
            let raw = unsafe { std::slice::from_raw_parts(buf.map as *const u32, pixel_count) };
            // Convert each captured pixel to X cursor ARGB (0xAARRGGBB). x-formats
            // (no alpha) are forced opaque so the cursor isn't fully transparent.
            let image: Vec<u32> = match cc.cap_format.and_then(pixel_layout) {
                Some(([bi, gi, ri], ai)) => raw
                    .iter()
                    .map(|&p| {
                        let by = p.to_le_bytes();
                        let a = ai.map_or(0xff, |i| by[i]);
                        u32::from_be_bytes([a, by[ri], by[gi], by[bi]])
                    })
                    .collect(),
                None => raw.iter().map(|p| p | 0xFF00_0000).collect(),
            };
            self.server.cursor.update_image(
                buf.width as u16, buf.height as u16,
                cc.hotspot.0 as u16, cc.hotspot.1 as u16,
                image,
            )
        };
        self.server.events.cursor_changed(serial);
        if let Some(ctx) = self.captures.get_mut(&wl_name)
            && let Some(cc) = ctx.cursor_cap.as_mut()
        {
            cc.in_flight = false;
            cc.frame_damage.clear();
        }
        frame.destroy();
        self.start_cursor_cap_ext(wl_name, qh);
    }

    fn cursor_frame_failed(&mut self, wl_name: u32, frame: &ExtImageCopyCaptureFrameV1) {
        crate::vlog!("cursor capture frame failed for output {wl_name}");
        if let Some(ctx) = self.captures.get_mut(&wl_name)
            && let Some(cc) = ctx.cursor_cap.as_mut()
        {
            cc.in_flight = false;
            cc.cap_ready = false;
            cc.cap_size = None;
            cc.cap_format = None;
        }
        frame.destroy();
    }

    fn remove_output(&mut self, wl_name: u32) {
        self.outputs.remove(&wl_name);
        self.captures.remove(&wl_name);
        let (changed, geom, ts, rects, positions) = {
            let mut s = self.server.screen.lock().unwrap();
            let changed = s.remove_wayland_output(wl_name);
            let rects = s.layout_rects();
            let positions: Vec<(u32, (i32, i32))> = self
                .captures
                .keys()
                .filter_map(|&n| s.physical_pos(n).map(|p| (n, p)))
                .collect();
            (changed, (s.width, s.height), (s.timestamp, s.config_timestamp), rects, positions)
        };
        // Resize the framebuffer and refresh the remaining outputs' positions and
        // input layout — removing an output can shrink the screen and shift the
        // others (same work apply_output does when an output is added/changed).
        self.server.framebuffer.ensure(u32::from(geom.0), u32::from(geom.1));
        let rects = rects
            .into_iter()
            .map(|(px, py, pw, ph, lx, ly, lw, lh)| crate::input::OutputRect {
                px, py, pw, ph, lx, ly, lw, lh,
            })
            .collect();
        self.server.input.set_layout(rects);
        for (n, (px, py)) in positions {
            if let Some(ctx) = self.captures.get_mut(&n) {
                ctx.x = px;
                ctx.y = py;
            }
        }
        if changed {
            self.server.input.set_geometry(geom.0, geom.1);
            self.server.events.screen_changed(geom.0, geom.1, ts.0, ts.1);
        }
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
            wl_registry::Event::Global { name, interface, version } => {
                if interface == wl_output::WlOutput::interface().name {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
                    state.outputs.entry(name).or_default().proxy = Some(output);
                    state.ensure_xdg_output(name, qh);
                } else if interface == ZxdgOutputManagerV1::interface().name {
                    state.xdg_output_mgr =
                        Some(registry.bind::<ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ()));
                    let names: Vec<u32> = state.outputs.keys().copied().collect();
                    for n in names {
                        state.ensure_xdg_output(n, qh);
                    }
                } else if interface == wl_shm::WlShm::interface().name {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), qh, ()));
                    state.maybe_start_captures(qh);
                } else if interface == ExtOutputImageCaptureSourceManagerV1::interface().name
                    && state.server.config.screen.wants_imagecopy()
                {
                    state.ext_source_mgr =
                        Some(registry.bind::<ExtOutputImageCaptureSourceManagerV1, _, _>(name, version.min(1), qh, ()));
                    state.maybe_start_captures(qh);
                } else if interface == ExtImageCopyCaptureManagerV1::interface().name
                    && state.server.config.screen.wants_imagecopy()
                {
                    state.ext_capture_mgr =
                        Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(name, version.min(1), qh, ()));
                    state.maybe_start_captures(qh);
                } else if interface == ZwlrScreencopyManagerV1::interface().name
                    && state.server.config.screen.wants_screencopy()
                {
                    state.screencopy =
                        Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, version.min(3), qh, ()));
                    state.maybe_start_captures(qh);
                } else if interface == wl_seat::WlSeat::interface().name {
                    let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, name);
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
                    let mgr = registry.bind::<ExtDataControlManagerV1, _, _>(name, version.min(1), qh, ());
                    state.ext_manager = Some(mgr);
                    state.try_init_device(conn, qh);
                } else if interface == ZwlrDataControlManagerV1::interface().name
                    && state.server.config.clipboard.wants_wlr()
                {
                    let mgr = registry.bind::<ZwlrDataControlManagerV1, _, _>(name, version.min(2), qh, ());
                    state.wlr_manager = Some(mgr);
                    state.try_init_device(conn, qh);
                } else if interface == ZwlrVirtualPointerManagerV1::interface().name {
                    let mgr = registry.bind::<ZwlrVirtualPointerManagerV1, _, _>(name, version.min(2), qh, ());
                    state.vptr_manager = Some(mgr);
                    state.try_init_virtual_input(conn, qh);
                } else if interface == ZwpVirtualKeyboardManagerV1::interface().name {
                    let mgr = registry.bind::<ZwpVirtualKeyboardManagerV1, _, _>(name, version.min(1), qh, ());
                    state.vkbd_manager = Some(mgr);
                    state.try_init_virtual_input(conn, qh);
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if state.outputs.contains_key(&name) {
                    state.remove_output(name);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _output: &wl_output::WlOutput,
        event: wl_output::Event,
        &wl_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let acc = state.outputs.entry(wl_name).or_default();
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                acc.x = x;
                acc.y = y;
            }
            wl_output::Event::Mode { flags, width, height, refresh } => {
                if let WEnum::Value(flags) = flags
                    && flags.contains(wl_output::Mode::Current)
                {
                    acc.width = width;
                    acc.height = height;
                    acc.refresh_mhz = refresh;
                }
            }
            wl_output::Event::Scale { factor } => acc.scale = factor,
            wl_output::Event::Name { name } => acc.name = name.into_bytes(),
            wl_output::Event::Done => state.apply_output(wl_name, qh),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _xdg: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        &wl_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zxdg_output_v1::Event;
        let Some(acc) = state.outputs.get_mut(&wl_name) else { return };
        match event {
            Event::LogicalPosition { x, y } => {
                acc.logical_x = Some(x);
                acc.logical_y = Some(y);
            }
            Event::LogicalSize { width, height } => {
                acc.logical_width = Some(width);
                acc.logical_height = Some(height);
            }
            // Deprecated in v3 (the compositor sends wl_output.done instead, which
            // already drives apply_output); honoured for v1/v2 compositors.
            Event::Done => state.apply_output(wl_name, qh),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, u32> for State {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        &wl_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format, width, height, stride } => {
                if let WEnum::Value(format) = format
                    && let Some(ctx) = state.captures.get_mut(&wl_name)
                {
                    ctx.pending = Some((format, width, height, stride));
                    if frame.version() < 3 {
                        state.copy_frame(wl_name, frame, qh);
                    }
                }
            }
            Event::BufferDone => state.copy_frame(wl_name, frame, qh),
            Event::Flags { flags } => {
                if let WEnum::Value(flags) = flags
                    && let Some(ctx) = state.captures.get_mut(&wl_name)
                {
                    ctx.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            // We use plain `copy`, so no Damage events arrive; we diff instead.
            Event::Ready { .. } => state.frame_ready(wl_name, frame, qh),
            Event::Failed => state.frame_failed(wl_name, frame, qh),
            _ => {}
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ExtDataControlDeviceV1,
        event: ext_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.offer_mimes.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => state.update_selection(Sel::Clipboard, id.map(DataOffer::Ext)),
            Event::PrimarySelection { id } => state.update_selection(Sel::Primary, id.map(DataOffer::Ext)),
            Event::Finished => crate::vlog!("ext-data-control device finished"),
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mimes.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Sel> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_source_v1::Event,
        sel: &Sel,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_source_v1::Event::Send { mime_type: _, fd } => {
                let data = state.server.clipboard.x_data(*sel);
                let _ = std::fs::File::from(fd).write_all(&data);
            }
            ext_source_v1::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.offer_mimes.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => state.update_selection(Sel::Clipboard, id.map(DataOffer::Wlr)),
            Event::PrimarySelection { id } => state.update_selection(Sel::Primary, id.map(DataOffer::Wlr)),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mimes.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, Sel> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        sel: &Sel,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type: _, fd } => {
                let data = state.server.clipboard.x_data(*sel);
                let _ = File::from(fd).write_all(&data);
            }
            zwlr_data_control_source_v1::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

/// Byte offsets, within a little-endian 4-byte pixel, of the `[blue, green,
/// red]` channels and (optionally) the alpha channel. Enough to convert any
/// byte-ordered 8888 wl_shm format to/from our native layout; `None` for
/// packed/float formats (10-bit etc.) that need bit unpacking, not a byte
/// permutation. Offsets are the reverse of the DRM fourcc channel order.
fn pixel_layout(fmt: wl_shm::Format) -> Option<(crate::capture::ChannelMap, Option<usize>)> {
    use wl_shm::Format as F;
    Some(match fmt {
        F::Xrgb8888 => ([0, 1, 2], None),    // memory B,G,R,x
        F::Argb8888 => ([0, 1, 2], Some(3)), // memory B,G,R,A
        F::Xbgr8888 => ([2, 1, 0], None),    // memory R,G,B,x
        F::Abgr8888 => ([2, 1, 0], Some(3)), // memory R,G,B,A
        F::Rgbx8888 => ([1, 2, 3], None),    // memory x,B,G,R
        F::Rgba8888 => ([1, 2, 3], Some(0)), // memory A,B,G,R
        F::Bgrx8888 => ([3, 2, 1], None),    // memory x,R,G,B
        F::Bgra8888 => ([3, 2, 1], Some(0)), // memory A,R,G,B
        _ => return None,
    })
}

/// The blit channel map (B,G,R source offsets) for converting a captured buffer
/// in `fmt` into our XRGB framebuffer.
fn channel_map(fmt: wl_shm::Format) -> Option<crate::capture::ChannelMap> {
    pixel_layout(fmt).map(|(bgr, _)| bgr)
}

/// Resolves the blit channel map for a buffer format, warning once-ish and
/// falling back to the native order for formats we can't byte-permute.
fn blit_channels(wl_name: u32, fmt: wl_shm::Format) -> crate::capture::ChannelMap {
    channel_map(fmt).unwrap_or_else(|| {
        crate::warning!(
            "output {wl_name} capture format {fmt:?} is not a byte-ordered \
             8888 format; colors may be wrong"
        );
        crate::capture::XRGB
    })
}

/// Creates a memfd-backed wl_shm buffer of the given geometry/format.
fn create_shm_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
) -> Option<ShmBuffer> {
    let size = stride as usize * height as usize;
    if size == 0 {
        return None;
    }
    // SAFETY: standard memfd_create + ftruncate + mmap dance.
    let fd = unsafe {
        let fd = nix::libc::memfd_create(c"wl-uinput-proxy-capture".as_ptr(), 0);
        if fd < 0 {
            return None;
        }
        let fd = OwnedFd::from_raw_fd(fd);
        if nix::libc::ftruncate(fd.as_fd().as_raw_fd(), size as nix::libc::off_t) < 0 {
            return None;
        }
        fd
    };
    let map = unsafe {
        nix::libc::mmap(
            std::ptr::null_mut(),
            size,
            nix::libc::PROT_READ | nix::libc::PROT_WRITE,
            nix::libc::MAP_SHARED,
            fd.as_fd().as_raw_fd(),
            0,
        )
    };
    if map == nix::libc::MAP_FAILED {
        return None;
    }
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
    Some(ShmBuffer {
        buffer,
        pool,
        map: map.cast(),
        size,
        width,
        height,
        stride,
        format,
    })
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _kbd: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event
            && format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
        {
            // Read the keymap text first so we can dedup. Forwarding a keymap to
            // our virtual keyboard makes sway re-broadcast the same keymap to our
            // passive wl_keyboard; ignoring an unchanged keymap breaks that
            // feedback loop (which otherwise spams + exhausts fds: ETOOMANYREFS).
            let Some(text) = (match fd.try_clone() {
                Ok(dup) => read_keymap(dup, size),
                Err(_) => None,
            }) else {
                return;
            };
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&text, &mut hasher);
            let hash = std::hash::Hasher::finish(&hasher);
            if state.keymap_hash == Some(hash) {
                return;
            }
            state.keymap_hash = Some(hash);
            // Forward the keymap to the virtual keyboard verbatim. The X keymap
            // we advertise is built from this same keymap (X keycode = evdev+8 =
            // xkb keycode), so the keycodes the client sends resolve to the same
            // keysyms — the forward is an identity mapping, but the protocol
            // requires a keymap be set before any `key` request. A dup is kept so
            // we can re-forward if the virtual keyboard is created later.
            if let Ok(dup) = fd.try_clone() {
                state.server.input.set_keymap(KEYMAP_FORMAT_XKB_V1, dup.as_fd(), size);
                state.vkbd_keymap = Some((KEYMAP_FORMAT_XKB_V1, dup, size));
            }
            // Track modifier state from this keymap so chords (Ctrl+C etc.)
            // produce `modifiers` updates on the virtual keyboard.
            state.server.input.set_modifier_keymap(&text);
            if let Some(table) = crate::keymap::build(&text) {
                *state.server.keymap.lock().unwrap() = Some(table);
                crate::log!("loaded compositor keymap");
                // Tell X clients the keyboard mapping changed so they re-read it
                // (vncagent caches keycode→keysym and would otherwise type stale
                // characters after a compositor layout switch).
                let count = crate::keymap::MAX_KEYCODE - crate::keymap::MIN_KEYCODE + 1;
                state
                    .server
                    .events
                    .keyboard_mapping_changed(crate::keymap::MIN_KEYCODE, count);
            }
        }
    }
}

/// Reads the null-terminated xkb keymap text from the compositor's fd.
fn read_keymap(fd: OwnedFd, size: u32) -> Option<String> {
    let mut buf = vec![0u8; size as usize];
    File::from(fd).read_exact_at(&mut buf, 0).ok()?;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).ok()
}

impl Dispatch<ExtImageCopyCaptureSessionV1, u32> for State {
    fn event(
        state: &mut Self,
        _session: &ExtImageCopyCaptureSessionV1,
        event: ext_session_v1::Event,
        &wl_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                if let Some(ctx) = state.captures.get_mut(&wl_name) {
                    ctx.session_size = Some((width, height));
                }
            }
            Event::ShmFormat { format } => {
                if let WEnum::Value(fmt) = format
                    && let Some(ctx) = state.captures.get_mut(&wl_name)
                {
                    // Prefer Xrgb8888 (zero-conversion); otherwise the first
                    // byte-permutable format we can convert; only fall back to an
                    // unconvertible (packed/float) format if nothing better is
                    // offered.
                    let better = match ctx.session_format {
                        None => true,
                        Some(cur) if cur == wl_shm::Format::Xrgb8888 => false,
                        Some(cur) => {
                            fmt == wl_shm::Format::Xrgb8888
                                || (channel_map(fmt).is_some() && channel_map(cur).is_none())
                        }
                    };
                    if better {
                        ctx.session_format = Some(fmt);
                    }
                }
            }
            Event::Done => {
                if let Some(ctx) = state.captures.get_mut(&wl_name) {
                    let Some(fmt) = ctx.session_format else {
                        crate::warning!("ext session for output {wl_name} offered no shm format; skipping");
                        return;
                    };
                    crate::log!("ext capture output {wl_name} using shm format {fmt:?}");
                    ctx.session_ready = true;
                }
                // Kick off the first frame now that constraints are known.
                state.start_capture_ext(wl_name, qh);
            }
            Event::Stopped => {
                crate::vlog!("ext capture session stopped for output {wl_name}; will restart");
                if let Some(ctx) = state.captures.get_mut(&wl_name) {
                    ctx.session = None;
                    ctx.session_ready = false;
                    ctx.session_size = None;
                    ctx.session_format = None;
                    ctx.in_flight = false;
                    ctx.next_at = Instant::now() + SLOW_INTERVAL;
                }
                // tick_captures → start_capture_ext will recreate the session.
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u32> for State {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_frame_v1::Event,
        &wl_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_frame_v1::Event;
        match event {
            Event::Damage { x, y, width, height } => {
                if let Some(ctx) = state.captures.get_mut(&wl_name) {
                    ctx.frame_damage.push((x, y, width, height));
                }
            }
            Event::Transform { .. } | Event::PresentationTime { .. } => {}
            Event::Ready => state.ext_frame_ready(wl_name, frame, qh),
            Event::Failed { reason } => state.ext_frame_failed(wl_name, reason, frame, qh),
            _ => {}
        }
    }
}

/// Cursor-session user-data: distinguishes cursor sessions/frames from screen
/// sessions/frames (both use the same `ExtImageCopyCaptureSessionV1` /
/// `ExtImageCopyCaptureFrameV1` types).
impl Dispatch<ExtImageCopyCaptureCursorSessionV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureCursorSessionV1,
        event: ext_cursor_session_v1::Event,
        &wl_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_cursor_session_v1::Event;
        match event {
            Event::Enter => {
                if let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.present = true;
                }
                state.start_cursor_cap_ext(wl_name, qh);
            }
            Event::Leave => {
                if let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.present = false;
                    cc.pos = None;
                }
            }
            Event::Position { x, y } => {
                // Convert output-local hotspot position to virtual-screen coords.
                let root = state.captures.get(&wl_name)
                    .map(|ctx| ((ctx.x + x) as i16, (ctx.y + y) as i16));
                if let Some((rx, ry)) = root {
                    state.server.cursor.update_position(rx, ry);
                }
                if let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.pos = Some((x, y));
                }
            }
            Event::Hotspot { x, y } => {
                if let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.hotspot = (x, y);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, CursorSessionUD> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_session_v1::Event,
        &CursorSessionUD(wl_name): &CursorSessionUD,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                if let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.cap_size = Some((width, height));
                }
            }
            Event::ShmFormat { format } => {
                if let WEnum::Value(fmt) = format
                    && let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    // Prefer Argb8888 (zero-conversion + alpha); else a
                    // convertible format with alpha (transparent cursors); else
                    // any convertible; only then an unconvertible format.
                    let rank = |f: wl_shm::Format| -> u8 {
                        if f == wl_shm::Format::Argb8888 {
                            3
                        } else {
                            match pixel_layout(f) {
                                Some((_, Some(_))) => 2,
                                Some((_, None)) => 1,
                                None => 0,
                            }
                        }
                    };
                    if cc.cap_format.is_none_or(|cur| rank(fmt) > rank(cur)) {
                        cc.cap_format = Some(fmt);
                    }
                }
            }
            Event::Done => {
                let ready = if let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    if cc.cap_format.is_none() {
                        crate::warning!(
                            "cursor session for output {wl_name} offered no shm format; skipping"
                        );
                        false
                    } else {
                        cc.cap_ready = true;
                        true
                    }
                } else {
                    false
                };
                if ready {
                    state.start_cursor_cap_ext(wl_name, qh);
                }
            }
            Event::Stopped => {
                crate::vlog!("cursor cap session stopped for output {wl_name}");
                if let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.cap_session = None;
                    cc.cap_ready = false;
                    cc.cap_size = None;
                    cc.cap_format = None;
                    cc.in_flight = false;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, CursorSessionUD> for State {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_frame_v1::Event,
        &CursorSessionUD(wl_name): &CursorSessionUD,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_frame_v1::Event;
        match event {
            Event::Damage { x, y, width, height } => {
                if let Some(ctx) = state.captures.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.frame_damage.push((x, y, width, height));
                }
            }
            Event::Transform { .. } | Event::PresentationTime { .. } => {}
            Event::Ready => state.cursor_frame_ready(wl_name, frame, qh),
            Event::Failed { .. } => state.cursor_frame_failed(wl_name, frame),
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
delegate_noop!(State: ignore ExtDataControlManagerV1);
delegate_noop!(State: ignore ZwlrDataControlManagerV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);
