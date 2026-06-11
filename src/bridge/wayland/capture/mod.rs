//! Screen (and cursor) capture, abstracted over the two compositor protocols.
//!
//! Both [`zwlr-screencopy-v1`](screencopy) and
//! [`ext-image-copy-capture-v1`](imagecopy) feed captured frames into the shared
//! [`Framebuffer`](crate::bridge::capture::Framebuffer) and report changes to XDAMAGE;
//! they differ only in how a frame is requested and how damage is learned. The
//! shared lifecycle (per-output context creation, layout repositioning, paced
//! ticking) is the [`CaptureBackend`] trait; [`State`] holds whichever concrete
//! backend was negotiated as a [`Capture`] and the per-protocol `Dispatch` impls
//! (in the submodules) match their own variant directly.
//!
//! Capture is self-paced: [`tick`](CaptureBackend::tick) issues any due captures
//! and returns how long the event loop should wait before ticking again. See the
//! pacing constants below and [`crate::bridge::capture::Framebuffer::blit_diff`] for why
//! we drive plain `copy` rather than `copy_with_damage`.

use std::collections::HashMap;
use std::time::Duration;

use super::*;

mod imagecopy;
mod screencopy;

pub(crate) use imagecopy::ImageCopyCapture;
pub(crate) use screencopy::ScreencopyCapture;

/// Capture pacing. We self-pace plain `copy` requests rather than rely on
/// `copy_with_damage` (see [`crate::bridge::capture::Framebuffer::blit_diff`]).
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

/// The negotiated screen-capture backend (only one is ever active).
pub(crate) enum Capture {
    None,
    Screencopy(ScreencopyCapture),
    ImageCopy(ImageCopyCapture),
}

impl Capture {
    fn active(&self) -> Option<&dyn CaptureBackend> {
        match self {
            Capture::None => None,
            Capture::Screencopy(c) => Some(c),
            Capture::ImageCopy(c) => Some(c),
        }
    }

    fn active_mut(&mut self) -> Option<&mut dyn CaptureBackend> {
        match self {
            Capture::None => None,
            Capture::Screencopy(c) => Some(c),
            Capture::ImageCopy(c) => Some(c),
        }
    }

    pub(super) fn is_active(&self) -> bool {
        !matches!(self, Capture::None)
    }

    /// Updates an output's physical (virtual-screen) position.
    pub(super) fn set_position(&mut self, wl_name: u32, x: i32, y: i32) {
        if let Some(b) = self.active_mut() {
            b.set_position(wl_name, x, y);
        }
    }

    /// Drops an output's capture context.
    pub(super) fn remove_output(&mut self, wl_name: u32) {
        if let Some(b) = self.active_mut() {
            b.remove_output(wl_name);
        }
    }

    /// Each tracked output's physical position from the current layout.
    pub(super) fn positions(
        &self,
        screen: &crate::bridge::x11::randr::Screen,
    ) -> Vec<(u32, (i32, i32))> {
        self.active()
            .map(|b| b.positions(screen))
            .unwrap_or_default()
    }

    /// Records the seat pointer so the ext backend can open cursor sessions.
    pub(super) fn set_pointer(&mut self, pointer: wl_pointer::WlPointer) {
        if let Capture::ImageCopy(c) = self {
            c.set_pointer(pointer);
        }
    }
}

/// Behaviour shared by both capture protocols.
pub(crate) trait CaptureBackend {
    /// Ensures a capture context exists for every output with a bound proxy.
    fn sync_outputs(&mut self, outputs: &HashMap<u32, OutputAcc>, qh: &QueueHandle<State>);
    /// Updates an output's physical (virtual-screen) position.
    fn set_position(&mut self, wl_name: u32, x: i32, y: i32);
    /// Drops an output's capture context.
    fn remove_output(&mut self, wl_name: u32);
    /// Each tracked output's physical position from the current layout.
    fn positions(&self, screen: &crate::bridge::x11::randr::Screen) -> Vec<(u32, (i32, i32))>;
    /// Issues any due captures; returns how long to wait before ticking again.
    fn tick(&mut self, qh: &QueueHandle<State>) -> Duration;
}

/// Whether any client is currently watching the screen (has a DAMAGE object or
/// read pixels recently). We don't capture otherwise.
fn capture_enabled(server: &Server) -> bool {
    server.damage.active() || server.framebuffer.read_within(READ_GATE_MS)
}

/// The capture interval at the fast cap, honouring `-fps` (default ~30fps).
fn fast_interval(server: &Server) -> Duration {
    server
        .config
        .fps
        .map(|f| Duration::from_millis(1000 / u64::from(f.max(1))))
        .unwrap_or(FAST_INTERVAL)
}

impl State {
    /// Ensures a capture backend exists (negotiating ext over wlr once shm and a
    /// screencopy protocol are available) and a capture context per output.
    /// Actual capture requests are issued, paced, by [`tick_captures`](Self::tick_captures).
    pub(super) fn maybe_start_captures(&mut self, qh: &QueueHandle<Self>) {
        let has_wlr = self.shm.is_some() && self.screencopy.is_some();
        let has_ext =
            self.shm.is_some() && self.ext_source_mgr.is_some() && self.ext_capture_mgr.is_some();
        if !has_wlr && !has_ext {
            return;
        }
        if matches!(self.capture, Capture::None) {
            let Some(shm) = self.shm.clone() else { return };
            self.capture = if has_ext {
                Capture::ImageCopy(ImageCopyCapture::new(
                    self.server.clone(),
                    shm,
                    self.ext_source_mgr.clone().unwrap(),
                    self.ext_capture_mgr.clone().unwrap(),
                    self.pointer.clone(),
                ))
            } else {
                Capture::Screencopy(ScreencopyCapture::new(
                    self.server.clone(),
                    shm,
                    self.screencopy.clone().unwrap(),
                ))
            };
        }
        if let Some(b) = self.capture.active_mut() {
            b.sync_outputs(&self.outputs, qh);
        }
    }

    /// Issues capture requests for any outputs that are due, and returns how long
    /// to wait before the loop should tick again. Called once per loop iteration.
    pub(super) fn tick_captures(&mut self, qh: &QueueHandle<Self>) -> Duration {
        let enabled = self.capture.is_active() && capture_enabled(&self.server);
        if enabled != self.capture_active {
            self.capture_active = enabled;
            crate::vlog!(
                "screen capture {}",
                if enabled { "started" } else { "stopped" }
            );
        }
        if !enabled {
            return DISABLED_INTERVAL;
        }
        match self.capture.active_mut() {
            Some(b) => b.tick(qh),
            None => DISABLED_INTERVAL,
        }
    }
}
