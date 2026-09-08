//! `ext-data-control-v1` backend: the [`DataControlManager`] impl plus the
//! device, offer and source `Dispatch` impls.

use wayland_client::protocol::wl_seat;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::{
    self as ext_device_v1, ExtDataControlDeviceV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{
    self as ext_offer_v1, ExtDataControlOfferV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{
    self as ext_source_v1, ExtDataControlSourceV1,
};

use super::*;

impl DataControlManager for ExtDataControlManagerV1 {
    type Device = ExtDataControlDeviceV1;
    type Source = ExtDataControlSourceV1;

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
            Sel::Primary => device.set_primary_selection(Some(source)),
        }
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _device: &ExtDataControlDeviceV1,
        event: ext_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.offer_mimes.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => {
                state.update_selection(Sel::Clipboard, id.map(DataOffer::Ext))
            }
            Event::PrimarySelection { id } => {
                state.update_selection(Sel::Primary, id.map(DataOffer::Ext))
            }
            Event::Finished => crate::vlog!("ext-data-control device finished"),
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_offer_v1::Event::Offer { mime_type } = event {
            state
                .offer_mimes
                .entry(offer.id())
                .or_default()
                .push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Sel> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_source_v1::Event,
        sel: &Sel,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_source_v1::Event::Send { mime_type: _, fd } => {
                let data = state.server.clipboard.x_data(*sel);
                state.queue_send(fd, data);
            }
            ext_source_v1::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore ExtDataControlManagerV1);
