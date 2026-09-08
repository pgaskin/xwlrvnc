//! `zwlr-data-control-v1` backend: the [`DataControlManager`] impl plus the
//! device, offer and source `Dispatch` impls.

use wayland_client::protocol::wl_seat;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::{
    self, ZwlrDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::{
    self, ZwlrDataControlSourceV1,
};

use super::*;

impl DataControlManager for ZwlrDataControlManagerV1 {
    type Device = ZwlrDataControlDeviceV1;
    type Source = ZwlrDataControlSourceV1;

    fn create_device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> Self::Device {
        self.get_data_device(seat, qh, ())
    }
    fn create_source(&self, qh: &QueueHandle<State>, sel: Sel) -> Self::Source {
        self.create_data_source(qh, sel)
    }
    fn offer(source: &Self::Source, mime: String) {
        source.offer(mime);
    }
    fn set_selection(device: &Self::Device, sel: Sel, source: &Self::Source) {
        match sel {
            Sel::Clipboard => device.set_selection(Some(source)),
            Sel::Primary if device.version() >= 2 => device.set_primary_selection(Some(source)),
            Sel::Primary => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.offer_mimes.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => {
                state.update_selection(Sel::Clipboard, id.map(DataOffer::Wlr))
            }
            Event::PrimarySelection { id } => {
                state.update_selection(Sel::Primary, id.map(DataOffer::Wlr))
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state
                .offer_mimes
                .entry(offer.id())
                .or_default()
                .push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, Sel> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        sel: &Sel,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type: _, fd } => {
                let data = state.server.clipboard.x_data(*sel);
                state.queue_send(fd, data);
            }
            zwlr_data_control_source_v1::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore ZwlrDataControlManagerV1);
