//! Clipboard bridge over the data-control protocols: `ext-data-control-v1`
//! (preferred) or `zwlr-data-control-v1` (fallback).
//!
//! The two protocols are byte-for-byte equivalent for our purposes, so the
//! manager-specific bits (creating the device/source, setting selections) live
//! behind the [`DataControlManager`] trait — implemented per protocol in the
//! [`ext`] and [`wlr`] submodules — and the X→Wayland source factory is written
//! once, generically, in [`install_source_factory`]. The per-interface
//! `Dispatch` impls (also in the submodules) stay concrete but defer to the
//! shared [`State::update_selection`] bookkeeping.

use wayland_client::protocol::wl_seat;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::ExtDataControlDeviceV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1;

use super::*;

mod ext;
mod wlr;

/// Opcode of the `data_offer` event on both data-control device interfaces
/// (creates a child offer object); same value for wlr and ext.
const DATA_OFFER_OPCODE: u16 = 0;

/// The live data-control device, whichever protocol we negotiated. The inner
/// proxy is kept alive (not read) so the compositor doesn't destroy the device.
#[allow(dead_code)]
pub(crate) enum DataDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

/// The manager-side of a data-control protocol: enough to create the per-seat
/// device, mint a source for an X-owned selection, and publish it. Implemented
/// for both the ext and wlr managers (in [`ext`] / [`wlr`]) so the X→Wayland
/// source factory is shared.
trait DataControlManager: Clone + Send + Sync + 'static {
    type Device: Clone + Send + Sync + 'static;
    type Source: Clone;

    /// Binds the device for `seat` (kept alive in [`DataDevice`]).
    fn create_device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> Self::Device;
    /// Mints a source tagged with `sel` (its `Dispatch` userdata).
    fn create_source(&self, qh: &QueueHandle<State>, sel: Sel) -> Self::Source;
    /// Advertises a mime type on the source.
    fn offer(source: &Self::Source, mime: String);
    /// Sets `sel`'s selection on the device to `source`.
    fn set_selection(device: &Self::Device, sel: Sel, source: &Self::Source);
}

/// Installs the X→Wayland source factory: when X takes a selection, mint a
/// data-control source advertising our text mimes and publish it. Written once
/// over any [`DataControlManager`].
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
        // If we already have an ext device, nothing to do — it's already optimal.
        if matches!(self.device, Some(DataDevice::Ext(_))) {
            return;
        }
        let Some(seat) = &self.seat else { return };

        if let Some(mgr) = &self.ext_manager {
            // ext-data-control-v1 is preferred; replace any existing wlr device.
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

    /// Records a clipboard/primary selection change and notifies X clients.
    pub(super) fn update_selection(&mut self, sel: Sel, offer: Option<DataOffer>) {
        // With -noprimary, ignore the Wayland PRIMARY selection entirely (just
        // tidy up the offer the compositor handed us).
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
