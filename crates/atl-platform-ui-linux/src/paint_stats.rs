// SPDX-License-Identifier: MIT
//! Per-window paint cost: one line `atl-platform-ui: paint ...` per second of frame-clock activity (never per frame), deltas since the last line; no line while the clock is idle.
//! Stages, each `mean/max` us over the frames that ran it: `build` (present: the Scene -> render nodes + textures, `scene::build`), `snapshot` (the view's snapshot), `render` (the rest of the paint phase:
//! GSK render, which also downloads/uploads a texture GSK cannot import, and the swap), `after` (paint end -> our after-paint: GDK's after-paint and the buffer releases).
//! `paints` = paint phases that snapshotted the scene; `stale` = those with no present since the last one (nothing new drawn); `other` = paint phases that did not snapshot the scene (chrome, sideband).
//! `textures_new` / `textures_reused` count the buffer quads of presented scenes: a new texture is an import (or, where GSK cannot import, a CPU download + upload at render).
//! `interval_us` = `p50/p95/max` of paint end -> next paint end over the scene paints ending in this line (the first paint after an idle clock counts from the last paint before it); `request_us` = `mean/max`
//! of a frame request -> the frame-clock tick that serves it (GDK's wait for the compositor's frame callback and its frame-time grid; our own scheduling adds nothing between them).

/// One stage's timings since the last line.
#[derive(Default, Clone, Copy)]
struct Stage {
    n: u64,
    sum_ns: i64,
    max_ns: i64,
}

impl Stage {
    fn add(&mut self, ns: i64) {
        self.n += 1;
        self.sum_ns += ns;
        self.max_ns = self.max_ns.max(ns);
    }

    /// `mean/max` in us; `-` when the stage did not run.
    fn show(&self) -> String {
        match self.n {
            0 => "-".to_owned(),
            n => format!("{}/{}", self.sum_ns / n as i64 / 1000, self.max_ns / 1000),
        }
    }
}

/// The counters of one window, reset at each line.
#[derive(Default)]
pub struct PaintStats {
    since_ns: Option<i64>,
    cycles: u64,
    paints: u64,
    stale: u64,
    presents: u64,
    textures_new: u64,
    textures_reused: u64,
    build: Stage,
    snapshot: Stage,
    render: Stage,
    other: Stage,
    after: Stage,
    request: Stage,
    /// Paint end -> next paint end (ns) of the scene paints in this line.
    intervals: Vec<i64>,
    /// The last scene paint's end: kept across lines, so a line's first interval is counted.
    last_paint_ns: Option<i64>,
}

/// `p50/p95/max` in us of `v` (nearest rank, lower index); `-` when empty.
fn percentiles(v: &mut [i64]) -> String {
    if v.is_empty() {
        return "-".to_owned();
    }
    v.sort_unstable();
    let n = v.len();
    format!(
        "{}/{}/{}",
        v[n / 2] / 1000,
        v[n * 95 / 100] / 1000,
        v[n - 1] / 1000
    )
}

/// A line covers at least this much frame-clock time: the rate limit.
const LINE_PERIOD_NS: i64 = 1_000_000_000;

impl PaintStats {
    /// A present built its scene in `build_ns` with `new` new and `reused` reused textures.
    pub fn presented(&mut self, build_ns: i64, new: u64, reused: u64) {
        self.presents += 1;
        self.textures_new += new;
        self.textures_reused += reused;
        self.build.add(build_ns);
    }

    /// A frame request was served by a tick `wait_ns` after it was made.
    pub fn requested(&mut self, wait_ns: i64) {
        self.request.add(wait_ns);
    }

    /// One paint phase of `paint_ns` (before-paint end -> paint end at `end_ns`); `snapshot_ns` when it snapshotted the scene, `stale` when no present came since the previous one.
    pub fn painted(&mut self, end_ns: i64, paint_ns: i64, snapshot_ns: Option<i64>, stale: bool) {
        match snapshot_ns {
            Some(s) => {
                if let Some(last) = self.last_paint_ns.replace(end_ns) {
                    self.intervals.push(end_ns - last);
                }
                self.paints += 1;
                self.stale += u64::from(stale);
                self.snapshot.add(s);
                self.render.add(paint_ns - s);
            }
            None => self.other.add(paint_ns),
        }
    }

    /// The after-paint of a frame-clock cycle at `now_ns`, `after_ns` past its paint end (None: no paint phase this cycle); logs and resets once a line period has passed.
    pub fn cycle_done(&mut self, now_ns: i64, after_ns: Option<i64>) {
        self.cycles += 1;
        if let Some(a) = after_ns {
            self.after.add(a);
        }
        let since = *self.since_ns.get_or_insert(now_ns);
        if now_ns - since < LINE_PERIOD_NS {
            return;
        }
        tracing::info!(
            "atl-platform-ui: paint ms={} cycles={} paints={} stale={} presents={} textures_new={} textures_reused={} build_us={} snapshot_us={} render_us={} other_us={} after_us={} interval_us={} request_us={}",
            (now_ns - since) / 1_000_000,
            self.cycles,
            self.paints,
            self.stale,
            self.presents,
            self.textures_new,
            self.textures_reused,
            self.build.show(),
            self.snapshot.show(),
            self.render.show(),
            self.other.show(),
            self.after.show(),
            percentiles(&mut self.intervals),
            self.request.show(),
        );
        *self = Self {
            since_ns: Some(now_ns),
            last_paint_ns: self.last_paint_ns,
            ..Self::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-derived: 3 + 5 ms -> mean 4000 us, max 5000 us; render = paint - snapshot = 9 - 1 = 8 ms.
    #[test]
    fn stages_split_and_reset() {
        let mut s = PaintStats::default();
        s.presented(3_000_000, 1, 2);
        s.presented(5_000_000, 1, 0);
        s.painted(10_000_000, 9_000_000, Some(1_000_000), false);
        s.painted(11_000_000, 2_000_000, None, false);
        assert_eq!(s.build.show(), "4000/5000");
        assert_eq!(s.render.show(), "8000/8000");
        assert_eq!(s.other.show(), "2000/2000");
        assert_eq!((s.textures_new, s.textures_reused), (2, 2));
        s.cycle_done(0, None);
        s.cycle_done(LINE_PERIOD_NS, Some(1000));
        assert_eq!(
            (s.cycles, s.presents, s.build.show()),
            (0, 0, "-".to_owned())
        );
        // the last scene paint survives the reset: the next line's first interval counts from it
        assert_eq!(s.last_paint_ns, Some(10_000_000));
    }

    /// Paint ends at 0, 16, 33, 50, 100 ms -> intervals 16, 17, 17, 50 ms; other paints add none. Sorted [16, 17, 17, 50] ms: p50 = index 2 = 17 ms, p95 = index 3 (4 * 95 / 100) = 50 ms, max 50 ms.
    #[test]
    fn paint_intervals() {
        let mut s = PaintStats::default();
        for end_ms in [0, 16, 33, 50, 100] {
            s.painted(end_ms * 1_000_000, 1, Some(0), false);
            s.painted(end_ms * 1_000_000 + 1, 1, None, false);
        }
        assert_eq!(percentiles(&mut s.intervals), "17000/50000/50000");
        assert_eq!(percentiles(&mut []), "-");
    }
}
