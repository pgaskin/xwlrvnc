use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

use super::*;

impl PointerBackend for ZwlrVirtualPointerManagerV1 {
    fn create_pointer(
        &self,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<State>,
    ) -> Box<dyn VirtualPointer> {
        Box::new(self.create_virtual_pointer(Some(seat), qh, ()))
    }
}

impl VirtualPointer for ZwlrVirtualPointerV1 {
    fn motion_absolute(&self, time: u32, x: u32, y: u32, extent_x: u32, extent_y: u32) {
        self.motion_absolute(time, x, y, extent_x, extent_y);
    }
    fn motion(&self, time: u32, dx: f64, dy: f64) {
        self.motion(time, dx, dy);
    }
    fn button(&self, time: u32, button: u32, state: ButtonState) {
        self.button(time, button, state);
    }
    fn axis(&self, time: u32, axis: Axis, value: f64) {
        self.axis(time, axis, value);
    }
    fn axis_source(&self, source: AxisSource) {
        self.axis_source(source);
    }
    fn axis_discrete(&self, time: u32, axis: Axis, value: f64, discrete: i32) {
        self.axis_discrete(time, axis, value, discrete);
    }
    fn frame(&self) {
        self.frame();
    }
}

delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
