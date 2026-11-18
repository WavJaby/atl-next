// SPDX-License-Identifier: MIT
//! The compositor's frame clock, one thread in UI and headless mode alike (SurfaceFlinger's main thread: `commit` latches each vsync whatever the display's toolkit is doing).
//! Ticks sit on a grid of one refresh interval from an origin on CLOCK_MONOTONIC, the clock Choreographer reads vsync times on; after an idle stretch the clock resumes on the next grid point.
//! Where the grid's phase comes from is a capability tier ([`VsyncSource`]), chosen from the presentation samples themselves: a host that declares its presentation times display vsyncs puts the grid on
//! them (`hardware-vsync`); every other host, and headless, free-runs at the display's nominal period from a fixed origin (`free-running`, the baseline).

use std::sync::{Condvar, Mutex};

use atl_platform_ui::{PresentSource, PresentTiming};
use atl_services_types::display_model::period_ns;

/// What wakes the parked clock: a generation counter under its own mutex, bumped by every waker. The clock reads the generation before it checks its conditions (under other locks) and parks only while
/// it is unchanged, so a wake between check and park is never lost, and a waker (the toolkit thread) takes no lock but this one, held for an increment.
#[derive(Debug, Default)]
pub struct Wake {
    generation: Mutex<u64>,
    changed: Condvar,
}

impl Wake {
    /// The generation now: pass it to [`Wake::park`] after checking the conditions.
    pub fn generation(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Something may have changed.
    pub fn kick(&self) {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.changed.notify_all();
    }

    /// Parks while the generation is still `seen`.
    pub fn park(&self, seen: u64) {
        let mut g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        while *g == seen {
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// [`Wake::park`] for at most `limit`.
    pub fn park_for(&self, seen: u64, limit: std::time::Duration) {
        let g = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self
            .changed
            .wait_timeout_while(g, limit, |g| *g == seen)
            .unwrap_or_else(|e| e.into_inner());
    }
}

/// The first grid point `origin + k * interval` (k >= 0) at or after `now`.
pub fn next_tick(origin_ns: i64, interval_ns: i64, now_ns: i64) -> i64 {
    if now_ns <= origin_ns {
        return origin_ns;
    }
    let behind = now_ns - origin_ns;
    origin_ns + (behind + interval_ns - 1) / interval_ns * interval_ns
}

/// SurfaceFlinger's `kSnapToSameVsyncWithin` (VsyncSchedule.cpp `createDispatch`): two vsync times this close are the same vsync.
pub const SNAP_TO_SAME_VSYNC_NS: i64 = 3_000_000;

/// The next tick at or after `now` on the grid, never closer to the last one dispatched than a period allows: AOSP `VSyncDispatchTimerQueueEntry::adjustVsyncIfNeeded`'s `nextVsyncTooClose`
/// (`next - last + snap <= period` -> the first grid point a period after `last`). That test also covers AOSP's `alreadyDispatchedForVsync` (within the snap of `last`): AOSP retries from `last + snap` on its
/// stable model, which lands a period on; our grid can move its phase between ticks, so the retry is the too-close one, which cannot land closer. Without it a grid that moves onto a new presented sample
/// dispatches two vsyncs a few ms apart, a timeline no display has.
pub fn next_dispatch(origin_ns: i64, interval_ns: i64, now_ns: i64, last_ns: Option<i64>) -> i64 {
    let at = next_tick(origin_ns, interval_ns, now_ns);
    match last_ns {
        Some(last) if at - last + SNAP_TO_SAME_VSYNC_NS <= interval_ns => {
            next_tick(origin_ns, interval_ns, last + interval_ns)
        }
        _ => at,
    }
}

/// Where the grid's phase comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// The host's presentation times are display vsyncs on CLOCK_MONOTONIC: the grid sits on the last one (AOSP: the vsync model fed by hardware vsync timestamps).
    HardwareVsync,
    /// No trustworthy phase: the grid free-runs at the nominal period from a fixed origin; presentation samples never move it. A presentation time that is not a vsync (a commit time, a compositor
    /// timer) fed back into the grid makes the app's vsync its own commit pace plus the toolkit's latency (headless labwc, WSLg: measured).
    FreeRunning,
}

impl Tier {
    /// The name the log line uses.
    pub fn name(self) -> &'static str {
        match self {
            Tier::HardwareVsync => "hardware-vsync",
            Tier::FreeRunning => "free-running",
        }
    }
}

/// Consecutive samples of the other kind that switch the tier: one odd sample (a frame the host composited off its usual path) does not flip it. 6 = AOSP's minimum vsync sample count before its
/// model predicts (VsyncSchedule.cpp `createTracker`: `kMinSamplesForPrediction` 6), the number of timestamps AOSP itself needs to trust a hardware vsync source.
pub const SWITCH_SAMPLES: u32 = 6;

/// A tier change, for the log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierChange {
    pub tier: Tier,
    pub reason: &'static str,
}

/// The vsync source tier and the grid it gives. Starts free-running: until the host has shown vsyncs, nothing it reported is a phase. Fed every presentation sample in order ([`VsyncSource::on_sample`]),
/// asked for the grid at every tick ([`VsyncSource::grid`]).
#[derive(Debug)]
pub struct VsyncSource {
    tier: Tier,
    /// Consecutive samples that disagree with the tier.
    against: u32,
    /// The last vsync sample: the hardware tier's phase.
    last_vsync: Option<PresentTiming>,
    /// The host's last advertised non-zero refresh period, ns.
    refresh_ns: Option<i64>,
    /// The free-running grid `(origin, interval)`: set at the first tick of the tier, and again (at the last tick) when the nominal period changes, so a new period continues from where the ticks are.
    free: Option<(i64, i64)>,
}

impl Default for VsyncSource {
    fn default() -> Self {
        VsyncSource {
            tier: Tier::FreeRunning,
            against: 0,
            last_vsync: None,
            refresh_ns: None,
            free: None,
        }
    }
}

impl VsyncSource {
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// One presentation sample, in order: its refresh is the nominal period from now (when the host gives one); the tier switches after [`SWITCH_SAMPLES`] consecutive samples of the other kind.
    pub fn on_sample(&mut self, s: PresentTiming) -> Option<TierChange> {
        if s.refresh_ns > 0 {
            self.refresh_ns = Some(s.refresh_ns);
        }
        let vsync = s.source == PresentSource::Vsync;
        if vsync {
            self.last_vsync = Some(s);
        }
        if vsync == (self.tier == Tier::HardwareVsync) {
            self.against = 0;
            return None;
        }
        self.against += 1;
        if self.against < SWITCH_SAMPLES {
            return None;
        }
        self.against = 0;
        self.free = None;
        let (tier, reason) = match s.source {
            PresentSource::Vsync => (
                Tier::HardwareVsync,
                "presentation times are display vsyncs on CLOCK_MONOTONIC",
            ),
            PresentSource::NotVsync => (
                Tier::FreeRunning,
                "presentation times are not vsyncs: the host does not mark them",
            ),
            PresentSource::OtherClock => (
                Tier::FreeRunning,
                "presentation vsyncs are on another clock than CLOCK_MONOTONIC",
            ),
        };
        self.tier = tier;
        Some(TierChange { tier, reason })
    }

    /// The nominal period: the host's advertised refresh, else the display's rate (`refresh_hz`, the monitor's mode).
    pub fn nominal_ns(&self, refresh_hz: f32) -> i64 {
        self.refresh_ns.unwrap_or_else(|| period_ns(refresh_hz))
    }

    /// The grid `(origin_ns, interval_ns)` of the next tick. Hardware: the last vsync and its period (the nominal one when the host gives none). Free-running: a fixed origin (`last_tick_ns`, else
    /// `start_ns`, when the tier began or the nominal period changed) at the nominal period.
    pub fn grid(
        &mut self,
        start_ns: i64,
        last_tick_ns: Option<i64>,
        refresh_hz: f32,
    ) -> (i64, i64) {
        let nominal = self.nominal_ns(refresh_hz);
        match (self.tier, self.last_vsync) {
            (Tier::HardwareVsync, Some(v)) => (
                v.time_ns,
                if v.refresh_ns > 0 {
                    v.refresh_ns
                } else {
                    nominal
                },
            ),
            _ => match self.free {
                Some((origin, interval)) if interval == nominal => (origin, interval),
                _ => {
                    let origin = last_tick_ns.unwrap_or(start_ns);
                    self.free = Some((origin, nominal));
                    (origin, nominal)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: i64 = 16_666_667; // 1e9 / 60, rounded up
    const START: i64 = 1_000_000_000;
    /// The first frame is shown 5 ms after the start: a phase the start time's grid does not have.
    const OFFSET: i64 = 5_000_000;
    /// A frame's sample reaches the clock 2 ms after the frame is shown.
    const LATENCY: i64 = 2_000_000;
    /// Per-frame jitter of the shown time, cycling 0 / 0.2 / 0.4 ms.
    const JITTER: [i64; 3] = [0, 200_000, 400_000];
    /// The parent's gate: a tick within 1 ms of the phase of the last frame shown.
    const TOLERANCE: i64 = 1_000_000;
    const TICKS: u32 = 100;
    const MONITOR_HZ: f32 = 60.0;

    /// Frame `k` is shown at `OFFSET + k x PERIOD + JITTER[k]` after the start.
    fn shown(k: i64) -> i64 {
        START + OFFSET + k * PERIOD + JITTER[k as usize % JITTER.len()]
    }

    fn sample(time_ns: i64, refresh_ns: i64, source: PresentSource) -> PresentTiming {
        PresentTiming {
            time_ns,
            refresh_ns,
            source,
        }
    }

    /// Over 100 ticks from `SWITCH_SAMPLES` + 1 periods after the first sample arrives, the worst distance of a tick from the phase of the last frame shown before it. The fake feed delivers each sample
    /// `LATENCY` after its frame is shown, marked `source`; the clock computes each grid from the samples delivered at that moment.
    fn worst_phase_error(feed: Option<PresentSource>) -> i64 {
        let window_start = shown(0) + LATENCY + PERIOD * (i64::from(SWITCH_SAMPLES) + 1);
        let mut vs = VsyncSource::default();
        let (mut now, mut next_frame, mut counted, mut worst, mut last_tick) =
            (START, 0i64, 0u32, 0i64, None);
        while counted < TICKS {
            if let Some(source) = feed {
                while shown(next_frame) + LATENCY <= now {
                    vs.on_sample(sample(shown(next_frame), PERIOD, source));
                    next_frame += 1;
                }
            }
            let (origin, interval) = vs.grid(START, last_tick, MONITOR_HZ);
            let at = next_tick(origin, interval, now);
            if at >= window_start {
                let last = (0..)
                    .map(shown)
                    .take_while(|&t| t <= at)
                    .last()
                    .expect("a frame is shown before the window");
                let phase = (at - last).rem_euclid(PERIOD);
                worst = worst.max(phase.min(PERIOD - phase));
                counted += 1;
            }
            last_tick = Some(at);
            now = at + 1;
        }
        worst
    }

    /// Tier hardware-vsync: samples marked as vsyncs put the ticks on the presented phase within 1 ms. Tier free-running: the same samples unmarked (or on another clock) leave the grid on the start
    /// time: each tick is `OFFSET` + that frame's jitter off the frames, worst 5 + 0.4 ms; with no feed at all likewise.
    #[test]
    fn ticks_follow_presentation_phase_only_for_vsyncs() {
        let free_running = OFFSET + JITTER[JITTER.len() - 1];
        assert!(worst_phase_error(Some(PresentSource::Vsync)) <= TOLERANCE);
        assert_eq!(
            worst_phase_error(Some(PresentSource::NotVsync)),
            free_running
        );
        assert_eq!(
            worst_phase_error(Some(PresentSource::OtherClock)),
            free_running
        );
        assert_eq!(worst_phase_error(None), free_running);
    }

    /// The tier switches after `SWITCH_SAMPLES` (6) consecutive samples of the other kind, never on fewer: 5 vsyncs leave it free-running, the 6th switches; then 5 non-vsyncs, one vsync (resets the
    /// run), 5 non-vsyncs stay hardware; the 6th consecutive switches back, naming the reason of the last sample's kind.
    #[test]
    fn tier_switch_is_hysteretic() {
        let mut vs = VsyncSource::default();
        assert_eq!(vs.tier(), Tier::FreeRunning);
        let v = sample(0, PERIOD, PresentSource::Vsync);
        let n = sample(0, PERIOD, PresentSource::NotVsync);
        let o = sample(0, PERIOD, PresentSource::OtherClock);
        for _ in 0..5 {
            assert_eq!(vs.on_sample(v), None);
        }
        assert_eq!(vs.on_sample(v).map(|c| c.tier), Some(Tier::HardwareVsync));
        for _ in 0..5 {
            assert_eq!(vs.on_sample(n), None);
        }
        assert_eq!(vs.on_sample(v), None);
        for _ in 0..5 {
            assert_eq!(vs.on_sample(o), None);
        }
        assert_eq!(vs.tier(), Tier::HardwareVsync);
        let change = vs
            .on_sample(o)
            .expect("the 6th consecutive non-vsync switches");
        assert_eq!(change.tier, Tier::FreeRunning);
        assert!(change.reason.contains("another clock"));
        // alternating kinds never switch
        for i in 0..100 {
            assert_eq!(vs.on_sample(if i % 2 == 0 { v } else { n }), None);
        }
    }

    /// Free-running period: the host's advertised refresh when non-zero, else the monitor's (60 Hz -> 16 666 667 ns, rounded); a refresh of 0 (headless wlroots: "unknown") keeps the last known one. A
    /// new period re-anchors the origin at the last tick: WSLg's advertised 16 666 666 ns after a tick at 5000 gives (5000, 16 666 666); a 20 ms refresh after a tick at 50 ms gives (50 ms, 20 ms).
    #[test]
    fn free_running_period_and_anchor() {
        const WSLG_REFRESH: i64 = 16_666_666;
        let mut vs = VsyncSource::default();
        assert_eq!(vs.grid(1000, None, MONITOR_HZ), (1000, PERIOD));
        vs.on_sample(sample(7, 0, PresentSource::NotVsync));
        assert_eq!(vs.grid(1000, Some(5_000), MONITOR_HZ), (1000, PERIOD));
        vs.on_sample(sample(9, WSLG_REFRESH, PresentSource::NotVsync));
        assert_eq!(
            vs.grid(1000, Some(5_000), MONITOR_HZ),
            (5_000, WSLG_REFRESH)
        );
        // the sample's time never moves it
        vs.on_sample(sample(12_345, WSLG_REFRESH, PresentSource::NotVsync));
        assert_eq!(
            vs.grid(1000, Some(9_000), MONITOR_HZ),
            (5_000, WSLG_REFRESH)
        );
        vs.on_sample(sample(13, 20_000_000, PresentSource::NotVsync));
        vs.on_sample(sample(14, 0, PresentSource::NotVsync));
        assert_eq!(
            vs.grid(1000, Some(50_000_000), MONITOR_HZ),
            (50_000_000, 20_000_000)
        );
    }

    /// Hardware tier with a vsync sample of refresh 0 (a variable-rate display may report none): the grid is the sample's time at the nominal period.
    #[test]
    fn hardware_vsync_without_refresh_uses_nominal() {
        let mut vs = VsyncSource::default();
        for k in 0..i64::from(SWITCH_SAMPLES) {
            vs.on_sample(sample(k * PERIOD, PERIOD, PresentSource::Vsync));
        }
        vs.on_sample(sample(77, 0, PresentSource::Vsync));
        assert_eq!(vs.grid(0, None, MONITOR_HZ), (77, PERIOD));
    }

    /// On the grid 100, 110, 120 ...: a time on a point stays, between two points goes up, before the origin goes to the origin.
    #[test]
    fn next_grid_point() {
        assert_eq!(next_tick(100, 10, 100), 100);
        assert_eq!(next_tick(100, 10, 101), 110);
        assert_eq!(next_tick(100, 10, 110), 110);
        assert_eq!(next_tick(100, 10, 95), 100);
        assert_eq!(next_tick(100, 10, 1_000_001), 1_000_010);
    }

    /// On a 16.67 ms grid: last tick at 0, the grid moved its phase 5 ms back (origin 11.67 ms). The plain next point after now = 1 ms is
    /// 11.67 ms, 11.67 ms after the last (< period - 3 ms = 13.67 ms): too close, so the first point at or after 16.67 ms is taken, 28.33 ms. A point 14 ms after (> 13.67 ms) stays; no last tick, no change.
    #[test]
    fn dispatch_not_too_close() {
        let p = 16_666_667;
        let origin = p - 5_000_000;
        assert_eq!(next_dispatch(origin, p, 1_000_000, Some(0)), origin + p);
        assert_eq!(next_dispatch(origin, p, 1_000_000, None), origin);
        let origin14 = 14_000_000;
        assert_eq!(next_dispatch(origin14, p, 1_000_000, Some(0)), origin14);
        // same vsync again (now == last on an unchanged grid): one period on
        assert_eq!(next_dispatch(0, p, 0, Some(0)), p);
    }

    /// A kick between reading the generation and parking is not lost: park returns at once.
    #[test]
    fn kick_before_park_is_kept() {
        let w = Wake::default();
        let seen = w.generation();
        w.kick();
        w.park(seen);
        assert_eq!(w.generation(), seen + 1);
    }
}
