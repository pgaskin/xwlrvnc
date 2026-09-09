use std::collections::BTreeMap;

use x11rb_protocol::protocol::randr::{ModeFlag, ModeInfo};

use crate::{
    bridge::x11::RANDR_OUTPUT_BASE,
    util::{OutputRect, mm},
};

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

    /// Reconfigures an output (position + mode), returning true if changed.
    pub fn set_crtc(&mut self, crtc: u32, x: i16, y: i16, mode: u32) -> bool {
        let size = self.mode_size(mode);
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
        self.recompute_bounds()
    }

    /// Recomputes the X11 screen (i.e., bounding box of all outputs) size for
    /// connected enabled outputs, returning true if changed.
    pub fn recompute_bounds(&mut self) -> bool {
        let mut w = 0u16;
        let mut h = 0u16;
        for o in &self.outputs {
            if o.connected && o.mode != 0 {
                w = w.max(o.x.max(0) as u16 + o.width);
                h = h.max(o.y.max(0) as u16 + o.height);
            }
        }
        let (w, h) = (w.max(1), h.max(1));
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
        self.width = width;
        self.height = height;
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
                let mode = if same_mode {
                    self.outputs[i].mode
                } else {
                    self.alloc_mode(width, height, refresh_mhz)
                };
                let o = &mut self.outputs[i];
                let old = o.mode;
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
                // the previous native mode is ours to retire
                if old != mode && self.modes.iter().any(|m| m.info.id == old && m.ours) {
                    self.outputs[i].mode_ids.retain(|&m| m != old);
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
    fn is_wayland(&self) -> bool {
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
