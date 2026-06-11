use std::collections::HashMap;

/// Stores window properties per-connection. Note that we don't actually have
/// any real windows, but this needed for the clipboard to work.
#[derive(Default)]
pub(super) struct Properties {
    map: HashMap<(u32, u32), Property>,
}

/// A window property.
pub(super) struct Property {
    pub type_: u32,
    pub format: u8,
    pub data: Vec<u8>,
}

impl Properties {
    pub fn set(&mut self, window: u32, property: u32, value: Property) {
        self.map.insert((window, property), value);
    }

    pub fn remove(&mut self, window: u32, property: u32) -> Option<Property> {
        self.map.remove(&(window, property))
    }

    pub fn get(&self, window: u32, property: u32) -> Option<&Property> {
        self.map.get(&(window, property))
    }

    /// The atoms of every property set on `window`.
    pub fn list(&self, window: u32) -> Vec<u32> {
        self.map
            .keys()
            .filter(|(w, _)| *w == window)
            .map(|(_, a)| *a)
            .collect()
    }
}
