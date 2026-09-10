//! `zwlr-output-power-management-v1` client, for `-pwrmgr`.
//!
//! An output the compositor (usually its idle daemon) has powered off is
//! disabled as far as capture is concerned: wlroots fails every screencopy on
//! it, so a client connecting to a machine whose monitor went to sleep sees a
//! frozen screen until someone moves the local mouse. Holding a power control
//! per output tells us when that is the case, and lets us turn the output back
//! on when an X client starts watching the screen, or when it goes off while
//! one is watching.
//!
//! Nothing is ever turned off again: the idle daemon will do that on its own
//! schedule, and turning it off ourselves when the client leaves could black
//! out a local user who has since sat down.

use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1;
use wayland_protocols_wlr::output_power_management::v1::client::zwlr_output_power_v1::{
    self, Mode, ZwlrOutputPowerV1,
};

use super::*;
use crate::config::PwrMgrType;

/// One output's power control.
pub(super) struct OutputPower {
    /// `None` once the compositor sent `failed` (another client holds it, or
    /// the output cannot be controlled), so it is not asked again.
    proxy: Option<ZwlrOutputPowerV1>,
    /// The last `mode` from the compositor; `None` until the first.
    on: Option<bool>,
}

impl State {
    /// Creates an output's power control once both it and the manager exist.
    /// Idempotent, since either can arrive first.
    pub(super) fn ensure_output_power(&mut self, wl_name: u32, qh: &QueueHandle<Self>) {
        let Some(mgr) = &self.power_mgr else { return };
        let Some(acc) = self.outputs.get_mut(&wl_name) else {
            return;
        };
        if acc.power.is_some() {
            return;
        }
        let Some(output) = &acc.proxy else { return };
        acc.power = Some(OutputPower {
            proxy: Some(mgr.get_output_power(output, qh, wl_name)),
            on: None,
        });
    }

    /// The initial globals have all arrived: say so if the manager is missing.
    pub(super) fn output_power_settled(&self) {
        let choice = self.server.config.pwrmgr;
        if self.power_mgr.is_some() || !choice.wants_wlr() {
            return;
        }
        if choice == PwrMgrType::Wlr {
            crate::warning!(
                "the compositor has no zwlr_output_power_manager_v1; sleeping outputs will not be \
                 woken for capture"
            );
        } else {
            crate::vlog!("no zwlr_output_power_manager_v1; sleeping outputs will not be woken");
        }
    }

    /// Turns on every output known to be off.
    pub(super) fn wake_outputs(&self, why: &str) {
        for &wl_name in self.outputs.keys() {
            self.wake_output(wl_name, why);
        }
    }

    /// Turns an output on if the compositor last said it was off.
    fn wake_output(&self, wl_name: u32, why: &str) {
        let Some(power) = self.outputs.get(&wl_name).and_then(|a| a.power.as_ref()) else {
            return;
        };
        if let (Some(proxy), Some(false)) = (&power.proxy, power.on) {
            crate::log!("output {wl_name} is off; turning it on ({why})");
            proxy.set_mode(Mode::On);
        }
    }

    /// Releases an output's control when the output goes away.
    pub(super) fn drop_output_power(acc: &mut OutputAcc) {
        if let Some(proxy) = acc.power.take().and_then(|p| p.proxy) {
            proxy.destroy();
        }
    }
}

impl Dispatch<ZwlrOutputPowerV1, u32> for State {
    fn event(
        state: &mut Self,
        _power: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        &wl_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(power) = state
            .outputs
            .get_mut(&wl_name)
            .and_then(|a| a.power.as_mut())
        else {
            return;
        };
        match event {
            zwlr_output_power_v1::Event::Mode { mode } => {
                let on = matches!(mode, WEnum::Value(Mode::On));
                let changed = power.on.replace(on) != Some(on);
                if changed {
                    crate::vlog!(
                        "output {wl_name} power is {}",
                        if on { "on" } else { "off" }
                    );
                }
                // went to sleep under a watching client (its idle daemon does not
                // count remote viewing as activity), so wake it straight back up.
                // Only on the transition: sway repeats `off` on every commit, and
                // a compositor refusing to wake would otherwise be asked forever.
                if changed && !on && state.capture_active {
                    state.wake_output(wl_name, "screen capture is active");
                }
            }
            zwlr_output_power_v1::Event::Failed => {
                crate::vlog!("output {wl_name} power control failed; it will not be woken");
                if let Some(proxy) = power.proxy.take() {
                    proxy.destroy();
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore ZwlrOutputPowerManagerV1);
