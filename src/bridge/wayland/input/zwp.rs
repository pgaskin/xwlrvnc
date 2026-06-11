use std::os::fd::BorrowedFd;

use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

use super::*;

impl KeyboardBackend for ZwpVirtualKeyboardManagerV1 {
    fn create_keyboard(
        &self,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<State>,
    ) -> Box<dyn VirtualKeyboard> {
        Box::new(self.create_virtual_keyboard(seat, qh, ()))
    }
}

impl VirtualKeyboard for ZwpVirtualKeyboardV1 {
    fn keymap(&self, format: u32, fd: BorrowedFd, size: u32) {
        self.keymap(format, fd, size);
    }
    fn key(&self, time: u32, key: u32, state: u32) {
        self.key(time, key, state);
    }
    fn modifiers(&self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.modifiers(depressed, latched, locked, group);
    }
}

delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);
