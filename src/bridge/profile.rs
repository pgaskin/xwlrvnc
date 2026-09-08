//! Lightweight runtime profiling, enabled by `-profile`.
//!
//! Every hot-path hook is a cheap no-op while disabled. The 1Hz reporter gets
//! its own thread so it keeps printing even when the capture or X threads are
//! stalled, which is exactly the case worth catching.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATS: Mutex<Stats> = Mutex::new(Stats::new());

#[derive(Clone)]
struct Stats {
    // capture loop
    frames: u32,
    blit_copy_sum_ns: u64,
    blit_copy_max_ns: u64,
    blit_wait_sum_ns: u64,
    blit_wait_max_ns: u64,
    frame_gap_max_ns: u64,
    req_lat_sum_ns: u64,
    req_lat_max_ns: u64,
    // X-side framebuffer reads (GetImage / ShmGetImage)
    reads: u32,
    read_bytes: u64,
    read_wait_sum_ns: u64,
    read_wait_max_ns: u64,
    read_hold_sum_ns: u64,
    read_hold_max_ns: u64,
    // DAMAGE protocol
    dmg_notify: u32,
    dmg_subtract: u32,
    dmg_create: u32,
}

impl Stats {
    const fn new() -> Self {
        Self {
            frames: 0,
            blit_copy_sum_ns: 0,
            blit_copy_max_ns: 0,
            blit_wait_sum_ns: 0,
            blit_wait_max_ns: 0,
            frame_gap_max_ns: 0,
            req_lat_sum_ns: 0,
            req_lat_max_ns: 0,
            reads: 0,
            read_bytes: 0,
            read_wait_sum_ns: 0,
            read_wait_max_ns: 0,
            read_hold_sum_ns: 0,
            read_hold_max_ns: 0,
            dmg_notify: 0,
            dmg_subtract: 0,
            dmg_create: 0,
        }
    }
}

static LAST_READY: Mutex<Option<Instant>> = Mutex::new(None); // outlives the window, so gaps are continuous

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Starts the 1Hz reporter thread if profiling is enabled.
pub fn start(enabled: bool) {
    if !enabled {
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
    std::thread::spawn(|| {
        let mut prev = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let now = Instant::now();
            let dt = now.duration_since(prev).as_secs_f64();
            prev = now;
            let s = {
                let mut g = STATS.lock().unwrap();
                std::mem::replace(&mut *g, Stats::new())
            };
            let f = s.frames.max(1) as f64;
            let r = s.reads.max(1) as f64;
            warning!(
                "profile: cap {:.0}fps gap_max={:.0}ms reqlat avg/max={:.0}/{:.0}ms blit copy avg/max={:.1}/{:.1}ms wait avg/max={:.2}/{:.2}ms | reads {} {:.1}MB hold avg/max={:.1}/{:.1}ms wait_max={:.2}ms | dmg notify={} sub={} create={}",
                s.frames as f64 / dt,
                s.frame_gap_max_ns as f64 / 1e6,
                s.req_lat_sum_ns as f64 / f / 1e6,
                s.req_lat_max_ns as f64 / 1e6,
                s.blit_copy_sum_ns as f64 / f / 1e6,
                s.blit_copy_max_ns as f64 / 1e6,
                s.blit_wait_sum_ns as f64 / f / 1e6,
                s.blit_wait_max_ns as f64 / 1e6,
                s.reads,
                s.read_bytes as f64 / 1e6,
                s.read_hold_sum_ns as f64 / r / 1e6,
                s.read_hold_max_ns as f64 / 1e6,
                s.read_wait_max_ns as f64 / 1e6,
                s.dmg_notify,
                s.dmg_subtract,
                s.dmg_create,
            );
        }
    });
}

/// Records one completed capture: framebuffer lock wait, copy time, and the
/// latency from capture request to `ready`.
pub fn frame(blit_wait_ns: u64, blit_copy_ns: u64, req_latency_ns: u64) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let gap = {
        let mut last = LAST_READY.lock().unwrap();
        let gap = last.map_or(0, |t| now.duration_since(t).as_nanos() as u64);
        *last = Some(now);
        gap
    };
    let mut s = STATS.lock().unwrap();
    s.frames += 1;
    s.blit_wait_sum_ns += blit_wait_ns;
    s.blit_wait_max_ns = s.blit_wait_max_ns.max(blit_wait_ns);
    s.blit_copy_sum_ns += blit_copy_ns;
    s.blit_copy_max_ns = s.blit_copy_max_ns.max(blit_copy_ns);
    s.frame_gap_max_ns = s.frame_gap_max_ns.max(gap);
    s.req_lat_sum_ns += req_latency_ns;
    s.req_lat_max_ns = s.req_lat_max_ns.max(req_latency_ns);
}

/// Records one framebuffer read (`GetImage`/`ShmGetImage`).
pub fn read(bytes: usize, wait_ns: u64, hold_ns: u64) {
    if !enabled() {
        return;
    }
    let mut s = STATS.lock().unwrap();
    s.reads += 1;
    s.read_bytes += bytes as u64;
    s.read_wait_sum_ns += wait_ns;
    s.read_wait_max_ns = s.read_wait_max_ns.max(wait_ns);
    s.read_hold_sum_ns += hold_ns;
    s.read_hold_max_ns = s.read_hold_max_ns.max(hold_ns);
}

pub fn damage_notify(n: u32) {
    if enabled() {
        STATS.lock().unwrap().dmg_notify += n;
    }
}

pub fn damage_subtract() {
    if enabled() {
        STATS.lock().unwrap().dmg_subtract += 1;
    }
}

pub fn damage_create(level: u8) {
    if enabled() {
        STATS.lock().unwrap().dmg_create += 1;
        warning!("profile: DamageCreate level={level}");
    }
}
