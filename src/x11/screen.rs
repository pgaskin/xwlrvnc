//! The screen/RandR model: a set of outputs, each with its own CRTC at a
//! position in the virtual screen, driving a mode. The screen size is the
//! bounding box of all connected outputs. Shared (behind a mutex) because RandR
//! and Wayland output changes both mutate it and it drives pointer scaling.

use x11rb_protocol::protocol::randr::{ModeFlag, ModeInfo};

const FIRST_ID: u32 = 0x40;

pub struct Mode {
    pub info: ModeInfo,
    pub name: Vec<u8>,
}

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
    /// Current mode id on this output's CRTC (0 = disabled).
    pub mode: u32,
    /// Mode ids this output advertises.
    pub mode_ids: Vec<u32>,
    pub mm_width: u32,
    pub mm_height: u32,
    /// The Wayland registry name this output mirrors (0 = synthetic/default).
    pub wl_name: u32,
    pub connected: bool,
}

pub struct Screen {
    pub width: u16,
    pub height: u16,
    pub modes: Vec<Mode>,
    pub outputs: Vec<Output>,
    next_id: u32,
    pub timestamp: u32,
    pub config_timestamp: u32,
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
            next_id: FIRST_ID,
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

    /// Creates a mode and appends it to the global mode list, returning its id.
    pub fn alloc_mode(&mut self, width: u16, height: u16, refresh_mhz: u32) -> u32 {
        let id = self.alloc_id();
        self.modes.push(make_mode(id, width, height, refresh_mhz));
        id
    }

    pub fn create_mode(&mut self, mut info: ModeInfo, name: Vec<u8>) -> u32 {
        let id = self.alloc_id();
        info.id = id;
        info.name_len = name.len() as u16;
        self.modes.push(Mode { info, name });
        id
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
        self.modes.iter().map(|m| m.info.clone()).collect()
    }

    pub fn mode_names(&self) -> Vec<u8> {
        self.modes.iter().flat_map(|m| m.name.iter().copied()).collect()
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

    /// Per-output physical (X-screen) and logical (compositor) rectangles for the
    /// connected, enabled outputs. Used to remap absolute pointer input from our
    /// physical-tiled screen back into logical space (correct under scaling).
    /// Each tuple is `(px, py, pw, ph, lx, ly, lw, lh)`.
    pub fn layout_rects(&self) -> Vec<(i32, i32, i32, i32, i32, i32, i32, i32)> {
        self.outputs
            .iter()
            .filter(|o| o.connected && o.mode != 0)
            .map(|o| {
                (
                    i32::from(o.x),
                    i32::from(o.y),
                    i32::from(o.width),
                    i32::from(o.height),
                    o.lx,
                    o.ly,
                    o.lw,
                    o.lh,
                )
            })
            .collect()
    }

    /// Reconfigures a CRTC (position + mode). Returns whether geometry changed.
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

    /// Recomputes the screen bounding box from connected, enabled outputs.
    /// Returns whether it changed.
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
    /// Logical coordinates only encode the *arrangement*; physical sizes differ
    /// by scale. We repack each axis independently: walking outputs left-to-right
    /// (top-to-bottom), an output that begins where a neighbour's logical edge
    /// ended is placed where that neighbour's *physical* edge ended. This is
    /// exact for edge-aligned layouts (rows, columns, aligned grids — the only
    /// sane multi-monitor configs); gaps are bridged 1:1.
    fn relayout_physical(&mut self) {
        let xs: Vec<(i32, i32, i32)> =
            self.outputs.iter().map(|o| (o.lx, o.lw, i32::from(o.width))).collect();
        let ys: Vec<(i32, i32, i32)> =
            self.outputs.iter().map(|o| (o.ly, o.lh, i32::from(o.height))).collect();
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
                let unchanged = o.lx == lx && o.ly == ly && o.lw == lw && o.lh == lh
                    && o.width == width && o.height == height && same_mode;
                if unchanged {
                    return false;
                }
                let mode = if same_mode {
                    self.outputs[i].mode
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
                // Drop the synthetic default output (and its now-orphaned mode)
                // now that a real one exists.
                let orphans: Vec<u32> = self
                    .outputs
                    .iter()
                    .filter(|o| o.wl_name == 0)
                    .flat_map(|o| o.mode_ids.iter().copied())
                    .collect();
                self.outputs.retain(|o| o.wl_name != 0);
                self.modes.retain(|m| {
                    !orphans.contains(&m.info.id)
                        || self.outputs.iter().any(|o| o.mode_ids.contains(&m.info.id))
                });
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
            self.relayout_physical();
            self.recompute_bounds();
            true
        } else {
            false
        }
    }
}

/// Repacks one axis: given each output's `(logical_start, logical_len,
/// physical_len)`, returns its physical start. See
/// [`Screen::relayout_physical`].
fn pack_axis(items: &[(i32, i32, i32)]) -> Vec<i32> {
    use std::collections::BTreeMap;
    let mut out = vec![0i32; items.len()];
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].0);
    // Maps a logical coordinate (output edge) to its assigned physical coordinate.
    let mut bound: BTreeMap<i32, i32> = BTreeMap::new();
    if let Some(min_start) = items.iter().map(|&(s, _, _)| s).min() {
        bound.insert(min_start, 0);
    }
    for &i in &order {
        let (ls, llen, plen) = items[i];
        let px = match bound.get(&ls) {
            Some(&p) => p,
            // No exact boundary (a gap): extend from the nearest known edge 1:1.
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

/// Builds a `ModeInfo` with plausible timings (so a refresh rate is shown) and a
/// `WxH` name.
fn make_mode(id: u32, width: u16, height: u16, refresh_mhz: u32) -> Mode {
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
    Mode { info, name }
}

/// Approximate millimeters for a pixel count at 96 DPI.
fn mm(pixels: u16) -> u32 {
    u32::from(pixels) * 254 / 960
}
