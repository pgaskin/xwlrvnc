use std::collections::HashMap;

use x11rb_protocol::protocol::xproto::EventMask;

// Per-connection window tracking. Currently only tracks event masks since
// vncserverui needs `PropertyNotify` for clipboard stuff.
#[derive(Default)]
pub(super) struct Windows {
    masks: HashMap<u32, EventMask>,
    warned: u32,
}

impl Windows {
    pub fn set_mask(&mut self, window: u32, mask: EventMask) {
        self.masks.insert(window, mask);
    }

    pub fn remove(&mut self, window: u32) {
        self.masks.remove(&window);
    }

    pub fn wants_property_change(&self, window: u32) -> bool {
        self.masks
            .get(&window)
            .is_some_and(|m| m.contains(EventMask::PROPERTY_CHANGE))
    }

    /// Warns (once per bit, per client) when a client selects event-mask bits
    /// we never deliver like we do for unhandled requests.
    ///
    /// This only covers maskable core events (those a client subscribes to via
    /// an event mask) since clients don't explicitly request unmaskable events.
    ///
    /// We implement the MappingNotify, SelectionRequest and SelectionNotify
    /// unmaskable events. GraphicsExpose and NoExpose are not relevant since we
    /// don't actually draw anything. We do not implement SelectionClear.
    ///
    /// We don't need SelectionClear because RealVNC gets notified about lost
    /// selections by XFixesSelectionNotify (both call
    /// XFixesSelectSelectionInput) and we send that on every Wayland clipboard
    /// change. It's also complicated to implement correctly since when we
    /// re-expose the X selection on Wayland, we get notified about our own
    /// selection, and it's hard to properly tell whether it's from ourself or
    /// another Wayland client.
    pub fn warn_unsupported(&mut self, mask: EventMask) {
        let supported = u32::from(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY);
        let unsupported = u32::from(mask) & !supported & !self.warned;
        if unsupported != 0 {
            self.warned |= unsupported;
            crate::warning!(
                "client selected events we don't deliver: {:?}",
                EventMask::from(unsupported)
            );
        }
    }
}
