//! Output tracking: accumulates each `wl_output`/`zxdg_output` into an
//! [`OutputAcc`] and, on every `done`, recomputes the virtual-screen layout that
//! drives the RandR model, the framebuffer size, the capture positions and the
//! input remap.

use super::*;

/// One output's events, accumulated until its `done`. Wayland delivers geometry
/// piecemeal across two protocols, so nothing here is trustworthy mid-batch.
#[derive(Default)]
pub(super) struct OutputAcc {
    pub proxy: Option<wl_output::WlOutput>,
    pub xdg: Option<ZxdgOutputV1>, // created once the manager is available
    pub x: i32,                    // wl_output geometry position, a scale-1 fallback
    pub y: i32,
    pub width: i32, // physical mode resolution, from the Mode event
    pub height: i32,
    pub refresh_mhz: i32,
    /// Integer scale from wl_output, 0 if unset. Only approximates the logical
    /// size when xdg-output is missing; xdg-output is preferred because it
    /// reports the true, possibly fractional, size.
    pub scale: i32,
    pub logical_x: Option<i32>, // from xdg-output, if it arrived
    pub logical_y: Option<i32>,
    pub logical_width: Option<i32>,
    pub logical_height: Option<i32>,
    pub name: Vec<u8>,
}

impl State {
    /// Creates an output's xdg-output once both it and the manager exist.
    /// Idempotent, since either can arrive first.
    pub(super) fn ensure_xdg_output(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        let Some(mgr) = &self.xdg_output_mgr else {
            return;
        };
        let Some(acc) = self.outputs.get_mut(&wl_name) else {
            return;
        };
        if acc.xdg.is_some() {
            return;
        }
        let Some(output) = &acc.proxy else { return };
        acc.xdg = Some(mgr.get_xdg_output(output, qh, wl_name));
    }

    pub(super) fn apply_output(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        let Some(acc) = self.outputs.get(&wl_name) else {
            return;
        };
        if acc.width <= 0 || acc.height <= 0 {
            return;
        }
        // The logical rect drives the layout topology, so prefer xdg-output,
        // which is accurate under fractional scaling. Falling back to wl_output
        // geometry and mode/scale is correct for unscaled and integer-scaled
        // outputs, but fractional scales really do need xdg-output.
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
            (
                changed,
                (s.width, s.height),
                (s.timestamp, s.config_timestamp),
            )
        };
        self.server
            .framebuffer
            .ensure(u32::from(geom.0), u32::from(geom.1));
        // a relayout can shift other outputs too, so refresh every capture
        // context from the new layout rather than just this one
        let positions: Vec<(u32, (i32, i32))> = {
            let s = self.server.screen.lock().unwrap();
            // hand the layout to the input path so absolute motion resolves in
            // logical coordinates, which is what scaling needs
            self.server.input.set_layout(s.layout_rects());
            self.capture.positions(&s)
        };
        for (n, (px, py)) in positions {
            self.capture.set_position(n, px, py);
        }
        if changed {
            self.server.input.set_geometry(geom.0, geom.1);
            self.server
                .events
                .screen_changed(geom.0, geom.1, ts.0, ts.1);
            crate::log!("outputs changed; virtual screen {}x{}", geom.0, geom.1);
        }
        self.maybe_start_captures(qh);
    }

    pub(super) fn remove_output(&mut self, wl_name: u32) {
        self.outputs.remove(&wl_name);
        self.capture.remove_output(wl_name);
        let (changed, geom, ts, rects, positions) = {
            let mut s = self.server.screen.lock().unwrap();
            let changed = s.remove_wayland_output(wl_name);
            let rects = s.layout_rects();
            let positions = self.capture.positions(&s);
            (
                changed,
                (s.width, s.height),
                (s.timestamp, s.config_timestamp),
                rects,
                positions,
            )
        };
        // Removing an output can shrink the screen and shift the rest, so resize
        // the framebuffer and refresh the survivors' positions and input layout
        // — the same work apply_output does on add or change.
        self.server
            .framebuffer
            .ensure(u32::from(geom.0), u32::from(geom.1));
        self.server.input.set_layout(rects);
        for (n, (px, py)) in positions {
            self.capture.set_position(n, px, py);
        }
        if changed {
            self.server.input.set_geometry(geom.0, geom.1);
            self.server
                .events
                .screen_changed(geom.0, geom.1, ts.0, ts.1);
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
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
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
        let Some(acc) = state.outputs.get_mut(&wl_name) else {
            return;
        };
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
