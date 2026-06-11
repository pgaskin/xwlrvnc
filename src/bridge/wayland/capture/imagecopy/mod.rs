//! `ext-image-copy-capture-v1` capture backend.
//!
//! Each output gets a capture session that negotiates buffer size/format and
//! then delivers damage-tracked frames; the compositor tells us what changed, so
//! (unlike the wlr path) we don't diff. This protocol also exposes a per-output
//! cursor session, used to capture the cursor image separately and report it via
//! XFixes (unless `-cursor none` bakes it into the screen frames instead).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_output, wl_pointer};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self as ext_frame_v1, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options as CaptureOptions,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self as ext_session_v1, ExtImageCopyCaptureSessionV1,
};
use x11rb_protocol::protocol::xproto::Rectangle;

use super::*;

mod cursor;
use cursor::CursorCap;

/// Per-output capture state for the ext path.
struct Ctx {
    output: wl_output::WlOutput,
    x: i32,
    y: i32,
    buffer: Option<ShmBuffer>,
    /// A capture has been requested and we're awaiting its `ready`/`failed`.
    in_flight: bool,
    /// Earliest time the next capture for this output should be requested.
    next_at: Instant,
    /// When the in-flight capture was requested (latency profiling).
    req_at: Option<Instant>,
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
    /// Per-output cursor capture state.
    cursor_cap: Option<CursorCap>,
}

pub(crate) struct ImageCopyCapture {
    server: Arc<Server>,
    shm: wl_shm::WlShm,
    source_mgr: ExtOutputImageCaptureSourceManagerV1,
    cap_mgr: ExtImageCopyCaptureManagerV1,
    /// Passive pointer for `create_pointer_cursor_session`; learned at seat select.
    pointer: Option<wl_pointer::WlPointer>,
    ctxs: HashMap<u32, Ctx>,
}

impl ImageCopyCapture {
    pub(super) fn new(
        server: Arc<Server>,
        shm: wl_shm::WlShm,
        source_mgr: ExtOutputImageCaptureSourceManagerV1,
        cap_mgr: ExtImageCopyCaptureManagerV1,
        pointer: Option<wl_pointer::WlPointer>,
    ) -> Self {
        Self {
            server,
            shm,
            source_mgr,
            cap_mgr,
            pointer,
            ctxs: HashMap::new(),
        }
    }

    pub(super) fn set_pointer(&mut self, pointer: wl_pointer::WlPointer) {
        self.pointer = Some(pointer);
    }

    fn create_ext_session(&mut self, wl_name: u32, qh: &QueueHandle<State>) {
        let source_mgr = self.source_mgr.clone();
        let cap_mgr = self.cap_mgr.clone();
        let Some(output) = self.ctxs.get(&wl_name).map(|c| c.output.clone()) else {
            return;
        };
        let source = source_mgr.create_source(&output, qh, ());
        // `-cursor none` composites the cursor into the screen frames (we then
        // report a transparent XFixes cursor so the agent doesn't double-draw);
        // otherwise it's captured separately and delivered via XFixes.
        let bake = self.server.config.cursor == crate::config::CursorType::Baked;
        let options = if bake {
            CaptureOptions::PaintCursors
        } else {
            CaptureOptions::empty()
        };
        let session = cap_mgr.create_session(&source, options, qh, wl_name);

        // Cursor session: one per output so we get position events for whichever
        // output the pointer is currently on. Skipped when baking.
        let cursor_cap = self
            .pointer
            .as_ref()
            .filter(|_| !bake)
            .map(|ptr| CursorCap::open(&cap_mgr, &source, ptr, qh, wl_name));

        if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
            ctx.source = Some(source);
            ctx.session = Some(session);
            ctx.session_ready = false;
            ctx.session_size = None;
            ctx.session_format = None;
            ctx.cursor_cap = cursor_cap;
            ctx.next_at = Instant::now() + SLOW_INTERVAL; // wait for Done events
        }
    }

    fn start_capture_ext(&mut self, wl_name: u32, qh: &QueueHandle<State>) {
        // Determine the action without holding a mutable borrow.
        enum Action {
            NeedSession,
            WaitConstraints,
            Capture {
                session: ExtImageCopyCaptureSessionV1,
                w: u32,
                h: u32,
                fmt: wl_shm::Format,
            },
        }
        let action = {
            let Some(ctx) = self.ctxs.get(&wl_name) else {
                return;
            };
            if ctx.in_flight {
                return;
            }
            if ctx.session.is_none() {
                Action::NeedSession
            } else if !ctx.session_ready {
                Action::WaitConstraints
            } else {
                match (ctx.session.clone(), ctx.session_size, ctx.session_format) {
                    (Some(s), Some((w, h)), Some(fmt)) => Action::Capture {
                        session: s,
                        w,
                        h,
                        fmt,
                    },
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
                if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
                    ctx.next_at = Instant::now() + SLOW_INTERVAL;
                }
            }
            Action::Capture { session, w, h, fmt } => {
                let shm = self.shm.clone();
                let stride = w * 4; // Xrgb8888: 4 bytes per pixel
                let ctx = self.ctxs.get_mut(&wl_name).unwrap();
                let stale = ctx
                    .buffer
                    .as_ref()
                    .is_none_or(|b| b.width != w || b.height != h);
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

    fn frame_ready(
        &mut self,
        wl_name: u32,
        frame: &ExtImageCopyCaptureFrameV1,
        qh: &QueueHandle<State>,
    ) {
        let compute_damage = self.server.damage.active();
        let (mut blit_wait, mut blit_work) = (0u64, 0u64);
        if let Some(ctx) = self.ctxs.get(&wl_name)
            && let Some(b) = &ctx.buffer
        {
            let slice = unsafe { std::slice::from_raw_parts(b.map, b.size) };
            // Blit the frame into the shared framebuffer; skip the diff since
            // the compositor gives us damage rects directly.
            let (_, w, wk) = self.server.framebuffer.blit_diff(
                ctx.x,
                ctx.y,
                b.width,
                b.height,
                b.stride,
                slice,
                false,
                false,
                blit_channels(wl_name, b.format),
            );
            blit_wait = w;
            blit_work = wk;
        }
        // Convert compositor damage (buffer-local coords) to virtual-screen coords.
        let compositor_rects: Vec<Rectangle> = if compute_damage {
            self.ctxs.get(&wl_name).map_or(Vec::new(), |ctx| {
                ctx.frame_damage
                    .iter()
                    .map(|&(x, y, w, h)| Rectangle {
                        x: ctx.x as i16 + x as i16,
                        y: ctx.y as i16 + y as i16,
                        width: w.max(0) as u16,
                        height: h.max(0) as u16,
                    })
                    .collect()
            })
        } else {
            Vec::new()
        };
        let now = Instant::now();
        if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
            ctx.in_flight = false;
            ctx.frame_damage.clear();
            let req_latency = ctx
                .req_at
                .take()
                .map_or(0, |t| t.elapsed().as_nanos() as u64);
            ctx.next_at = now; // immediately due for the next tick
            crate::bridge::profile::frame(blit_wait, blit_work, req_latency);
        }
        if compute_damage && !compositor_rects.is_empty() {
            let geom = {
                let s = self.server.screen.lock().unwrap();
                (s.width, s.height)
            };
            self.server.damage.add_damage(&compositor_rects, geom);
        }
        frame.destroy();
        // Immediately requeue so the compositor can hold the next frame request
        // until content changes (the equivalent of our self-paced wlr loop).
        if capture_enabled(&self.server) {
            self.start_capture_ext(wl_name, qh);
        }
    }

    fn frame_failed(
        &mut self,
        wl_name: u32,
        reason: WEnum<ext_frame_v1::FailureReason>,
        frame: &ExtImageCopyCaptureFrameV1,
        qh: &QueueHandle<State>,
    ) {
        let reason_str = match reason {
            WEnum::Value(ext_frame_v1::FailureReason::BufferConstraints) => "buffer-constraints",
            WEnum::Value(ext_frame_v1::FailureReason::Stopped) => "session-stopped",
            _ => "unknown",
        };
        crate::vlog!("ext capture frame failed ({reason_str}) for output {wl_name}");
        if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
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
            if matches!(
                reason,
                WEnum::Value(ext_frame_v1::FailureReason::BufferConstraints)
            ) {
                ctx.buffer = None;
            }
        }
        frame.destroy();
        // tick → start_capture_ext recreates the buffer and retries.
        let _ = qh;
    }

    /// Handles a screen capture session event (buffer-size/format negotiation).
    fn screen_session_event(
        &mut self,
        wl_name: u32,
        event: ext_session_v1::Event,
        qh: &QueueHandle<State>,
    ) {
        use ext_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
                    ctx.session_size = Some((width, height));
                }
            }
            Event::ShmFormat { format } => {
                if let WEnum::Value(fmt) = format
                    && let Some(ctx) = self.ctxs.get_mut(&wl_name)
                {
                    // Prefer Xrgb8888 (zero-conversion); otherwise the first
                    // byte-permutable format we can convert; only fall back to an
                    // unconvertible (packed/float) format if nothing better is
                    // offered.
                    let better = match ctx.session_format {
                        None => true,
                        Some(wl_shm::Format::Xrgb8888) => false,
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
                if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
                    let Some(fmt) = ctx.session_format else {
                        crate::warning!(
                            "ext session for output {wl_name} offered no shm format; skipping"
                        );
                        return;
                    };
                    crate::log!("ext capture output {wl_name} using shm format {fmt:?}");
                    ctx.session_ready = true;
                }
                // Kick off the first frame now that constraints are known.
                self.start_capture_ext(wl_name, qh);
            }
            Event::Stopped => {
                crate::vlog!("ext capture session stopped for output {wl_name}; will restart");
                if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
                    ctx.session = None;
                    ctx.session_ready = false;
                    ctx.session_size = None;
                    ctx.session_format = None;
                    ctx.in_flight = false;
                    ctx.next_at = Instant::now() + SLOW_INTERVAL;
                }
                // tick → start_capture_ext will recreate the session.
            }
            _ => {}
        }
    }

    /// Handles a screen capture frame event (the captured screen image + damage).
    fn screen_frame_event(
        &mut self,
        wl_name: u32,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_frame_v1::Event,
        qh: &QueueHandle<State>,
    ) {
        use ext_frame_v1::Event;
        match event {
            Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
                    ctx.frame_damage.push((x, y, width, height));
                }
            }
            Event::Transform { .. } | Event::PresentationTime { .. } => {}
            Event::Ready => self.frame_ready(wl_name, frame, qh),
            Event::Failed { reason } => self.frame_failed(wl_name, reason, frame, qh),
            _ => {}
        }
    }
}

impl CaptureBackend for ImageCopyCapture {
    fn sync_outputs(&mut self, outputs: &HashMap<u32, OutputAcc>, qh: &QueueHandle<State>) {
        let now = Instant::now();
        let new: Vec<(u32, wl_output::WlOutput)> = outputs
            .iter()
            .filter(|(n, acc)| acc.proxy.is_some() && !self.ctxs.contains_key(n))
            .map(|(n, acc)| (*n, acc.proxy.clone().unwrap()))
            .collect();
        if !new.is_empty() && self.ctxs.is_empty() {
            crate::log!("using ext-image-copy-capture-v1 for screen capture");
        }
        for (name, output) in new {
            let (x, y) = self
                .server
                .screen
                .lock()
                .unwrap()
                .physical_pos(name)
                .unwrap_or((0, 0));
            self.ctxs.insert(
                name,
                Ctx {
                    output,
                    x,
                    y,
                    buffer: None,
                    in_flight: false,
                    next_at: now,
                    req_at: None,
                    source: None,
                    session: None,
                    session_ready: false,
                    session_size: None,
                    session_format: None,
                    frame_damage: Vec::new(),
                    cursor_cap: None,
                },
            );
        }
        // Create sessions for any output that doesn't have one yet. This handles
        // both new outputs and the case where ext managers arrive after outputs
        // were already registered.
        let needs_session: Vec<u32> = self
            .ctxs
            .iter()
            .filter(|(_, c)| c.session.is_none())
            .map(|(n, _)| *n)
            .collect();
        for name in needs_session {
            self.create_ext_session(name, qh);
        }
    }

    fn set_position(&mut self, wl_name: u32, x: i32, y: i32) {
        if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
            ctx.x = x;
            ctx.y = y;
        }
    }

    fn remove_output(&mut self, wl_name: u32) {
        self.ctxs.remove(&wl_name);
    }

    fn positions(&self, screen: &crate::bridge::x11::randr::Screen) -> Vec<(u32, (i32, i32))> {
        self.ctxs
            .keys()
            .filter_map(|&n| screen.physical_pos(n).map(|p| (n, p)))
            .collect()
    }

    fn tick(&mut self, qh: &QueueHandle<State>) -> Duration {
        let now = Instant::now();
        let mut wait = SLOW_INTERVAL;
        let due: Vec<u32> = self
            .ctxs
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
            self.start_capture_ext(name, qh);
        }
        wait
    }
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
        if let Capture::ImageCopy(cap) = &mut state.capture {
            cap.screen_session_event(wl_name, event, qh);
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
        if let Capture::ImageCopy(cap) = &mut state.capture {
            cap.screen_frame_event(wl_name, frame, event, qh);
        }
    }
}
