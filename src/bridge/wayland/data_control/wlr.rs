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
    fn create_source(&self, qh: &QueueHandle<State>, published: Published) -> Self::Source {
        self.create_data_source(qh, published)
    }
    fn offer(source: &Self::Source, mime: String) {
        source.offer(mime);
    }
    fn set_selection(device: &Self::Device, sel: Sel, source: Option<&Self::Source>) {
        match sel {
            Sel::Clipboard => device.set_selection(source),
            Sel::Primary if device.version() >= 2 => device.set_primary_selection(source),
            Sel::Primary => {} // v1 has no primary selection
        }
    }
    fn carries(device: &Self::Device, sel: Sel) -> bool {
        // set_primary_selection arrived in v2
        sel == Sel::Clipboard || device.version() >= 2
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

impl Dispatch<ZwlrDataControlSourceV1, Published> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        published: &Published,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type: _, fd } => {
                crate::cliplog!(
                    "{:?} send: {} bytes to a receiver",
                    published.sel,
                    published.data.len()
                );
                state.queue_send(fd, published.data.clone());
            }
            zwlr_data_control_source_v1::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore ZwlrDataControlManagerV1);
