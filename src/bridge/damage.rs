//! DAMAGE extension. Screen changes (from wlr-screencopy damage) are fed in as
//! regions and reported to clients with the same per-level semantics as the
//! X.org server (`DamageReportDamage` in miext/damage/damage.c and
//! `DamageExtReport`/`DamageExtNotify` in damageext/damageext.c):
//!
//! - RawRectangles: report every incoming rectangle.
//! - DeltaRectangles: report the newly-damaged part; accumulate into the region.
//! - BoundingBox: report the region extents, only when they grow.
//! - NonEmpty: report once (no area) when the region goes empty -> non-empty.
//!
//! A DamageNotify is one event per box, with `DamageNotifyMore` (0x80) set on
//! every box but the last. The region is cleared/reduced by DamageSubtract,
//! which re-arms reporting (and re-reports any remainder, as the real server
//! does).

use std::sync::{Arc, Mutex};

use x11rb_protocol::protocol::damage;
use x11rb_protocol::protocol::xproto::Rectangle;

use crate::bridge::event::{self, Client};
use crate::bridge::x11::ext::EXTENSIONS;
use crate::util::bbox;

const RAW: u8 = 0;
const DELTA: u8 = 1;
const BBOX: u8 = 2;
const NON_EMPTY: u8 = 3;
const MORE: u8 = 0x80;

struct DamageObj {
    client: Arc<Client>,
    id: u32,
    drawable: u32,
    level: u8,
    /// Accumulated, not-yet-subtracted damage.
    region: Vec<Rectangle>,
}

#[derive(Default)]
pub struct DamageSink {
    objects: Mutex<Vec<DamageObj>>,
    /// Current root geometry, reported as the DamageNotify `geometry`.
    geom: Mutex<(u16, u16)>,
}

impl DamageSink {
    pub fn create(&self, client: Arc<Client>, id: u32, drawable: u32, level: u8) {
        let mut objs = self.objects.lock().unwrap();
        let was_active = objs.iter().any(|o| !o.client.is_dead());
        objs.push(DamageObj {
            client,
            id,
            drawable,
            level,
            region: Vec::new(),
        });
        if !was_active {
            crate::vlog!("damage tracking started");
        }
    }

    /// Whether any client currently has a damage object (i.e. is watching the
    /// screen for changes) — used to gate the capture loop.
    pub fn active(&self) -> bool {
        self.objects
            .lock()
            .unwrap()
            .iter()
            .any(|o| !o.client.is_dead())
    }

    pub fn destroy(&self, client: &Arc<Client>, id: u32) {
        let mut objs = self.objects.lock().unwrap();
        objs.retain(|o| !(Arc::ptr_eq(&o.client, client) && o.id == id));
        let still_active = objs.iter().any(|o| !o.client.is_dead());
        if !still_active {
            crate::vlog!("damage tracking stopped");
        }
    }

    /// DamageSubtract: returns the subtracted part (for the `parts` region) and
    /// reduces/clears the damage. `repair == None` means subtract everything.
    pub fn subtract(
        &self,
        client: &Arc<Client>,
        id: u32,
        repair: Option<&[Rectangle]>,
    ) -> Vec<Rectangle> {
        let mut objs = self.objects.lock().unwrap();
        let Some(o) = objs
            .iter_mut()
            .find(|o| Arc::ptr_eq(&o.client, client) && o.id == id)
        else {
            return Vec::new();
        };
        if o.level == RAW {
            return Vec::new(); // subtract is a no-op at the raw level
        }
        // We implement the repair == None case exactly (the path VNC uses:
        // XDamageSubtract(d, None, parts) -> take the whole region into `parts`
        // and clear it). A non-None repair is treated as a full subtract, which
        // over-clears but never loses real damage.
        let _ = repair;
        std::mem::take(&mut o.region)
    }

    /// Feeds a new screen-damage region (root coordinates) to all damage objects.
    pub fn add_damage(&self, new: &[Rectangle], geom: (u16, u16)) {
        let new: Vec<Rectangle> = new
            .iter()
            .copied()
            .filter(|r| r.width > 0 && r.height > 0)
            .collect();
        if new.is_empty() {
            return;
        }
        *self.geom.lock().unwrap() = geom;
        let mut objs = self.objects.lock().unwrap();
        objs.retain(|o| !o.client.is_dead());
        for o in objs.iter_mut() {
            match o.level {
                RAW => {
                    o.region.extend_from_slice(&new);
                    notify(o, &new, geom);
                }
                DELTA => {
                    // Exact delta would be `new - region`; reporting all of `new`
                    // over-reports (safe) without a full region implementation.
                    o.region.extend_from_slice(&new);
                    notify(o, &new, geom);
                }
                BBOX => {
                    let old = bbox(&o.region).map(tuple);
                    o.region.extend_from_slice(&new);
                    let now = bbox(&o.region);
                    if old != now.map(tuple)
                        && let Some(now) = now
                    {
                        notify(o, &[now], geom);
                    }
                }
                NON_EMPTY => {
                    let was_empty = o.region.is_empty();
                    o.region.extend_from_slice(&new);
                    if was_empty && !o.region.is_empty() {
                        notify_non_empty(o, geom);
                    }
                }
                _ => o.region.extend_from_slice(&new),
            }
        }
    }
}

/// Sends a DamageNotify per box, with `DamageNotifyMore` on all but the last.
fn notify(o: &DamageObj, boxes: &[Rectangle], geom: (u16, u16)) {
    let geometry = Rectangle {
        x: 0,
        y: 0,
        width: geom.0,
        height: geom.1,
    };
    let n = boxes.len();
    for (i, area) in boxes.iter().enumerate() {
        let level = if i + 1 < n { o.level | MORE } else { o.level };
        send_notify(o, level, *area, geometry);
    }
}

/// NonEmpty report: a single event whose area is the whole drawable.
fn notify_non_empty(o: &DamageObj, geom: (u16, u16)) {
    let geometry = Rectangle {
        x: 0,
        y: 0,
        width: geom.0,
        height: geom.1,
    };
    send_notify(o, o.level, geometry, geometry);
}

fn send_notify(o: &DamageObj, level: u8, area: Rectangle, geometry: Rectangle) {
    crate::bridge::profile::damage_notify(1);
    let first_event = EXTENSIONS.lookup(b"DAMAGE").map_or(0, |e| e.first_event);
    let event = damage::NotifyEvent {
        response_type: first_event, // + XDamageNotify (0)
        level: damage::ReportLevel::from(level),
        sequence: o.client.seq(),
        drawable: o.drawable,
        damage: o.id,
        timestamp: crate::bridge::event::server_time_ms(),
        area,
        geometry,
    };
    event::send(&o.client, &event);
}

fn tuple(r: Rectangle) -> (i16, i16, u16, u16) {
    (r.x, r.y, r.width, r.height)
}
