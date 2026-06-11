//! Output (monitor) tracking: mirrors the compositor's `wl_output` /
//! `zxdg_output` state into [`OutputAcc`](super::OutputAcc) and, on each `done`,
//! recomputes the virtual-screen layout that drives the X RandR model, the
//! framebuffer size, the per-output capture positions, and the input remap.

use super::*;

impl State {
    /// Creates the xdg-output for an output once both the output proxy and the
    /// xdg-output manager exist. Idempotent.
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
            (
                changed,
                (s.width, s.height),
                (s.timestamp, s.config_timestamp),
            )
        };
        self.server
            .framebuffer
            .ensure(u32::from(geom.0), u32::from(geom.1));
        // A relayout can shift other outputs' physical positions too, so refresh
        // every capture context from the recomputed layout, not just this one.
        let positions: Vec<(u32, (i32, i32))> = {
            let s = self.server.screen.lock().unwrap();
            // Feed the physical↔logical layout to the input path so absolute
            // pointer motion maps to logical coordinates (correct under scaling).
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
        // Resize the framebuffer and refresh the remaining outputs' positions and
        // input layout — removing an output can shrink the screen and shift the
        // others (same work apply_output does when an output is added/changed).
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
