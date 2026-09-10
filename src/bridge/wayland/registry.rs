//! Registry binding: which globals we care about, at which version, and what
//! each one unblocks once it arrives.
//!
//! Globals show up in any order, so every arm binds and then re-runs the
//! relevant `try_init_*`/`maybe_start_*`, each of which no-ops until everything
//! it needs is present.

use wayland_protocols::ext::transient_seat::v1::client::ext_transient_seat_manager_v1::ExtTransientSeatManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::ZwlrOutputManagerV1;
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;

use super::*;

/// Binds a global with `()` user data, capped at the version we implement.
fn bind<I>(
    registry: &wl_registry::WlRegistry,
    name: u32,
    version: u32,
    max: u32,
    qh: &QueueHandle<State>,
) -> I
where
    I: Proxy + 'static,
    State: Dispatch<I, ()>,
{
    registry.bind(name, version.min(max), qh, ())
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let config = &state.server.config;
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == wl_output::WlOutput::interface().name {
                    let output =
                        registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
                    state.outputs.entry(name).or_default().proxy = Some(output);
                    state.ensure_xdg_output(name, qh);
                    state.ensure_output_power(name, qh);
                } else if interface == ZxdgOutputManagerV1::interface().name {
                    state.xdg_output_mgr = Some(bind(registry, name, version, 3, qh));
                    let names: Vec<u32> = state.outputs.keys().copied().collect();
                    for n in names {
                        state.ensure_xdg_output(n, qh);
                    }
                } else if interface == wl_shm::WlShm::interface().name {
                    state.shm = Some(bind(registry, name, version, 1, qh));
                    state.maybe_start_captures(qh);
                } else if interface == ExtOutputImageCaptureSourceManagerV1::interface().name
                    && config.screen.wants_imagecopy()
                {
                    state.ext_source_mgr = Some(bind(registry, name, version, 1, qh));
                    state.maybe_start_captures(qh);
                } else if interface == ExtImageCopyCaptureManagerV1::interface().name
                    && config.screen.wants_imagecopy()
                {
                    state.ext_capture_mgr = Some(bind(registry, name, version, 1, qh));
                    state.maybe_start_captures(qh);
                } else if interface == ZwlrScreencopyManagerV1::interface().name
                    && config.screen.wants_screencopy()
                {
                    state.screencopy = Some(bind(registry, name, version, 3, qh));
                    state.maybe_start_captures(qh);
                } else if interface == wl_seat::WlSeat::interface().name {
                    let seat =
                        registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, name);
                    state.seat_announced(name, seat, conn, qh);
                } else if interface == ExtTransientSeatManagerV1::interface().name {
                    state.has_transient_mgr = true;
                    if matches!(state.seat_mode, SeatMode::Transient) {
                        let mgr: ExtTransientSeatManagerV1 = bind(registry, name, version, 1, qh);
                        state.bind_transient_seat_manager(mgr, qh);
                    }
                } else if interface == ZwlrOutputPowerManagerV1::interface().name
                    && config.pwrmgr.wants_wlr()
                {
                    state.power_mgr = Some(bind(registry, name, version, 1, qh));
                    crate::log!("using zwlr-output-power-management-v1 to wake sleeping outputs");
                    let names: Vec<u32> = state.outputs.keys().copied().collect();
                    for n in names {
                        state.ensure_output_power(n, qh);
                    }
                } else if interface == ExtDataControlManagerV1::interface().name
                    && config.clipboard.wants_ext()
                {
                    state.ext_manager = Some(bind(registry, name, version, 1, qh));
                    state.try_init_device(conn, qh);
                } else if interface == ZwlrDataControlManagerV1::interface().name
                    && config.clipboard.wants_wlr()
                {
                    state.wlr_manager = Some(bind(registry, name, version, 2, qh));
                    state.try_init_device(conn, qh);
                } else if interface == ZwlrVirtualPointerManagerV1::interface().name {
                    let mgr: ZwlrVirtualPointerManagerV1 = bind(registry, name, version, 2, qh);
                    state.pointer_backend = Some(Box::new(mgr));
                    state.try_init_virtual_input(conn, qh);
                } else if interface == ZwlrOutputManagerV1::interface().name
                    && config.outmgr_wants_wlr()
                {
                    let mgr: ZwlrOutputManagerV1 = bind(registry, name, version, 4, qh);
                    crate::log!("using zwlr-output-management-v1 for dynamic resolution");
                    state.output_config = Some(OutputConfig::new(mgr));
                    state.server.dynres.set_available(true);
                } else if interface == ZwpVirtualKeyboardManagerV1::interface().name {
                    let mgr: ZwpVirtualKeyboardManagerV1 = bind(registry, name, version, 1, qh);
                    state.keyboard_backend = Some(Box::new(mgr));
                    state.try_init_virtual_input(conn, qh);
                }
            }
            wl_registry::Event::GlobalRemove { name } if state.outputs.contains_key(&name) => {
                state.remove_output(name);
            }
            wl_registry::Event::GlobalRemove { name } if state.transient_global() == Some(name) => {
                state.transient_seat_removed();
            }
            _ => {}
        }
    }
}
