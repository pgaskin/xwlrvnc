//! `ext-image-copy-capture-v1` capture backend.
//!
//! Each output gets a session that negotiates buffer size and format, then
//! delivers damage-tracked frames. The compositor says what changed, so unlike
//! the wlr path there is nothing to diff. The protocol also offers a per-output
//! [`cursor`] session, for capturing the cursor separately and reporting it over
//! XFixes — unless `-cursor none` bakes it into the screen frames instead.

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

/// One output's capture state, screen session and cursor session both.
struct Ctx {
    output: wl_output::WlOutput,
    x: i32,
    y: i32,
    buffer: Option<ShmBuffer>,
    /// The frame awaiting `ready`/`failed`. A session may only have one at a
    /// time (a second `create_frame` is a `duplicate_frame` protocol error), so
    /// this doubles as the in-flight guard and lets us tell our own frame's
    /// events from a stray one's.
    frame: Option<ExtImageCopyCaptureFrameV1>,
    /// Earliest the next capture may be requested.
    next_at: Instant,
    /// When the in-flight capture went out, for latency profiling.
    req_at: Option<Instant>,
    source: Option<ExtImageCaptureSourceV1>,
    session: Option<ExtImageCopyCaptureSessionV1>,
    /// Set once the session's `done` closes constraint negotiation.
    session_ready: bool,
    /// Geometry from the session's `buffer_size` event.
    session_size: Option<(u32, u32)>,
    /// Format picked from the session's `shm_format` events.
    session_format: Option<wl_shm::Format>,
    /// Rects from this frame's `damage` events, in buffer coordinates. Converted
    /// to root coordinates before they reach DAMAGE.
    frame_damage: Vec<(i32, i32, i32, i32)>,
    /// `None` when `-cursor none` bakes the cursor into the screen frames.
    cursor_cap: Option<CursorCap>,
}

pub(crate) struct ImageCopyCapture {
    server: Arc<Server>,
    shm: wl_shm::WlShm,
    source_mgr: ExtOutputImageCaptureSourceManagerV1,
    cap_mgr: ExtImageCopyCaptureManagerV1,
    /// Passive pointer for `create_pointer_cursor_session`, learned at seat select.
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
        // `-cursor none` composites the cursor into the screen frames, and we
        // then report a transparent XFixes cursor so the agent doesn't draw a
        // second one; otherwise it is captured separately and sent over XFixes
        let bake = self.server.config.cursor == crate::config::CursorType::Baked;
        let options = if bake {
            CaptureOptions::PaintCursors
        } else {
            CaptureOptions::empty()
        };
        let session = cap_mgr.create_session(&source, options, qh, wl_name);

        // one cursor session per output, so position events arrive from whichever
        // one the pointer is on
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
        // decide what to do without holding a mutable borrow
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
            if ctx.frame.is_some() {
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
                    ctx.frame = Some(frame);
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
            // no diff needed, the compositor hands us damage rects directly
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
        // compositor damage is buffer-local, so shift it into virtual-screen coords
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
            ctx.frame = None;
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
        // requeue at once so the compositor can hold the request until content
        // changes, which is this path's answer to the self-paced wlr loop
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
            ctx.frame = None;
            ctx.req_at = None;
            ctx.next_at = Instant::now() + SLOW_INTERVAL;
            // On buffer-constraints — the output resized, say — the session
            // re-sends its constraints in its own `done`, and all we need is a
            // buffer matching them, so drop the stale one and let the paced retry
            // rebuild it against the current `session_size`. Do NOT also clear
            // `session_ready`/`session_size`: that `done` may already have been
            // delivered, and we would wait forever for one that never comes.
            if matches!(
                reason,
                WEnum::Value(ext_frame_v1::FailureReason::BufferConstraints)
            ) {
                ctx.buffer = None;
            }
        }
        frame.destroy();
        // tick -> start_capture_ext rebuilds the buffer and retries
        let _ = qh;
    }

    /// Handles a screen session event, which is buffer size/format negotiation.
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
                    // prefer Xrgb8888, which needs no conversion, then anything
                    // byte-permutable, and only settle for a packed or float
                    // format if nothing better is offered
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
                // constraints are known, so kick off the first frame
                self.start_capture_ext(wl_name, qh);
            }
            Event::Stopped => {
                crate::vlog!("ext capture session stopped for output {wl_name}; will restart");
                if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
                    ctx.session = None;
                    ctx.session_ready = false;
                    ctx.session_size = None;
                    ctx.session_format = None;
                    ctx.frame = None;
                    ctx.next_at = Instant::now() + SLOW_INTERVAL;
                }
                // tick -> start_capture_ext will recreate the session
            }
            _ => {}
        }
    }

    /// Whether `frame` is the one this output is currently waiting on.
    fn is_current_frame(&self, wl_name: u32, frame: &ExtImageCopyCaptureFrameV1) -> bool {
        self.ctxs
            .get(&wl_name)
            .and_then(|c| c.frame.as_ref())
            .is_some_and(|f| f == frame)
    }

    /// Handles a screen frame event: the captured image, and its damage.
    fn screen_frame_event(
        &mut self,
        wl_name: u32,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_frame_v1::Event,
        qh: &QueueHandle<State>,
    ) {
        use ext_frame_v1::Event;
        // Only our own in-flight frame drives the state machine. Anything else is
        // a frame the compositor rejected as a duplicate, or a leftover from a
        // session we have already replaced; acting on it would clear `frame` and
        // re-arm us into creating another duplicate, forever.
        if !self.is_current_frame(wl_name, frame) {
            if let Event::Ready | Event::Failed { .. } = event {
                crate::vlog!("ignoring {event:?} for a stale capture frame on output {wl_name}");
                frame.destroy();
            }
            return;
        }
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
                    frame: None,
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
        // catch up any output without a session, which covers both new outputs
        // and managers that arrived after the outputs did
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
            .filter(|(_, c)| c.frame.is_none())
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
