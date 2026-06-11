//! Cursor capture for the ext-image-copy backend.
//!
//! Each output gets a `pointer_cursor_session` (created alongside its screen
//! session in [`ImageCopyCapture::create_ext_session`](super::ImageCopyCapture))
//! that reports the pointer's position/hotspot and a separate image stream for
//! the cursor itself, which we convert to an X ARGB cursor and publish via
//! XFixes. Skipped entirely when `-cursor none` bakes the cursor into the screen
//! frames instead.

use wayland_client::protocol::{wl_pointer, wl_shm};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_cursor_session_v1::{
    self as ext_cursor_session_v1, ExtImageCopyCaptureCursorSessionV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    self as ext_frame_v1, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    self as ext_session_v1, ExtImageCopyCaptureSessionV1,
};

use super::*;

/// Cursor image capture for one output.
///
/// The cursor image is identical on all outputs; only the position changes.
/// Each output's cursor session contributes position updates when the pointer
/// is on that output (signalled by `enter`/`leave`).
pub(super) struct CursorCap {
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

impl CursorCap {
    /// Opens a cursor session against an output's capture `source`, plus its
    /// image-capture sub-session.
    pub(super) fn open(
        cap_mgr: &ExtImageCopyCaptureManagerV1,
        source: &ExtImageCaptureSourceV1,
        pointer: &wl_pointer::WlPointer,
        qh: &QueueHandle<State>,
        wl_name: u32,
    ) -> Self {
        let cursor_session = cap_mgr.create_pointer_cursor_session(source, pointer, qh, wl_name);
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
    }
}

/// User-data marker distinguishing cursor capture sessions/frames from screen
/// capture sessions/frames (both use the same `ExtImageCopyCaptureSessionV1` /
/// `ExtImageCopyCaptureFrameV1` Wayland type).
struct CursorSessionUD(u32); // inner = wl_name

impl ImageCopyCapture {
    fn start_cursor_cap_ext(&mut self, wl_name: u32, qh: &QueueHandle<State>) {
        let (cap_session, w, h, fmt) = {
            let Some(ctx) = self.ctxs.get(&wl_name) else {
                return;
            };
            let Some(cc) = &ctx.cursor_cap else { return };
            if cc.in_flight || !cc.present || !cc.cap_ready {
                return;
            }
            match (cc.cap_session.clone(), cc.cap_size, cc.cap_format) {
                (Some(s), Some((w, h)), Some(fmt)) => (s, w, h, fmt),
                _ => return,
            }
        };
        let shm = self.shm.clone();
        let stride = w * 4;
        let ctx = self.ctxs.get_mut(&wl_name).unwrap();
        let cc = ctx.cursor_cap.as_mut().unwrap();
        let stale = cc
            .buf
            .as_ref()
            .is_none_or(|b| b.width != w || b.height != h);
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

    fn cursor_frame_ready(
        &mut self,
        wl_name: u32,
        frame: &ExtImageCopyCaptureFrameV1,
        qh: &QueueHandle<State>,
    ) {
        let serial = {
            let Some(ctx) = self.ctxs.get(&wl_name) else {
                frame.destroy();
                return;
            };
            let Some(cc) = &ctx.cursor_cap else {
                frame.destroy();
                return;
            };
            let Some(buf) = &cc.buf else {
                frame.destroy();
                return;
            };
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
                buf.width as u16,
                buf.height as u16,
                cc.hotspot.0 as u16,
                cc.hotspot.1 as u16,
                image,
            )
        };
        self.server.events.cursor_changed(serial);
        if let Some(ctx) = self.ctxs.get_mut(&wl_name)
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
        if let Some(ctx) = self.ctxs.get_mut(&wl_name)
            && let Some(cc) = ctx.cursor_cap.as_mut()
        {
            cc.in_flight = false;
            cc.cap_ready = false;
            cc.cap_size = None;
            cc.cap_format = None;
        }
        frame.destroy();
    }

    /// Handles a cursor *pointer* event — where the cursor is, from the
    /// `ext_image_copy_capture_cursor_session_v1` object (enter/leave/position/
    /// hotspot). Distinct from the cursor *image* stream
    /// ([`cursor_session_event`](Self::cursor_session_event)).
    fn cursor_pointer_event(
        &mut self,
        wl_name: u32,
        event: ext_cursor_session_v1::Event,
        qh: &QueueHandle<State>,
    ) {
        use ext_cursor_session_v1::Event;
        match event {
            Event::Enter => {
                if let Some(ctx) = self.ctxs.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.present = true;
                }
                self.start_cursor_cap_ext(wl_name, qh);
            }
            Event::Leave => {
                if let Some(ctx) = self.ctxs.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.present = false;
                    cc.pos = None;
                }
            }
            Event::Position { x, y } => {
                // Convert output-local hotspot position to virtual-screen coords.
                let root = self
                    .ctxs
                    .get(&wl_name)
                    .map(|ctx| ((ctx.x + x) as i16, (ctx.y + y) as i16));
                if let Some((rx, ry)) = root {
                    self.server.cursor.update_position(rx, ry);
                }
                if let Some(ctx) = self.ctxs.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.pos = Some((x, y));
                }
            }
            Event::Hotspot { x, y } => {
                if let Some(ctx) = self.ctxs.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.hotspot = (x, y);
                }
            }
            _ => {}
        }
    }

    /// Handles a cursor *image* capture session event (buffer-size/format) — the
    /// cursor-image parallel of [`screen_session_event`](ImageCopyCapture::screen_session_event).
    fn cursor_session_event(
        &mut self,
        wl_name: u32,
        event: ext_session_v1::Event,
        qh: &QueueHandle<State>,
    ) {
        use ext_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                if let Some(ctx) = self.ctxs.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.cap_size = Some((width, height));
                }
            }
            Event::ShmFormat { format } => {
                if let WEnum::Value(fmt) = format
                    && let Some(ctx) = self.ctxs.get_mut(&wl_name)
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
                let ready = if let Some(ctx) = self.ctxs.get_mut(&wl_name)
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
                    self.start_cursor_cap_ext(wl_name, qh);
                }
            }
            Event::Stopped => {
                crate::vlog!("cursor cap session stopped for output {wl_name}");
                if let Some(ctx) = self.ctxs.get_mut(&wl_name)
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

    /// Handles a cursor *image* capture frame event (the captured cursor image) —
    /// the cursor-image parallel of [`screen_frame_event`](ImageCopyCapture::screen_frame_event).
    fn cursor_frame_event(
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
                if let Some(ctx) = self.ctxs.get_mut(&wl_name)
                    && let Some(cc) = ctx.cursor_cap.as_mut()
                {
                    cc.frame_damage.push((x, y, width, height));
                }
            }
            Event::Transform { .. } | Event::PresentationTime { .. } => {}
            Event::Ready => self.cursor_frame_ready(wl_name, frame, qh),
            Event::Failed { .. } => self.cursor_frame_failed(wl_name, frame),
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureCursorSessionV1,
        event: ext_cursor_session_v1::Event,
        &wl_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let Capture::ImageCopy(cap) = &mut state.capture {
            cap.cursor_pointer_event(wl_name, event, qh);
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
        if let Capture::ImageCopy(cap) = &mut state.capture {
            cap.cursor_session_event(wl_name, event, qh);
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
        if let Capture::ImageCopy(cap) = &mut state.capture {
            cap.cursor_frame_event(wl_name, frame, event, qh);
        }
    }
}
