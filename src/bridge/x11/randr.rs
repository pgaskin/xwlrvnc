use std::collections::BTreeMap;

use x11rb_protocol::protocol::randr::{ModeFlag, ModeInfo};

use crate::{
    bridge::x11::RANDR_OUTPUT_BASE,
    util::{OutputRect, mm},
};

/// Largest screen we will accept, matching the range reported by
/// `RRGetScreenSizeRange`. Screen bounds feed the framebuffer allocation, and a
/// client picks the screen size, so without a bound a bad request could ask us
/// to allocate an absurd framebuffer.
pub const MAX_SCREEN: u16 = 16384;

/// A RandR screen backed by Wayland outputs.
pub struct Screen {
    pub width: u16,
    pub height: u16,
    pub modes: Vec<Mode>,
    pub outputs: Vec<Output>,
    next_id: u32,
    pub timestamp: u32,
    pub config_timestamp: u32,
}

/// An output positioned on a screen.
pub struct Output {
    pub output_id: u32,
    pub crtc_id: u32,
    pub name: Vec<u8>,
    /// CRTC position in the virtual screen, in **physical** pixels. For Wayland
    /// outputs this is computed by [`Screen::relayout_physical`] from the logical
    /// layout so that scaled outputs' native-resolution buffers tile without
    /// overlap (see `logical_*` below). The width/height are the physical mode.
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    /// The output's rectangle in the compositor's **logical** coordinate space
    /// (from xdg-output, or wl_output geometry + mode/scale as a fallback). Used
    /// only to reconstruct the relative arrangement; physical positions are
    /// derived from it. Zero for the synthetic default output.
    pub lx: i32,
    pub ly: i32,
    pub lw: i32,
    pub lh: i32,
    pub mode: u32, // 0 is disabled
    pub mode_ids: Vec<u32>,
    pub mm_width: u32,
    pub mm_height: u32,
    pub wl_name: u32, // wayland object, or 0 if not
    pub connected: bool,
}

// A mode for an output.
pub struct Mode {
    pub info: ModeInfo,
    pub name: Vec<u8>,
    /// Allocated by us for an output's native resolution, and so freed once no
    /// output lists it; a client-created mode (`RandrCreateMode`) is the
    /// client's to destroy.
    ours: bool,
}

impl Screen {
    /// A default single output of the given size (used until Wayland outputs
    /// arrive, or if there's no compositor).
    pub fn new(width: u16, height: u16) -> Self {
        let mut s = Self {
            width,
            height,
            modes: Vec::new(),
            outputs: Vec::new(),
            next_id: RANDR_OUTPUT_BASE,
            timestamp: 1,
            config_timestamp: 1,
        };
        let mode = s.alloc_mode(width, height, 0);
        let (output_id, crtc_id) = (s.alloc_id(), s.alloc_id());
        s.outputs.push(Output {
            output_id,
            crtc_id,
            name: b"screen".to_vec(),
            x: 0,
            y: 0,
            width,
            height,
            lx: 0,
            ly: 0,
            lw: i32::from(width),
            lh: i32::from(height),
            mode,
            mode_ids: vec![mode],
            mm_width: mm(width),
            mm_height: mm(height),
            wl_name: 0,
            connected: true,
        });
        s
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn alloc_mode(&mut self, width: u16, height: u16, refresh_mhz: u32) -> u32 {
        let id = self.alloc_id();
        self.modes.push(Mode::new(id, width, height, refresh_mhz));
        id
    }

    pub fn create_mode(&mut self, mut info: ModeInfo, name: Vec<u8>) -> u32 {
        let id = self.alloc_id();
        info.id = id;
        info.name_len = name.len() as u16;
        self.modes.push(Mode {
            info,
            name,
            ours: false,
        });
        id
    }

    /// Drops the native modes no output lists any more, after an output's mode
    /// changed or the output went away; without this every resolution change
    /// leaves a mode behind forever.
    fn prune_modes(&mut self) {
        let outputs = &self.outputs;
        self.modes
            .retain(|m| !m.ours || outputs.iter().any(|o| o.mode_ids.contains(&m.info.id)));
    }

    pub fn add_output_mode(&mut self, output: u32, mode: u32) {
        if let Some(o) = self.outputs.iter_mut().find(|o| o.output_id == output)
            && !o.mode_ids.contains(&mode)
        {
            o.mode_ids.push(mode);
        }
    }

    pub fn delete_output_mode(&mut self, output: u32, mode: u32) {
        if let Some(o) = self.outputs.iter_mut().find(|o| o.output_id == output) {
            o.mode_ids.retain(|&m| m != mode);
        }
    }

    pub fn destroy_mode(&mut self, mode: u32) {
        self.modes.retain(|m| m.info.id != mode);
        for o in &mut self.outputs {
            o.mode_ids.retain(|&m| m != mode);
        }
    }

    pub fn output_by_id(&self, output: u32) -> Option<&Output> {
        self.outputs.iter().find(|o| o.output_id == output)
    }

    pub fn output_by_crtc(&self, crtc: u32) -> Option<&Output> {
        self.outputs.iter().find(|o| o.crtc_id == crtc)
    }

    pub fn output_by_wl_name(&self, wl_name: u32) -> Option<&Output> {
        self.outputs
            .iter()
            .find(|o| o.wl_name == wl_name && o.is_wayland())
    }

    pub fn mode_size(&self, id: u32) -> Option<(u16, u16)> {
        self.modes
            .iter()
            .find(|m| m.info.id == id)
            .map(|m| (m.info.width, m.info.height))
    }

    pub fn mode_infos(&self) -> Vec<ModeInfo> {
        self.modes.iter().map(|m| m.info).collect()
    }

    pub fn mode_names(&self) -> Vec<u8> {
        self.modes
            .iter()
            .flat_map(|m| m.name.iter().copied())
            .collect()
    }

    pub fn crtc_ids(&self) -> Vec<u32> {
        self.outputs.iter().map(|o| o.crtc_id).collect()
    }

    pub fn output_ids(&self) -> Vec<u32> {
        self.outputs.iter().map(|o| o.output_id).collect()
    }

    /// The physical CRTC position of the output mirroring `wl_name`, for placing
    /// its capture buffer in the framebuffer.
    pub fn physical_pos(&self, wl_name: u32) -> Option<(i32, i32)> {
        self.outputs
            .iter()
            .find(|o| o.wl_name == wl_name)
            .map(|o| (i32::from(o.x), i32::from(o.y)))
    }

    /// Per-output physical (X11 screen) and logical (Wayland compositor)
    /// rectangles for the connected enabled outputs. Used to remap absolute
    /// pointer input from the physical-tiled screen back into logical space (so
    /// it is correct for scaled displays).
    pub fn layout_rects(&self) -> Vec<OutputRect> {
        self.outputs
            .iter()
            .filter(|o| o.connected && o.mode != 0)
            .map(|o| OutputRect {
                px: i32::from(o.x),
                py: i32::from(o.y),
                pw: i32::from(o.width),
                ph: i32::from(o.height),
                lx: o.lx,
                ly: o.ly,
                lw: o.lw,
                lh: o.lh,
            })
            .collect()
    }

    /// Reconfigures a CRTC (position + mode, 0 to disable) in the model only,
    /// as `RRSetCrtcConfig` does: the screen size is the client's to set with
    /// `RRSetScreenSize`, so the bounds are left alone. Returns false for an
    /// unknown CRTC or mode.
    ///
    /// For a Wayland output this is only right when the compositor is (or has
    /// just been) driving the output at the mode's size; see
    /// [`adopt_mode`](Self::adopt_mode) for the latter.
    pub fn set_crtc(&mut self, crtc: u32, x: i16, y: i16, mode: u32) -> bool {
        let size = if mode == 0 {
            None
        } else {
            match self.mode_size(mode) {
                Some(size) => Some(size),
                None => return false,
            }
        };
        let Some(o) = self.outputs.iter_mut().find(|o| o.crtc_id == crtc) else {
            return false;
        };
        o.mode = mode;
        o.x = x;
        o.y = y;
        if let Some((w, h)) = size {
            o.width = w;
            o.height = h;
        }
        self.bump();
        true
    }

    /// Puts every Wayland output back to what the compositor is actually
    /// driving, and the screen back to their bounds: re-enables CRTCs a client
    /// disabled, at a mode of the output's real size. For rolling back a
    /// client's reconfiguration that the compositor then refused (RealVNC
    /// disables the CRTC and resizes the screen *before* it asks for the mode,
    /// and does not undo either when that fails, which would leave the X
    /// screen a size the capture never fills and the output disabled).
    /// Returns whether anything changed.
    pub fn restore_outputs(&mut self) -> bool {
        let mut changed = false;
        for i in 0..self.outputs.len() {
            let o = &self.outputs[i];
            if !o.is_wayland() {
                continue;
            }
            let (width, height) = (o.width, o.height);
            let ok = self
                .mode_size(o.mode)
                .is_some_and(|size| size == (width, height));
            if ok {
                continue;
            }
            let listed = o
                .mode_ids
                .iter()
                .copied()
                .find(|&id| self.mode_size(id) == Some((width, height)));
            let mode = match listed {
                Some(id) => id,
                None => self.alloc_mode(width, height, 0),
            };
            let o = &mut self.outputs[i];
            o.mode = mode;
            if !o.mode_ids.contains(&mode) {
                o.mode_ids.push(mode);
            }
            changed = true;
        }
        self.relayout_physical();
        let bounds = self.recompute_bounds();
        if changed && !bounds {
            self.bump();
        }
        changed || bounds
    }

    /// After the compositor drove a Wayland output to a client's requested
    /// size, switches its CRTC to the mode the client actually asked for (the
    /// wl_output sync gave it a native mode of the same size), so the client
    /// reads back what it set. A no-op unless the sizes agree.
    pub fn adopt_mode(&mut self, crtc: u32, mode: u32) {
        let Some((w, h)) = self.mode_size(mode) else {
            return;
        };
        let Some(o) = self.outputs.iter_mut().find(|o| o.crtc_id == crtc) else {
            return;
        };
        if o.width == w && o.height == h && o.mode != mode {
            if !o.mode_ids.contains(&mode) {
                o.mode_ids.push(mode);
            }
            o.mode = mode;
            // the native mode of that size stood for what the compositor
            // drives, which the client's mode now does
            let native: Vec<u32> = self
                .modes
                .iter()
                .filter(|m| m.ours && m.info.id != mode && (m.info.width, m.info.height) == (w, h))
                .map(|m| m.info.id)
                .collect();
            self.outputs
                .iter_mut()
                .find(|o| o.crtc_id == crtc)
                .unwrap()
                .mode_ids
                .retain(|m| !native.contains(m));
            self.prune_modes();
            self.bump();
        }
    }

    /// Recomputes the X11 screen (i.e., bounding box of all outputs) size for
    /// connected enabled outputs, returning true if changed.
    pub fn recompute_bounds(&mut self) -> bool {
        let mut w = 0u16;
        let mut h = 0u16;
        for o in &self.outputs {
            if o.connected && o.mode != 0 {
                // saturating, since a client picks the crtc positions and
                // `position + size` can leave u16 behind
                w = w.max((o.x.max(0) as u16).saturating_add(o.width));
                h = h.max((o.y.max(0) as u16).saturating_add(o.height));
            }
        }
        let (w, h) = (w.clamp(1, MAX_SCREEN), h.clamp(1, MAX_SCREEN));
        let changed = w != self.width || h != self.height;
        self.width = w;
        self.height = h;
        if changed {
            self.bump();
        }
        changed
    }

    /// Derives each output's physical CRTC position from the logical layout so
    /// that native-resolution capture buffers tile without overlap, even when
    /// outputs have different (possibly fractional) scales.
    ///
    /// Logical coordinates only encode the arrangement; physical sizes differ
    /// by scale. We repack each axis independently, walking outputs
    /// left-to-right (top-to-bottom), so an output that begins where a
    /// neighbour's logical edge ended is placed where that neighbour's physical
    /// edge ended. This is exact for edge-aligned layouts (rows, columns,
    /// aligned grids); gaps are bridged 1:1.
    fn relayout_physical(&mut self) {
        let xs: Vec<(i32, i32, i32)> = self
            .outputs
            .iter()
            .map(|o| (o.lx, o.lw, i32::from(o.width)))
            .collect();
        let ys: Vec<(i32, i32, i32)> = self
            .outputs
            .iter()
            .map(|o| (o.ly, o.lh, i32::from(o.height)))
            .collect();
        let px = pack_axis(&xs);
        let py = pack_axis(&ys);
        let max = i32::from(i16::MAX);
        for (i, o) in self.outputs.iter_mut().enumerate() {
            o.x = px[i].clamp(0, max) as i16;
            o.y = py[i].clamp(0, max) as i16;
        }
    }

    pub fn set_size(&mut self, width: u16, height: u16) {
        self.width = width.clamp(1, MAX_SCREEN);
        self.height = height.clamp(1, MAX_SCREEN);
        self.bump();
    }

    pub fn bump(&mut self) {
        self.timestamp = self.timestamp.wrapping_add(1);
        self.config_timestamp = self.config_timestamp.wrapping_add(1);
    }

    /// Creates or updates the output mirroring a given Wayland output, then
    /// recomputes the physical layout and screen bounds. Returns whether
    /// anything changed.
    ///
    /// `lx,ly,lw,lh` is the output's rectangle in the compositor's logical
    /// space; `width,height` is its physical mode resolution. The CRTC keeps the
    /// physical mode as its size, and its physical position is derived from the
    /// logical layout in [`relayout_physical`](Self::relayout_physical).
    #[allow(clippy::too_many_arguments)]
    pub fn sync_wayland_output(
        &mut self,
        wl_name: u32,
        name: &[u8],
        lx: i32,
        ly: i32,
        lw: i32,
        lh: i32,
        width: u16,
        height: u16,
        refresh_mhz: u32,
    ) -> bool {
        // Find or create the output and its native mode.
        let existing = self.outputs.iter().position(|o| o.wl_name == wl_name);
        match existing {
            Some(i) => {
                let o = &self.outputs[i];
                let same_mode = self
                    .modes
                    .iter()
                    .find(|m| m.info.id == o.mode)
                    .is_some_and(|m| m.info.width == width && m.info.height == height);
                let unchanged = o.lx == lx
                    && o.ly == ly
                    && o.lw == lw
                    && o.lh == lh
                    && o.width == width
                    && o.height == height
                    && same_mode;
                if unchanged {
                    return false;
                }
                // keep the mode if it already has this size; else reuse one of
                // the output's listed modes with it (a client-created mode the
                // compositor was just driven to); else allocate a native one
                let listed = self.outputs[i]
                    .mode_ids
                    .iter()
                    .copied()
                    .find(|&id| self.mode_size(id) == Some((width, height)));
                let mode = if same_mode {
                    self.outputs[i].mode
                } else if let Some(id) = listed {
                    id
                } else {
                    self.alloc_mode(width, height, refresh_mhz)
                };
                let o = &mut self.outputs[i];
                o.lx = lx;
                o.ly = ly;
                o.lw = lw;
                o.lh = lh;
                o.width = width;
                o.height = height;
                o.mode = mode;
                if !o.mode_ids.contains(&mode) {
                    o.mode_ids.push(mode);
                }
                // Any native mode of another size is ours to retire: a native
                // mode only ever stands for what the compositor is driving now.
                // (Looked up by size rather than by the previous CRTC mode, which
                // is 0 when a client disabled the CRTC before changing it, as
                // RealVNC does.)
                let stale: Vec<u32> = self
                    .modes
                    .iter()
                    .filter(|m| {
                        m.ours && m.info.id != mode && self.outputs[i].mode_ids.contains(&m.info.id)
                    })
                    .map(|m| m.info.id)
                    .collect();
                if !stale.is_empty() {
                    self.outputs[i].mode_ids.retain(|m| !stale.contains(m));
                    self.prune_modes();
                }
            }
            None => {
                let mode = self.alloc_mode(width, height, refresh_mhz);
                let (output_id, crtc_id) = (self.alloc_id(), self.alloc_id());
                let name = if name.is_empty() {
                    format!("output-{wl_name}").into_bytes()
                } else {
                    name.to_vec()
                };
                self.outputs.push(Output {
                    output_id,
                    crtc_id,
                    name,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    lx,
                    ly,
                    lw,
                    lh,
                    mode,
                    mode_ids: vec![mode],
                    mm_width: mm(width),
                    mm_height: mm(height),
                    wl_name,
                    connected: true,
                });
                // drop the fake output, and its mode, now that we have a real one
                self.outputs.retain(|o| o.is_wayland());
                self.prune_modes();
            }
        }
        self.relayout_physical();
        self.recompute_bounds();
        true
    }

    pub fn remove_wayland_output(&mut self, wl_name: u32) -> bool {
        let before = self.outputs.len();
        self.outputs.retain(|o| o.wl_name != wl_name);
        if self.outputs.len() != before {
            self.prune_modes();
            self.relayout_physical();
            self.recompute_bounds();
            true
        } else {
            false
        }
    }
}

impl Output {
    /// Whether this output mirrors a real Wayland output (`wl_name` 0 is the
    /// synthetic default we start with before any output is known).
    pub fn is_wayland(&self) -> bool {
        self.wl_name != 0
    }
}

impl Mode {
    fn new(id: u32, width: u16, height: u16, refresh_mhz: u32) -> Self {
        let name = format!("{width}x{height}").into_bytes();
        let dot_clock = if refresh_mhz > 0 {
            (u64::from(width) * u64::from(height) * u64::from(refresh_mhz) / 1000) as u32
        } else {
            u32::from(width) * u32::from(height) * 60
        };
        let info = ModeInfo {
            id,
            width,
            height,
            dot_clock,
            hsync_start: 0,
            hsync_end: 0,
            htotal: width,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: height,
            name_len: name.len() as u16,
            mode_flags: ModeFlag::from(0u32),
        };
        Self {
            info,
            name,
            ours: true,
        }
    }
}

/// Repacks one axis, given each output's `(logical_start, logical_len,
/// physical_len)`, returning its physical start. See
/// [`Screen::relayout_physical`].
fn pack_axis(items: &[(i32, i32, i32)]) -> Vec<i32> {
    let mut out = vec![0i32; items.len()];
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].0);

    // map a logical coordinate (output edge) to its assigned physical coordinate
    let mut bound: BTreeMap<i32, i32> = BTreeMap::new();
    if let Some(min_start) = items.iter().map(|&(s, _, _)| s).min() {
        bound.insert(min_start, 0);
    }
    for &i in &order {
        let (ls, llen, plen) = items[i];
        let px = match bound.get(&ls) {
            Some(&p) => p,
            // no exact boundary (i.e., gap), extend from the nearest known edge
            // 1:1
            None => match bound.range(..=ls).next_back() {
                Some((&b, &pb)) => pb + (ls - b),
                None => 0,
            },
        };
        bound.entry(ls).or_insert(px);
        out[i] = px;
        bound
            .entry(ls + llen)
            .and_modify(|v| *v = (*v).max(px + plen))
            .or_insert(px + plen);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen mirroring one 1280x800 Wayland output (registry name 7).
    fn screen() -> Screen {
        let mut s = Screen::new(640, 480);
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1280, 800, 1280, 800, 60_000));
        s
    }

    fn the_output(s: &Screen) -> &Output {
        s.output_by_wl_name(7).expect("the wayland output")
    }

    fn native_modes(s: &Screen) -> Vec<(u16, u16)> {
        s.modes
            .iter()
            .filter(|m| m.ours)
            .map(|m| (m.info.width, m.info.height))
            .collect()
    }

    #[test]
    fn wayland_output_replaces_the_default() {
        let s = screen();
        assert_eq!(s.outputs.len(), 1);
        assert_eq!((s.width, s.height), (1280, 800));
        assert_eq!(native_modes(&s), vec![(1280, 800)]);
        assert_eq!(s.mode_size(the_output(&s).mode), Some((1280, 800)));
    }

    #[test]
    fn set_crtc_leaves_the_screen_size_alone() {
        // RRSetCrtcConfig never resizes the screen; that is RRSetScreenSize's
        // job (and RealVNC disables the CRTC before it shrinks the screen)
        let mut s = screen();
        let crtc = the_output(&s).crtc_id;
        let ts = s.config_timestamp;
        assert!(s.set_crtc(crtc, 0, 0, 0));
        assert_eq!(the_output(&s).mode, 0);
        assert_eq!((s.width, s.height), (1280, 800));
        assert_ne!(s.config_timestamp, ts);
        assert!(
            s.layout_rects().is_empty(),
            "a disabled CRTC takes no input"
        );
        assert!(!s.set_crtc(crtc, 0, 0, 12345), "unknown mode");
        assert!(!s.set_crtc(999, 0, 0, 0), "unknown crtc");
    }

    #[test]
    fn set_size_is_clamped() {
        let mut s = screen();
        s.set_size(0, 40_000);
        assert_eq!((s.width, s.height), (1, MAX_SCREEN));
    }

    #[test]
    fn compositor_change_reuses_the_clients_mode_and_retires_the_native_one() {
        let mut s = screen();
        let (output, crtc) = (the_output(&s).output_id, the_output(&s).crtc_id);
        // the client's sequence: create + add a mode, disable, resize, set
        let info = ModeInfo {
            id: 0,
            width: 1024,
            height: 640,
            dot_clock: 0,
            hsync_start: 0,
            hsync_end: 0,
            htotal: 1024,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: 640,
            name_len: 0,
            mode_flags: ModeFlag::from(0u32),
        };
        let client_mode = s.create_mode(info, b"1024x640_vnc".to_vec());
        s.add_output_mode(output, client_mode);
        assert!(s.set_crtc(crtc, 0, 0, 0));
        s.set_size(1024, 640);
        // ... and the compositor's answer, as a wl_output change
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1024, 640, 1024, 640, 60_000));
        let o = the_output(&s);
        assert_eq!((o.width, o.height), (1024, 640));
        assert_eq!(
            o.mode, client_mode,
            "the listed mode of that size is reused"
        );
        assert_eq!((s.width, s.height), (1024, 640));
        assert!(
            native_modes(&s).is_empty(),
            "the old native mode is retired even though the CRTC was disabled"
        );
        assert!(
            s.modes.iter().any(|m| m.info.id == client_mode),
            "the client's mode is the client's to destroy"
        );
        // adopting is then a no-op
        let ts = s.config_timestamp;
        s.adopt_mode(crtc, client_mode);
        assert_eq!(s.config_timestamp, ts);
    }

    #[test]
    fn adopt_mode_switches_to_the_clients_mode_of_the_same_size() {
        let mut s = screen();
        let crtc = the_output(&s).crtc_id;
        // the compositor changed first (a native mode was allocated), then the
        // client's own mode of that size is adopted
        assert!(s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 800, 600, 800, 600, 60_000));
        assert_eq!(native_modes(&s), vec![(800, 600)]);
        let client_mode = s.alloc_mode(800, 600, 0);
        s.modes.last_mut().unwrap().ours = false;
        s.adopt_mode(crtc, client_mode);
        assert_eq!(the_output(&s).mode, client_mode);
        assert!(
            native_modes(&s).is_empty(),
            "the native mode of that size is pruned"
        );
        // a mode of another size is not adopted
        let other = s.alloc_mode(640, 480, 0);
        s.adopt_mode(crtc, other);
        assert_eq!(the_output(&s).mode, client_mode);
    }

    #[test]
    fn restore_outputs_undoes_a_refused_reconfiguration() {
        let mut s = screen();
        let crtc = the_output(&s).crtc_id;
        let mode = the_output(&s).mode;
        assert!(!s.restore_outputs(), "nothing to restore");
        // the client disabled the CRTC and shrank the screen for a mode the
        // compositor then refused
        assert!(s.set_crtc(crtc, 0, 0, 0));
        s.set_size(1000, 700);
        assert!(s.restore_outputs());
        let o = the_output(&s);
        assert_eq!(o.mode, mode, "re-enabled at the output's real mode");
        assert_eq!((o.width, o.height), (1280, 800));
        assert_eq!(
            (s.width, s.height),
            (1280, 800),
            "the screen is the outputs' bounds again"
        );
        assert_eq!(s.layout_rects().len(), 1);
    }

    #[test]
    fn unchanged_compositor_state_is_a_no_op() {
        let mut s = screen();
        let ts = s.config_timestamp;
        assert!(!s.sync_wayland_output(7, b"HEADLESS-1", 0, 0, 1280, 800, 1280, 800, 60_000));
        assert_eq!(s.config_timestamp, ts);
    }

    #[test]
    fn removing_the_output_prunes_its_native_mode() {
        let mut s = screen();
        assert!(s.remove_wayland_output(7));
        assert!(s.outputs.is_empty());
        assert!(native_modes(&s).is_empty());
        assert_eq!((s.width, s.height), (1, 1));
        assert!(!s.remove_wayland_output(7));
    }
}
