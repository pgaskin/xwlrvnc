//! Clipboard bridge over the data-control protocols: `ext-data-control-v1`
//! (preferred) or `zwlr-data-control-v1` (fallback).
//!
//! The two are equivalent for our purposes, so the manager-specific bits live
//! behind the [`DataControlManager`] trait, implemented per protocol in [`ext`]
//! and [`wlr`], and the X-to-Wayland source factory is written once over that
//! trait in [`install_source_factory`]. The `Dispatch` impls have to stay
//! concrete, but they all defer to [`State::update_selection`].

use wayland_client::protocol::wl_seat;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::ExtDataControlDeviceV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1;

use super::*;

mod ext;
mod wlr;

/// Opcode of the `data_offer` event, which creates the child offer object. The
/// same value on both device interfaces.
const DATA_OFFER_OPCODE: u16 = 0;

/// The live device, whichever protocol won. Held rather than read: dropping the
/// proxy would destroy the device.
#[allow(dead_code)]
pub(crate) enum DataDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

/// The manager side of a data-control protocol: enough to create the per-seat
/// device, mint a source for an X-owned selection, and publish it.
trait DataControlManager: Clone + Send + Sync + 'static {
    type Device: Clone + Send + Sync + 'static;
    type Source: Clone;

    /// Binds the device for `seat`, kept alive in [`DataDevice`].
    fn create_device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> Self::Device;
    /// Mints a source tagged with `sel` as its `Dispatch` userdata.
    fn create_source(&self, qh: &QueueHandle<State>, sel: Sel) -> Self::Source;
    /// Advertises a mime type on the source.
    fn offer(source: &Self::Source, mime: String);
    /// Sets `sel`'s selection on the device to `source`.
    fn set_selection(device: &Self::Device, sel: Sel, source: &Self::Source);
}

/// Installs the source factory: when X takes a selection, mint a data-control
/// source advertising our text mimes and publish it.
fn install_source_factory<M: DataControlManager>(
    mgr: M,
    device: M::Device,
    conn: Connection,
    qh: QueueHandle<State>,
    clipboard: &clipboard::Clipboard,
) {
    clipboard.set_source_factory(Box::new(move |sel| {
        let source = mgr.create_source(&qh, sel);
        for m in clipboard::TEXT_MIMES {
            M::offer(&source, (*m).to_string());
        }
        M::set_selection(&device, sel, &source);
        let _ = conn.flush();
    }));
}

impl State {
    pub(super) fn try_init_device(&mut self, conn: &Connection, qh: &QueueHandle<Self>) {
        // an ext device is already the best we can do
        if matches!(self.device, Some(DataDevice::Ext(_))) {
            return;
        }
        let Some(seat) = &self.seat else { return };

        if let Some(mgr) = &self.ext_manager {
            // ext is preferred, so replace any wlr device we already made
            crate::log!("using ext-data-control-v1");
            let device = mgr.create_device(seat, qh);
            install_source_factory(
                mgr.clone(),
                device.clone(),
                conn.clone(),
                qh.clone(),
                &self.server.clipboard,
            );
            self.device = Some(DataDevice::Ext(device));
        } else if self.device.is_none() {
            let Some(mgr) = &self.wlr_manager else { return };
            crate::log!("using zwlr-data-control-v1");
            let device = mgr.create_device(seat, qh);
            install_source_factory(
                mgr.clone(),
                device.clone(),
                conn.clone(),
                qh.clone(),
                &self.server.clipboard,
            );
            self.device = Some(DataDevice::Wlr(device));
        }
    }

    /// Records a selection change and notifies the X clients watching it.
    pub(super) fn update_selection(&mut self, sel: Sel, offer: Option<DataOffer>) {
        // with -noprimary, ignore PRIMARY entirely and just tidy up the offer
        // the compositor handed us
        if sel == Sel::Primary && self.server.config.noprimary {
            if let Some(o) = offer {
                self.offer_mimes.remove(&o.id());
                o.destroy();
            }
            return;
        }
        let mimes = offer
            .as_ref()
            .and_then(|o| self.offer_mimes.remove(&o.id()))
            .unwrap_or_default();
        // a foreign app taking the selection revokes any X owner, but the
        // compositor announces the source we published through here too
        if !self.server.clipboard.take_self_published(sel) {
            self.server.clipboard.clear_x_owner(sel);
        }
        let owner = if offer.is_some() {
            clipboard::OWNER_WINDOW
        } else {
            0
        };
        let (serial, old) = self.server.clipboard.set_offer(sel, offer, mimes);
        if let Some(old) = old {
            old.destroy();
        }
        self.server.events.selection_changed(sel, owner, serial);
    }
}
