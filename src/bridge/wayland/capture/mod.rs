//! Screen and cursor capture, over whichever of the two protocols the
//! compositor offers.
//!
//! [`zwlr-screencopy-v1`](screencopy) and
//! [`ext-image-copy-capture-v1`](imagecopy) both feed frames into the shared
//! [`Framebuffer`](crate::bridge::capture::Framebuffer) and report changes to
//! DAMAGE, differing only in how a frame is asked for and how damage is learned.
//! The shared lifecycle is the [`CaptureBackend`] trait, [`State`] holds the
//! negotiated one as a [`Capture`], and each submodule's `Dispatch` impls match
//! their own variant.
//!
//! Capture is self-paced: [`tick`](CaptureBackend::tick) issues what is due and
//! says how long to wait before the next one.

use std::collections::HashMap;
use std::time::Duration;

use super::*;

mod imagecopy;
mod screencopy;

pub(crate) use imagecopy::ImageCopyCapture;
pub(crate) use screencopy::ScreencopyCapture;

/// Capture pacing, for the self-paced plain `copy` requests we drive instead of
/// `copy_with_damage` (see
/// [`blit_diff`](crate::bridge::capture::Framebuffer::blit_diff) for why).
///
/// Each output's interval tracks its actual change rate: a changed frame resets
/// to `FAST_INTERVAL`, an unchanged one grows geometrically toward
/// `SLOW_INTERVAL`. Continuous content therefore runs at the fast cap, sparse
/// change (a blinking caret, a clock) backs off to a few fps, and a still screen
/// reaches the slow rate. Since every capture costs a full-frame diff, not
/// capturing faster than the screen changes is the whole point.
///
/// 30fps is deliberate: vncagent only consumes ~20 reads/sec, so anything faster
/// is work the client never sees.
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

    pub(super) fn set_position(&mut self, wl_name: u32, x: i32, y: i32) {
        if let Some(b) = self.active_mut() {
            b.set_position(wl_name, x, y);
        }
    }

    pub(super) fn remove_output(&mut self, wl_name: u32) {
        if let Some(b) = self.active_mut() {
            b.remove_output(wl_name);
        }
    }

    pub(super) fn positions(
        &self,
        screen: &crate::bridge::x11::randr::Screen,
    ) -> Vec<(u32, (i32, i32))> {
        self.active()
            .map(|b| b.positions(screen))
            .unwrap_or_default()
    }

    /// Records the seat pointer so the ext backend can open cursor sessions,
    /// including for outputs whose screen session already exists.
    pub(super) fn set_pointer(&mut self, pointer: wl_pointer::WlPointer, qh: &QueueHandle<State>) {
        if let Capture::ImageCopy(c) = self {
            c.set_pointer(pointer, qh);
        }
    }
}

/// What both capture protocols have in common.
pub(crate) trait CaptureBackend {
    /// Ensures a capture context exists for every output with a bound proxy.
    fn sync_outputs(&mut self, outputs: &HashMap<u32, OutputAcc>, qh: &QueueHandle<State>);
    /// Updates an output's physical (virtual-screen) position.
    fn set_position(&mut self, wl_name: u32, x: i32, y: i32);
    /// Drops an output's capture context.
    fn remove_output(&mut self, wl_name: u32);
    /// Each tracked output's physical position from the current layout.
    fn positions(&self, screen: &crate::bridge::x11::randr::Screen) -> Vec<(u32, (i32, i32))>;
    /// Issues any due captures, returning how long to wait before ticking again.
    fn tick(&mut self, qh: &QueueHandle<State>) -> Duration;
}

/// Whether a client is watching the screen, by holding a DAMAGE object or having
/// read pixels recently. Nothing is captured otherwise.
fn capture_enabled(server: &Server) -> bool {
    server.damage.active() || server.framebuffer.read_within(READ_GATE_MS)
}

/// The fast-cap interval, honouring `-fps`.
fn fast_interval(server: &Server) -> Duration {
    server
        .config
        .fps
        .map(|f| Duration::from_millis(1000 / u64::from(f.max(1))))
        .unwrap_or(FAST_INTERVAL)
}

impl State {
    /// Ensures the preferred backend exists, and a context per output: ext once
    /// shm and both its managers are bound, else screencopy, and a screencopy
    /// backend is replaced if the ext managers turn up later. Nothing is chosen
    /// until the initial registry burst has settled, so the usual case picks
    /// once with every global known. The requests themselves are issued, paced,
    /// by [`tick_captures`](Self::tick_captures).
    pub(super) fn maybe_start_captures(&mut self, qh: &QueueHandle<Self>) {
        if !self.registry_settled {
            return;
        }
        let has_wlr = self.shm.is_some() && self.screencopy.is_some();
        let has_ext =
            self.shm.is_some() && self.ext_source_mgr.is_some() && self.ext_capture_mgr.is_some();
        if !has_wlr && !has_ext {
            return;
        }
        let upgrade = has_ext && matches!(self.capture, Capture::Screencopy(_));
        if upgrade {
            crate::vlog!("switching to ext-image-copy-capture-v1");
        }
        if matches!(self.capture, Capture::None) || upgrade {
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

    /// Issues capture requests for any outputs that are due, returning how long
    /// the event loop should wait before ticking again.
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
