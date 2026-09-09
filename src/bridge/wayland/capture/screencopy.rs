//! `zwlr-screencopy-v1` capture backend.
//!
//! Each output is captured with a plain `copy` into a wl_shm buffer. Plain
//! `copy` reports no damage, so we diff the result against the framebuffer to
//! work out what changed and to pace the next request.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_output;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use super::*;

/// One output's capture state.
struct Ctx {
    output: wl_output::WlOutput,
    x: i32,
    y: i32,
    /// The capture that is out, awaiting its `ready`/`failed`. Declared before
    /// `buffer` so a dropped context cancels the copy before destroying the
    /// buffer it copies into.
    frame: Option<ZwlrScreencopyFrameV1>,
    buffer: Option<ShmBuffer>,
    /// Earliest the next capture may be requested.
    next_at: Instant,
    /// When the in-flight capture went out, for latency profiling.
    req_at: Option<Instant>,
    /// Format from the `buffer` event, applied on `buffer_done`.
    pending: Option<(wl_shm::Format, u32, u32, u32)>,
    y_invert: bool,
    /// Adaptive interval: fast on change, backing off on unchanged frames.
    interval: Duration,
    /// `Framebuffer::last_read_ms` at the previous blit, for no-damage pacing.
    last_read_seen: u64,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        // The backend can be replaced (by ext) or an output removed with a
        // capture in flight; dropping the proxy alone would leave the frame
        // alive in the compositor, and its events unanswered.
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
    }
}

pub(crate) struct ScreencopyCapture {
    server: Arc<Server>,
    shm: wl_shm::WlShm,
    mgr: ZwlrScreencopyManagerV1,
    ctxs: HashMap<u32, Ctx>,
}

impl ScreencopyCapture {
    pub(super) fn new(
        server: Arc<Server>,
        shm: wl_shm::WlShm,
        mgr: ZwlrScreencopyManagerV1,
    ) -> Self {
        Self {
            server,
            shm,
            mgr,
            ctxs: HashMap::new(),
        }
    }

    fn start_capture(&mut self, wl_name: u32, qh: &QueueHandle<State>) {
        let Some(ctx) = self.ctxs.get_mut(&wl_name) else {
            return;
        };
        // overlay_cursor = 1 renders the cursor into the image so the remote can
        // see it; we then report a transparent XFixes cursor so the agent doesn't
        // draw a second one on top
        ctx.frame = Some(self.mgr.capture_output(1, &ctx.output, qh, wl_name));
        ctx.req_at = Some(Instant::now());
    }

    /// Creates the shm buffer if needed, then asks the compositor to copy.
    fn copy_frame(&mut self, wl_name: u32, frame: &ZwlrScreencopyFrameV1, qh: &QueueHandle<State>) {
        let Some(ctx) = self.ctxs.get_mut(&wl_name) else {
            return;
        };
        let Some((format, w, h, stride)) = ctx.pending.take() else {
            return;
        };
        let stale = ctx.buffer.as_ref().is_none_or(|b| {
            b.width != w || b.height != h || b.stride != stride || b.format != format
        });
        if stale {
            ctx.buffer = create_shm_buffer(&self.shm, qh, format, w, h, stride);
        }
        if let Some(buf) = &ctx.buffer {
            // always plain `copy`; blit_diff computes damage itself, and says why
            // copy_with_damage is not worth having
            frame.copy(&buf.buffer);
        }
    }

    /// Blits a completed capture, diffing to find what changed, reports it as
    /// DAMAGE, and schedules this output's next capture.
    fn frame_ready(
        &mut self,
        wl_name: u32,
        frame: &ZwlrScreencopyFrameV1,
        _qh: &QueueHandle<State>,
    ) {
        // only pay for the per-frame compare when a client actually holds a
        // DAMAGE object; otherwise just refresh the framebuffer for polled
        // GetImage/ShmGetImage reads
        let compute_damage = self.server.damage.active();
        let (mut bbox, mut blit_wait, mut blit_work) = (None, 0u64, 0u64);
        if let Some(ctx) = self.ctxs.get(&wl_name)
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
        let fast = fast_interval(&self.server);
        let now = Instant::now();
        if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
            ctx.frame = None;
            let req_latency = ctx
                .req_at
                .take()
                .map_or(0, |t| t.elapsed().as_nanos() as u64);
            // Pace to the rate the consumer actually wants, backing off
            // geometrically toward the slow rate. With damage the signal is a
            // pixel change; without it we can't tell what changed (that's the
            // compare we deliberately skipped), so the signal is whether the
            // client read the last frame — otherwise we'd blit 25MB frames nobody
            // pulls. The read gate then stops capture entirely.
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
            crate::bridge::profile::frame(blit_wait, blit_work, req_latency);
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

    fn frame_failed(
        &mut self,
        wl_name: u32,
        frame: &ZwlrScreencopyFrameV1,
        _qh: &QueueHandle<State>,
    ) {
        crate::warning!("capture failed for output {wl_name}");
        if let Some(ctx) = self.ctxs.get_mut(&wl_name) {
            ctx.frame = None;
            ctx.req_at = None;
            ctx.next_at = Instant::now() + SLOW_INTERVAL; // back off, then retry
        }
        frame.destroy();
    }

    /// Handles one `zwlr_screencopy_frame_v1` event.
    fn frame_event(
        &mut self,
        wl_name: u32,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        qh: &QueueHandle<State>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        // Only the frame this output is waiting on drives its state; anything
        // else is a leftover from a replaced context, which is only ever
        // destroyed here if it was not already.
        let current = self
            .ctxs
            .get(&wl_name)
            .and_then(|c| c.frame.as_ref())
            .is_some_and(|f| f == frame);
        if !current {
            if let Event::Ready { .. } | Event::Failed = event {
                crate::vlog!("ignoring {event:?} for a stale capture frame on output {wl_name}");
                frame.destroy();
            }
            return;
        }
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if let WEnum::Value(format) = format
                    && let Some(ctx) = self.ctxs.get_mut(&wl_name)
                {
                    ctx.pending = Some((format, width, height, stride));
                    if frame.version() < 3 {
                        self.copy_frame(wl_name, frame, qh);
                    }
                }
            }
            Event::BufferDone => self.copy_frame(wl_name, frame, qh),
            Event::Flags { flags } => {
                if let WEnum::Value(flags) = flags
                    && let Some(ctx) = self.ctxs.get_mut(&wl_name)
                {
                    ctx.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            // plain `copy` sends no Damage events, so there is nothing to collect
            Event::Ready { .. } => self.frame_ready(wl_name, frame, qh),
            Event::Failed => self.frame_failed(wl_name, frame, qh),
            _ => {}
        }
    }
}

impl CaptureBackend for ScreencopyCapture {
    fn sync_outputs(&mut self, outputs: &HashMap<u32, OutputAcc>, _qh: &QueueHandle<State>) {
        let now = Instant::now();
        let fast = fast_interval(&self.server);
        let new: Vec<(u32, wl_output::WlOutput)> = outputs
            .iter()
            .filter(|(n, acc)| acc.proxy.is_some() && !self.ctxs.contains_key(n))
            .map(|(n, acc)| (*n, acc.proxy.clone().unwrap()))
            .collect();
        if !new.is_empty() && self.ctxs.is_empty() {
            crate::log!("using zwlr-screencopy-v1 for screen capture");
        }
        for (name, output) in new {
            // the physical position, if the output has synced; otherwise
            // apply_output corrects it on the first wl_output `done`
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
                    frame: None,
                    buffer: None,
                    next_at: now,
                    req_at: None,
                    pending: None,
                    y_invert: false,
                    interval: fast,
                    last_read_seen: 0,
                },
            );
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
            self.start_capture(name, qh);
        }
        wait
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
        if let Capture::Screencopy(cap) = &mut state.capture {
            cap.frame_event(wl_name, frame, event, qh);
        }
    }
}
