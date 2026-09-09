//! Cursor capture for the ext-image-copy backend.
//!
//! Each output's `pointer_cursor_session`, created alongside its screen session
//! in [`ImageCopyCapture`], carries two things: the
//! pointer's position and hotspot, and a separate image stream for the cursor
//! itself, which becomes an X ARGB cursor published over XFixes. None of this
//! runs when `-cursor none` bakes the cursor into the screen frames.

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

/// One output's cursor capture. The image is the same on every output and only
/// the position differs, so each session contributes updates while the pointer
/// is on it, bracketed by `enter`/`leave`.
pub(super) struct CursorCap {
    /// Source of the `enter`/`leave`/`position`/`hotspot` events. Held for the
    /// events, and destroyed with the context.
    cursor_session: ExtImageCopyCaptureCursorSessionV1,
    /// The image sub-session, from `get_capture_session()`.
    cap_session: Option<ExtImageCopyCaptureSessionV1>,
    cap_ready: bool,
    cap_size: Option<(u32, u32)>,
    cap_format: Option<wl_shm::Format>,
    buf: Option<ShmBuffer>,
    /// The frame awaiting `ready`/`failed`; see [`Ctx::frame`](super::Ctx).
    frame: Option<ExtImageCopyCaptureFrameV1>,
    /// Whether the pointer is on this output right now.
    present: bool,
    /// Hotspot within the cursor image.
    hotspot: (i32, i32),
    /// Hotspot in output-local buffer coordinates, `None` after a leave.
    pos: Option<(i32, i32)>,
    frame_damage: Vec<(i32, i32, i32, i32)>,
}

impl CursorCap {
    /// Opens a cursor session against an output's `source`, and its image
    /// sub-session.
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
            frame: None,
            present: false,
            hotspot: (0, 0),
            pos: None,
            frame_damage: Vec::new(),
        }
    }
}

impl Drop for CursorCap {
    fn drop(&mut self) {
        // as for the screen context: the proxies don't destroy the objects
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        if let Some(session) = self.cap_session.take() {
            session.destroy();
        }
        self.cursor_session.destroy();
    }
}

/// Userdata marking a session or frame as the cursor's rather than the screen's.
/// Both use the same Wayland types, so the `Dispatch` impls need telling apart.
struct CursorSessionUD(u32); // inner = wl_name

impl ImageCopyCapture {
    fn start_cursor_cap_ext(&mut self, wl_name: u32, qh: &QueueHandle<State>) {
        let (cap_session, w, h, fmt) = {
            let Some(ctx) = self.ctxs.get(&wl_name) else {
                return;
            };
            let Some(cc) = &ctx.cursor_cap else { return };
            if cc.frame.is_some() || !cc.present || !cc.cap_ready {
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
            cc.frame = Some(frame);
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
            // SAFETY: an shm mapping, 4-byte aligned, pixel_count * 4 == buf.size
            let raw = unsafe { std::slice::from_raw_parts(buf.map as *const u32, pixel_count) };
            // convert each pixel to X cursor ARGB (0xAARRGGBB), forcing the
            // x-formats opaque so an alpha-less cursor isn't fully transparent
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
            cc.frame = None;
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
            cc.frame = None;
            cc.cap_ready = false;
            cc.cap_size = None;
            cc.cap_format = None;
        }
        frame.destroy();
    }

    /// Handles a cursor *pointer* event — where the cursor is. Distinct from the
    /// cursor *image* stream, which is
    /// [`cursor_session_event`](Self::cursor_session_event).
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
                // shift the output-local hotspot into virtual-screen coords
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

    /// Handles a cursor *image* session event, the cursor's counterpart to
    /// [`screen_session_event`](ImageCopyCapture::screen_session_event).
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
                    // prefer Argb8888, which needs no conversion and has alpha,
                    // then anything convertible with alpha (transparent cursors),
                    // then anything convertible, and only then the rest
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
                    if let Some(frame) = cc.frame.take() {
                        frame.destroy();
                    }
                    if let Some(session) = cc.cap_session.take() {
                        session.destroy();
                    }
                    cc.cap_ready = false;
                    cc.cap_size = None;
                    cc.cap_format = None;
                }
            }
            _ => {}
        }
    }

    /// Handles a cursor *image* frame event, the cursor's counterpart to
    /// [`screen_frame_event`](ImageCopyCapture::screen_frame_event).
    fn cursor_frame_event(
        &mut self,
        wl_name: u32,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_frame_v1::Event,
        qh: &QueueHandle<State>,
    ) {
        use ext_frame_v1::Event;
        // Same rule as the screen path: only our own in-flight frame counts, or a
        // rejected duplicate re-arms us into producing another one.
        let current = self
            .ctxs
            .get(&wl_name)
            .and_then(|c| c.cursor_cap.as_ref())
            .and_then(|cc| cc.frame.as_ref())
            .is_some_and(|f| f == frame);
        if !current {
            if let Event::Ready | Event::Failed { .. } = event {
                crate::vlog!("ignoring {event:?} for a stale cursor frame on output {wl_name}");
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
