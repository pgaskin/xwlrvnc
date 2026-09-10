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
    /// The output's `wl_output` transform, the fallback for frames that carry
    /// none.
    transform: Transform,
    /// From the frame's `transform` event, the same value in the same sense
    /// as `wl_output` sends (see [`Transform`]); wins over `transform`.
    frame_transform: Option<Transform>,
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
    /// `None` when `-cursor none` bakes the cursor into the screen frames, or
    /// while a stopped cursor session waits to be reopened.
    cursor_cap: Option<CursorCap>,
    /// When to reopen the cursor session after the compositor stopped it.
    cursor_reopen_at: Option<Instant>,
}

impl Ctx {
    /// The transform to apply to this output's capture buffer.
    fn transform(&self) -> Transform {
        self.frame_transform.unwrap_or(self.transform)
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        // Dropping the proxies does not destroy the objects; without this an
        // output removal (or a replaced backend) leaves the session running in
        // the compositor. Frame first, since it belongs to the session.
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        if let Some(session) = self.session.take() {
            session.destroy();
        }
        if let Some(source) = self.source.take() {
            source.destroy();
        }
    }
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

    /// Records the seat pointer, and opens a cursor session for every output
    /// whose screen session was created before the pointer was known (the seat
    /// can arrive after the capture managers and outputs, and with `-seat NAME`
    /// it always does).
    pub(super) fn set_pointer(&mut self, pointer: wl_pointer::WlPointer, qh: &QueueHandle<State>) {
        self.pointer = Some(pointer);
        if self.bake_cursor() {
            return;
        }
        let cap_mgr = self.cap_mgr.clone();
        let Some(ptr) = &self.pointer else { return };
        for (&wl_name, ctx) in self.ctxs.iter_mut() {
            if ctx.cursor_cap.is_none()
                && let Some(source) = &ctx.source
            {
                ctx.cursor_cap = Some(CursorCap::open(&cap_mgr, source, ptr, qh, wl_name));
                ctx.cursor_reopen_at = None;
            }
        }
    }

    /// Whether `-cursor none` composites the cursor into the screen frames (with
    /// a transparent XFixes cursor so the agent doesn't draw a second one),
    /// rather than capturing it separately and sending it over XFixes.
    fn bake_cursor(&self) -> bool {
        self.server.config.cursor == crate::config::CursorType::Baked
    }

    fn create_ext_session(&mut self, wl_name: u32, qh: &QueueHandle<State>) {
        let source_mgr = self.source_mgr.clone();
        let cap_mgr = self.cap_mgr.clone();
        let Some(output) = self.ctxs.get(&wl_name).map(|c| c.output.clone()) else {
            return;
        };
        let source = source_mgr.create_source(&output, qh, ());
        let bake = self.bake_cursor();
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
            // a restart after `stopped` replaces the source and session; the old
            // cursor session goes with its `CursorCap` drop below
            if let Some(old) = ctx.session.replace(session) {
                old.destroy();
            }
            if let Some(old) = ctx.source.replace(source) {
                old.destroy();
            }
            ctx.session_ready = false;
            ctx.session_size = None;
            ctx.session_format = None;
            ctx.frame_transform = None;
            ctx.cursor_cap = cursor_cap;
            ctx.cursor_reopen_at = None;
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
        let mut compositor_rects: Vec<Rectangle> = Vec::new();
        if let Some(ctx) = self.ctxs.get(&wl_name)
            && let Some(b) = &ctx.buffer
        {
            let (px, py) = physical_pos(&self.server, wl_name);
            let transform = ctx.transform();
            let slice = unsafe { std::slice::from_raw_parts(b.map, b.size) };
            // no diff needed, the compositor hands us damage rects directly
            let (_, w, wk) = self.server.framebuffer.blit_diff(
                px,
                py,
                b.width,
                b.height,
                b.stride,
                slice,
                false,
                transform,
                false,
                blit_channels(wl_name, b.format),
            );
            blit_wait = w;
            blit_work = wk;
            // compositor damage is buffer-local: it goes through the same
            // transform as the pixels, then shifts into virtual-screen coords
            if compute_damage {
                let (bw, bh) = (b.width as i32, b.height as i32);
                compositor_rects = ctx
                    .frame_damage
                    .iter()
                    .map(|&r| {
                        let (x, y, w, h) = transform.rect(r, bw, bh);
                        Rectangle {
                            x: (px + x) as i16,
                            y: (py + y) as i16,
                            width: w.max(0) as u16,
                            height: h.max(0) as u16,
                        }
                    })
                    .collect();
            }
        }
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
                            "ext session for output {wl_name} offered no shm format; \
                             skipping (try -screen screencopy)"
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
                    // a stopped session is dead, and the protocol expects us to
                    // destroy it (and any frame on it) rather than leave it
                    if let Some(frame) = ctx.frame.take() {
                        frame.destroy();
                    }
                    if let Some(session) = ctx.session.take() {
                        session.destroy();
                    }
                    ctx.session_ready = false;
                    ctx.session_size = None;
                    ctx.session_format = None;
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
            Event::Transform { transform } => {
                // the value wl_output sends for the output, in the same sense
                // (checked against sway); Transform's mapping undoes it
                if let WEnum::Value(t) = transform
                    && let Some(ctx) = self.ctxs.get_mut(&wl_name)
                {
                    ctx.frame_transform = Some(t.into());
                }
            }
            Event::PresentationTime { .. } => {}
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
            self.ctxs.insert(
                name,
                Ctx {
                    output,
                    transform: outputs
                        .get(&name)
                        .map_or(Transform::Normal, |a| a.transform),
                    frame_transform: None,
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
                    cursor_reopen_at: None,
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

    fn set_transform(&mut self, wl_name: u32, transform: Transform) {
        if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
            ctx.transform = transform;
        }
    }

    fn remove_output(&mut self, wl_name: u32) {
        self.ctxs.remove(&wl_name);
    }

    fn tick(&mut self, qh: &QueueHandle<State>) -> Duration {
        self.tick_cursors(qh);
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
