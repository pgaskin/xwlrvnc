//! `zwlr-output-management-v1` client, for `-dynres`.
//!
//! Mirrors the compositor's heads and their modes, and turns the mode change an
//! X client asked for (parked in [`DynRes`](crate::bridge::dynres::DynRes))
//! into an output configuration, one at a time.
//!
//! Two protocol rules shape the code. A configuration must mention *every*
//! head — leaving one out is an `unconfigured_head` protocol error — so
//! applying a change to one output means re-enabling all the others unchanged.
//! And a configuration object is single-use: exactly one of
//! `succeeded`/`failed`/`cancelled` comes back, then it must be destroyed.
//!
//! Nothing here updates the RandR model directly. The compositor answers by
//! changing the `wl_output`, which reaches
//! [`State::apply_output`](super::State::apply_output) like any other output
//! change and rebuilds the screen, framebuffer, capture positions and input
//! layout from what actually happened. A request only counts as done once that
//! has landed, so the X client that is waiting for the reply sees the new state
//! when it wakes — and a compositor that says `succeeded` and then changes
//! nothing (niri's nested backend cannot change the mode of its window) is
//! reported as a failure rather than believed.

use std::time::{Duration, Instant};

use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1;
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_configuration_v1::{
    self as config_v1, ZwlrOutputConfigurationV1,
};
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_head_v1::{
    self as head_v1, ZwlrOutputHeadV1,
};
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_manager_v1::{
    self as manager_v1, ZwlrOutputManagerV1,
};
use wayland_protocols_wlr::output_management::v1::client::zwlr_output_mode_v1::{
    self as mode_v1, ZwlrOutputModeV1,
};

use super::*;
use crate::bridge::dynres::ModeRequest;

/// How long to wait for the first `done` (the serial a configuration needs)
/// after a request arrives.
const SERIAL_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for the compositor's `succeeded`/`failed`/`cancelled`.
const APPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// After `succeeded`, how long to wait for the `wl_output` to actually report
/// the requested size before concluding the compositor accepted the
/// configuration and did nothing with it.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long to wait for a fresh serial after `cancelled`.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);
/// A cancel means our serial went stale (an output changed under us), which a
/// retry fixes; a compositor that keeps cancelling would otherwise loop forever.
const MAX_CANCELLED: u8 = 3;
/// The refresh to ask for when the output's own is unknown.
const DEFAULT_REFRESH_MHZ: i32 = 60_000;

/// One of a head's advertised modes.
struct Mode {
    proxy: ZwlrOutputModeV1,
    width: i32,
    height: i32,
    refresh: i32, // mHz, 0 if not advertised
    preferred: bool,
}

/// One output as `wlr-output-management` sees it. Matched to our `wl_output`
/// by name, which is the connector name on both sides.
struct Head {
    proxy: ZwlrOutputHeadV1,
    name: Vec<u8>,
    enabled: bool,
    modes: Vec<Mode>,
}

/// One way of asking for a size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Attempt {
    /// An advertised mode of exactly that size (the one to use on real
    /// hardware, where a custom mode may not be possible).
    Advertised,
    /// `set_custom_mode` with this refresh. Compositors disagree on what they
    /// want here: wlroots' nested (wayland/x11) backends reject any refresh but
    /// 0 ("refresh rates are not supported"), while niri silently ignores a
    /// custom mode with refresh 0. So both are tried, whichever worked last
    /// time first.
    Custom { refresh: i32 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Waiting for a serial to create the configuration against.
    NeedSerial,
    /// Configuration applied; waiting for `succeeded`/`failed`/`cancelled`.
    Applying,
    /// `succeeded`; waiting for the `wl_output` to report the new size.
    Settling,
    /// `cancelled`; waiting for a newer serial to retry with.
    Cancelled,
}

/// The request being carried out.
struct Active {
    req: ModeRequest,
    head: ZwlrOutputHeadV1,
    /// The mode size in the panel's own orientation: the request is in screen
    /// pixels, and a rotated output's mode is the untransformed size.
    mode_w: i32,
    mode_h: i32,
    /// What is left to try, in order.
    attempts: Vec<Attempt>,
    current: Option<Attempt>,
    phase: Phase,
    deadline: Instant,
    config: Option<ZwlrOutputConfigurationV1>,
    serial_used: u32,
    cancelled: u8,
}

pub(super) struct OutputConfig {
    mgr: ZwlrOutputManagerV1,
    /// From the manager's `done`; `None` until the first one, which is also
    /// when the head list is first complete enough to configure.
    serial: Option<u32>,
    heads: Vec<Head>,
    active: Option<Active>,
    /// Whether a custom mode with refresh 0 is the variant to try first (see
    /// [`Attempt::Custom`]); flips to whatever worked last.
    zero_refresh_first: bool,
}

impl OutputConfig {
    pub(super) fn new(mgr: ZwlrOutputManagerV1) -> Self {
        Self {
            mgr,
            serial: None,
            heads: Vec::new(),
            active: None,
            zero_refresh_first: true,
        }
    }

    fn head_mut(&mut self, proxy: &ZwlrOutputHeadV1) -> Option<&mut Head> {
        self.heads.iter_mut().find(|h| &h.proxy == proxy)
    }

    /// The head owning `mode`, for the mode's own events.
    fn head_of_mode(&mut self, mode: &ZwlrOutputModeV1) -> Option<&mut Head> {
        self.heads
            .iter_mut()
            .find(|h| h.modes.iter().any(|m| &m.proxy == mode))
    }

    /// The head behind the output called `name`. `wl_output` only reports a
    /// name at version 4; with a single head there is nothing to disambiguate,
    /// which covers the headless/nested setup `-dynres` is meant for.
    fn head_named(&self, name: &[u8]) -> Option<&Head> {
        if !name.is_empty()
            && let Some(h) = self.heads.iter().find(|h| h.name == name)
        {
            return Some(h);
        }
        match self.heads.as_slice() {
            [only] if name.is_empty() || only.name.is_empty() => Some(only),
            _ => None,
        }
    }
}

impl Active {
    /// The attempts for a size: the advertised mode if there is one, then the
    /// custom-mode variants in preference order.
    fn plan(head: &Head, mode_w: i32, mode_h: i32, refresh: i32, zero_first: bool) -> Vec<Attempt> {
        let mut attempts = Vec::new();
        if head
            .modes
            .iter()
            .any(|m| m.width == mode_w && m.height == mode_h)
        {
            attempts.push(Attempt::Advertised);
        }
        let zero = Attempt::Custom { refresh: 0 };
        let own = Attempt::Custom {
            refresh: if refresh > 0 {
                refresh
            } else {
                DEFAULT_REFRESH_MHZ
            },
        };
        if zero_first {
            attempts.extend([zero, own]);
        } else {
            attempts.extend([own, zero]);
        }
        attempts
    }

    fn label(&self) -> String {
        format!(
            "{}x{} on output {}",
            self.req.width, self.req.height, self.req.wl_name
        )
    }
}

impl State {
    /// Picks up a parked request and drives the active one forward: called on
    /// every pass of the event loop (the wake fd makes a pass happen as soon
    /// as a request is parked), so the deadlines below need no timer of their
    /// own.
    pub(super) fn tick_dynres(&mut self, qh: &QueueHandle<Self>) {
        if self.output_config.is_none() {
            if let Some(req) = self.server.dynres.take() {
                crate::warning!(
                    "cannot change the resolution: the compositor has no zwlr_output_manager_v1"
                );
                self.server.dynres.complete(
                    req.id,
                    Err("the compositor has no zwlr_output_manager_v1".into()),
                );
            }
            return;
        }
        if self
            .output_config
            .as_ref()
            .is_some_and(|c| c.active.is_none())
            && let Some(req) = self.server.dynres.take()
        {
            self.dynres_start(req);
        }
        self.dynres_advance(qh);
    }

    /// Resolves a request against the current outputs and heads and queues its
    /// first attempt.
    fn dynres_start(&mut self, req: ModeRequest) {
        let Some(acc) = self.outputs.get(&req.wl_name) else {
            self.server
                .dynres
                .complete(req.id, Err("the output no longer exists".into()));
            return;
        };
        let (name, transform, refresh) = (acc.name.clone(), acc.transform, acc.refresh_mhz);
        let cfg = self.output_config.as_mut().expect("checked by caller");
        let Some(head) = cfg.head_named(&name) else {
            let reason = format!(
                "no zwlr_output_head_v1 named {:?} (heads: {})",
                String::from_utf8_lossy(&name),
                cfg.heads
                    .iter()
                    .map(|h| String::from_utf8_lossy(&h.name).into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            crate::warning!("cannot change the resolution: {reason}");
            self.server.dynres.complete(req.id, Err(reason));
            return;
        };
        // the request is the size on screen; the mode is the panel's own
        // orientation, which a 90/270 transform swaps
        let (mode_w, mode_h) = if transform.swaps_axes() {
            (i32::from(req.height), i32::from(req.width))
        } else {
            (i32::from(req.width), i32::from(req.height))
        };
        let attempts = Active::plan(head, mode_w, mode_h, refresh, cfg.zero_refresh_first);
        crate::vlog!(
            "resolution change {}x{} on output {} ({:?}): trying {:?}",
            req.width,
            req.height,
            req.wl_name,
            String::from_utf8_lossy(&name),
            attempts
        );
        cfg.active = Some(Active {
            req,
            head: head.proxy.clone(),
            mode_w,
            mode_h,
            attempts,
            current: None,
            phase: Phase::NeedSerial,
            deadline: Instant::now() + SERIAL_TIMEOUT,
            config: None,
            serial_used: 0,
            cancelled: 0,
        });
    }

    /// Moves the active request along: applies when a serial is there,
    /// re-applies after a cancel, and enforces the deadlines.
    fn dynres_advance(&mut self, qh: &QueueHandle<Self>) {
        let Some(cfg) = &self.output_config else {
            return;
        };
        let Some(active) = &cfg.active else { return };
        let now = Instant::now();
        match active.phase {
            Phase::NeedSerial => {
                if cfg.serial.is_some() {
                    self.dynres_apply_next(qh);
                } else if now >= active.deadline {
                    self.dynres_fail("the output manager never sent a serial");
                }
            }
            Phase::Applying => {
                if now >= active.deadline {
                    self.dynres_fail("the compositor did not answer the configuration");
                }
            }
            Phase::Settling => {
                if self.dynres_settled() {
                    self.dynres_succeed();
                } else if now >= active.deadline {
                    crate::vlog!(
                        "the compositor accepted {:?} but the output did not change",
                        active.current
                    );
                    self.dynres_apply_next(qh);
                }
            }
            Phase::Cancelled => {
                if cfg.serial.is_some_and(|s| s != active.serial_used) {
                    self.dynres_apply_next(qh);
                } else if now >= active.deadline {
                    self.dynres_fail("the compositor kept cancelling the configuration");
                }
            }
        }
    }

    /// Applies the next attempt, or fails the request when none is left.
    fn dynres_apply_next(&mut self, qh: &QueueHandle<Self>) {
        let Some(cfg) = &mut self.output_config else {
            return;
        };
        let Some(active) = &mut cfg.active else {
            return;
        };
        let Some(serial) = cfg.serial else { return };
        if active.attempts.is_empty() {
            self.dynres_fail("the compositor rejected every way of setting the mode");
            return;
        }
        let attempt = active.attempts.remove(0);
        let config = cfg.mgr.create_configuration(serial, qh, ());
        for head in &cfg.heads {
            // every head must be configured, so the ones we are not touching get
            // enabled with no property changes, which keeps their current state
            if !head.enabled && head.proxy != active.head {
                config.disable_head(&head.proxy);
                continue;
            }
            let ch = config.enable_head(&head.proxy, qh, ());
            if head.proxy != active.head {
                continue;
            }
            match attempt {
                Attempt::Advertised => {
                    // the preferred one, else the fastest, at that size
                    let mode = head
                        .modes
                        .iter()
                        .filter(|m| m.width == active.mode_w && m.height == active.mode_h)
                        .max_by_key(|m| (m.preferred, m.refresh));
                    match mode {
                        Some(m) => ch.set_mode(&m.proxy),
                        None => ch.set_custom_mode(active.mode_w, active.mode_h, 0),
                    }
                }
                Attempt::Custom { refresh } => {
                    ch.set_custom_mode(active.mode_w, active.mode_h, refresh);
                }
            }
        }
        config.apply();
        crate::vlog!(
            "applying {:?} for {} (mode {}x{}, serial {serial})",
            attempt,
            active.label(),
            active.mode_w,
            active.mode_h
        );
        if let Some(old) = active.config.replace(config) {
            old.destroy();
        }
        active.current = Some(attempt);
        active.phase = Phase::Applying;
        active.deadline = Instant::now() + APPLY_TIMEOUT;
        active.serial_used = serial;
    }

    /// Whether the RandR model (fed by `wl_output`) now shows the active
    /// request's size on its output.
    fn dynres_settled(&self) -> bool {
        let Some(active) = self.output_config.as_ref().and_then(|c| c.active.as_ref()) else {
            return false;
        };
        self.server
            .screen
            .lock()
            .unwrap()
            .output_by_wl_name(active.req.wl_name)
            .is_some_and(|o| o.width == active.req.width && o.height == active.req.height)
    }

    /// Called after an output's `wl_output` state was applied to the model: the
    /// compositor's answer to the active request arrives this way.
    pub(super) fn dynres_output_changed(&mut self, wl_name: u32) {
        let settling = self
            .output_config
            .as_ref()
            .and_then(|c| c.active.as_ref())
            .is_some_and(|a| a.req.wl_name == wl_name && a.phase == Phase::Settling);
        if settling && self.dynres_settled() {
            self.dynres_succeed();
        }
    }

    /// Called when an output's `wl_output` global went away.
    pub(super) fn dynres_output_removed(&mut self, wl_name: u32) {
        let hit = self
            .output_config
            .as_ref()
            .and_then(|c| c.active.as_ref())
            .is_some_and(|a| a.req.wl_name == wl_name);
        if hit {
            self.dynres_fail("the output went away");
        }
    }

    fn dynres_take_active(&mut self) -> Option<Active> {
        let mut active = self.output_config.as_mut()?.active.take()?;
        if let Some(config) = active.config.take() {
            config.destroy();
        }
        Some(active)
    }

    fn dynres_succeed(&mut self) {
        let Some(active) = self.dynres_take_active() else {
            return;
        };
        if let (Some(Attempt::Custom { refresh }), Some(cfg)) =
            (active.current, self.output_config.as_mut())
        {
            cfg.zero_refresh_first = refresh == 0;
        }
        crate::log!(
            "resolution change applied: {} ({:?})",
            active.label(),
            active.current
        );
        self.server.dynres.complete(active.req.id, Ok(()));
    }

    pub(super) fn dynres_fail(&mut self, reason: &str) {
        let Some(active) = self.dynres_take_active() else {
            return;
        };
        crate::warning!("resolution change failed: {}: {reason}", active.label());
        self.server
            .dynres
            .complete(active.req.id, Err(reason.to_string()));
    }

    /// The compositor's answer to a configuration.
    fn dynres_config_event(
        &mut self,
        config: &ZwlrOutputConfigurationV1,
        event: config_v1::Event,
        qh: &QueueHandle<Self>,
    ) {
        let Some(active) = self.output_config.as_mut().and_then(|c| c.active.as_mut()) else {
            config.destroy();
            return;
        };
        if active.config.as_ref() != Some(config) {
            // a leftover from an attempt already given up on
            config.destroy();
            return;
        }
        active.config = None;
        config.destroy();
        match event {
            config_v1::Event::Succeeded => {
                // the compositor now re-describes the output; the wl_output
                // change rebuilds our screen from what it actually did, and
                // the request is done once that shows the requested size
                active.phase = Phase::Settling;
                active.deadline = Instant::now() + SETTLE_TIMEOUT;
                if self.dynres_settled() {
                    self.dynres_succeed();
                }
            }
            config_v1::Event::Failed => {
                crate::vlog!("the compositor rejected {:?}", active.current);
                self.dynres_apply_next(qh);
            }
            // our serial went stale (an output changed under us), so retry the
            // same attempt once the fresh `done` has landed
            config_v1::Event::Cancelled => {
                active.cancelled += 1;
                if active.cancelled > MAX_CANCELLED {
                    self.dynres_fail("the compositor kept cancelling the configuration");
                    return;
                }
                if let Some(cur) = active.current.take() {
                    active.attempts.insert(0, cur);
                }
                active.phase = Phase::Cancelled;
                active.deadline = Instant::now() + CANCEL_TIMEOUT;
                crate::vlog!("configuration cancelled (stale serial); retrying");
                self.dynres_advance(qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _mgr: &ZwlrOutputManagerV1,
        event: manager_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(cfg) = &mut state.output_config else {
            return;
        };
        match event {
            manager_v1::Event::Head { head } => cfg.heads.push(Head {
                proxy: head,
                name: Vec::new(),
                enabled: false,
                modes: Vec::new(),
            }),
            // one atomic batch of head/mode state has landed; the serial is what
            // a configuration must be created against
            manager_v1::Event::Done { serial } => {
                cfg.serial = Some(serial);
                state.dynres_advance(qh);
            }
            manager_v1::Event::Finished => {
                crate::warning!(
                    "the compositor withdrew zwlr_output_manager_v1; dynamic resolution is off"
                );
                state.dynres_fail("the compositor withdrew the output manager");
                state.output_config = None;
                state.server.dynres.set_available(false);
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputManagerV1, [
        manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        head: &ZwlrOutputHeadV1,
        event: head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(cfg) = &mut state.output_config else {
            return;
        };
        if let head_v1::Event::Finished = event {
            if let Some(i) = cfg.heads.iter().position(|h| &h.proxy == head) {
                let h = cfg.heads.remove(i);
                for m in &h.modes {
                    if m.proxy.version() >= 3 {
                        m.proxy.release();
                    }
                }
                if h.proxy.version() >= 3 {
                    h.proxy.release();
                }
            }
            if cfg.active.as_ref().is_some_and(|a| &a.head == head) {
                state.dynres_fail("the output went away");
            }
            return;
        }
        let Some(h) = cfg.head_mut(head) else { return };
        match event {
            head_v1::Event::Name { name } => h.name = name.into_bytes(),
            head_v1::Event::Enabled { enabled } => h.enabled = enabled != 0,
            head_v1::Event::Mode { mode } => h.modes.push(Mode {
                proxy: mode,
                width: 0,
                height: 0,
                refresh: 0,
                preferred: false,
            }),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputHeadV1, [
        head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        state: &mut Self,
        mode: &ZwlrOutputModeV1,
        event: mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(cfg) = &mut state.output_config else {
            return;
        };
        if let mode_v1::Event::Finished = event {
            if let Some(h) = cfg.head_of_mode(mode) {
                h.modes.retain(|m| &m.proxy != mode);
            }
            if mode.version() >= 3 {
                mode.release();
            }
            return;
        }
        let Some(h) = cfg.head_of_mode(mode) else {
            return;
        };
        let Some(m) = h.modes.iter_mut().find(|m| &m.proxy == mode) else {
            return;
        };
        match event {
            mode_v1::Event::Size { width, height } => {
                m.width = width;
                m.height = height;
            }
            mode_v1::Event::Refresh { refresh } => m.refresh = refresh,
            mode_v1::Event::Preferred => m.preferred = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        config: &ZwlrOutputConfigurationV1,
        event: config_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        state.dynres_config_event(config, event, qh);
    }
}

delegate_noop!(State: ignore ZwlrOutputConfigurationHeadV1);
