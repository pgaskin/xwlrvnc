use std::collections::HashMap;

use x11rb_protocol::protocol::xproto::Rectangle;

use crate::util::bbox;

// Stores rectangle lists for XFixes.
#[derive(Default)]
pub(super) struct Regions {
    map: HashMap<u32, Vec<Rectangle>>,
}

impl Regions {
    pub fn set(&mut self, id: u32, rects: Vec<Rectangle>) {
        self.map.insert(id, rects);
    }

    pub fn destroy(&mut self, id: u32) {
        self.map.remove(&id);
    }

    pub fn get(&self, id: u32) -> Vec<Rectangle> {
        self.map.get(&id).cloned().unwrap_or_default()
    }

    pub fn copy(&mut self, source: u32, destination: u32) {
        let src = self.get(source);
        self.map.insert(destination, src);
    }

    /// Store the bounding box of `source` as `destination` (empty if `source`
    /// is empty/unknown).
    pub fn extents(&mut self, source: u32, destination: u32) {
        let ext = bbox(self.map.get(&source).map_or(&[][..], Vec::as_slice));
        self.map
            .insert(destination, ext.map_or(vec![], |e| vec![e]));
    }
}
