use std::collections::HashMap;

/// Per-connection table of interned atoms pre-initialized with the ones from
/// `Xatom.h` (this is required for protocol compatibility).
pub(super) struct Atoms {
    by_name: HashMap<Vec<u8>, u32>,
    by_id: HashMap<u32, Vec<u8>>,
    next: u32,
}

#[rustfmt::skip]
static PREDEFINED_ATOMS: &[&str] = &[ // 1-indexed atoms
    "PRIMARY", "SECONDARY", "ARC", "ATOM", "BITMAP", "CARDINAL", "COLORMAP",
    "CURSOR", "CUT_BUFFER0", "CUT_BUFFER1", "CUT_BUFFER2", "CUT_BUFFER3",
    "CUT_BUFFER4", "CUT_BUFFER5", "CUT_BUFFER6", "CUT_BUFFER7", "DRAWABLE",
    "FONT", "INTEGER", "PIXMAP", "POINT", "RECTANGLE", "RESOURCE_MANAGER",
    "RGB_COLOR_MAP", "RGB_BEST_MAP", "RGB_BLUE_MAP", "RGB_DEFAULT_MAP",
    "RGB_GRAY_MAP", "RGB_GREEN_MAP", "RGB_RED_MAP", "STRING", "VISUALID",
    "WINDOW", "WM_COMMAND", "WM_HINTS", "WM_CLIENT_MACHINE", "WM_ICON_NAME",
    "WM_ICON_SIZE", "WM_NAME", "WM_NORMAL_HINTS", "WM_SIZE_HINTS",
    "WM_ZOOM_HINTS", "MIN_SPACE", "NORM_SPACE", "MAX_SPACE", "END_SPACE",
    "SUPERSCRIPT_X", "SUPERSCRIPT_Y", "SUBSCRIPT_X", "SUBSCRIPT_Y",
    "UNDERLINE_POSITION", "UNDERLINE_THICKNESS", "STRIKEOUT_ASCENT",
    "STRIKEOUT_DESCENT", "ITALIC_ANGLE", "X_HEIGHT", "QUAD_WIDTH", "WEIGHT",
    "POINT_SIZE", "RESOLUTION", "COPYRIGHT", "NOTICE", "FONT_NAME",
    "FAMILY_NAME", "FULL_NAME", "CAP_HEIGHT", "WM_CLASS", "WM_TRANSIENT_FOR",
];

pub const XA_INTEGER: u32 = 19;

impl Default for Atoms {
    fn default() -> Self {
        let by_name = PREDEFINED_ATOMS
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_bytes().to_vec(), i as u32 + 1))
            .collect();

        let by_id = PREDEFINED_ATOMS
            .iter()
            .enumerate()
            .map(|(i, name)| (i as u32 + 1, name.as_bytes().to_vec()))
            .collect();

        Self {
            by_name,
            by_id,
            next: PREDEFINED_ATOMS.len() as u32 + 1,
        }
    }
}

impl Atoms {
    pub fn intern(&mut self, name: &[u8], only_if_exists: bool) -> u32 {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        if only_if_exists {
            return 0;
        }
        let id = self.next;
        self.next += 1;
        self.by_name.insert(name.to_vec(), id);
        self.by_id.insert(id, name.to_vec());
        id
    }

    pub fn name(&self, id: u32) -> Option<&[u8]> {
        self.by_id.get(&id).map(Vec::as_slice)
    }
}
