// SPDX-License-Identifier: MIT
//! Stage `ui-block` of `xtask test window` (composer threading S2, composer backpressure, display-thread S5d): a SurfaceView whose thread keeps posting frames (xtask-harness/testdata/ui-block, Main.java)
//! while the app's UI thread sleeps 2 s (block). The fixture's steady phase before it is stage slow-display's subject (slow_display.rs); here it is only the window's own idle rate.
//! The window runs on the platform's display thread, which waits on no app thread (display-thread plan D2), so the block must not stop it. Judged on local runs only (the proxy and the app share
//! CLOCK_MONOTONIC; a remote run carries no commit times), from the window surface's `wl_surface.commit`s the proxy timed against the phase edges on the fixture's result line:
//! - Commits: the window keeps committing through the block at its own idle rate: strictly inside [block start + EDGE, block end - EDGE] (EDGE = one display period: a commit in flight at the start edge,
//!   the first after the end) at least max(1, floor(span / steady_p95) - [`SLACK`]), steady_p95 = the 95th percentile commit interval of the steady phase. Probe responds: >= 2 steady commits.
//!   Red before S5d: the toolkit ran inside the UI thread's Looper poll, so a sleeping UI thread committed nothing (S5c measured 0 inside).
//!   Timed against steady_p95 over the block (starvation.rs): a fail on a GPU-starved host is undecided (1.5-2.3 s renders against a 45-77 ms p95 measured at load 7-10 leave the steady rate
//!   saying nothing about the block, whatever the code does). A pass stands: starvation only removes commits.
//! - Posts: SurfaceFlinger's rule, one latch per present (display-thread plan D4): the producer tracks the display, not the UI thread, so the frames posted during the block are at least the window's
//!   commits in it less [`SLACK`].
//! - Timeouts (every run): no dequeue timed out (`-110`).

use super::{ACK_WAIT, Checks, Liveness};
use crate::composer::word;
use crate::starvation::{Fakes, Render, Timed, span};
use crate::wait::{Miss, Wait, until};
use crate::wayland_proxy::{Event as ProxyEvent, Proxy};
use std::path::Path;
use std::time::Duration;

/// The fixture's dir.
pub const FIXTURE: &str = "xtask-harness/testdata/ui-block";
/// The fixture's result line (Main.java draw).
const RESULT: &str = "AtlWindow: ub result ";
/// The fixture's block markers (Main.java block), stamped by the log: where the block sits among the window's paint lines.
const BLOCK_START: &str = "AtlWindow: ub block start";
const BLOCK_END: &str = "AtlWindow: ub block end";
/// Frames lost at the block's two edges (the post in flight when it starts, the first latch or commit after it): the design's `- 2`.
const SLACK: u64 = 2;
/// Warm-up (60 frames, 1 s at 60 Hz) + steady phase 4 s + block 2 s + tail 1 s, with room for a slow start.
const RESULT_WAIT: Duration = Duration::from_secs(25);
/// A dequeue that timed out (HWUI / libgui log `dequeueBuffer failed ... -110`, TIMED_OUT).
const TIMED_OUT: &str = "-110";
/// The steady phase's interval taken as the window's idle rate: its 95th percentile (nearest rank), so a few slow paints do not set it and the block is held to what the window nearly always does.
const STEADY_PERCENTILE: f64 = 0.95;

/// The frame period of the run's display (the gate's output locally, the target's display remotely), ms.
pub(super) fn period_ms(refresh_hz: f32) -> f64 {
    1_000.0 / f64::from(refresh_hz)
}

pub(super) fn result_line(app: &str) -> Result<&str, String> {
    app.lines()
        .find(|l| l.contains(RESULT))
        .ok_or_else(|| "no `ub result` line: the producer never got past the block".to_owned())
}

pub(super) fn number(line: &str, k: &str) -> Result<u64, String> {
    word(line, k)
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or_else(|| format!("no {k}= in {line}"))
}

/// No dequeue timed out.
fn timeouts_check(app: &str) -> Result<String, String> {
    let n = app
        .lines()
        .filter(|l| l.contains("dequeueBuffer") && l.contains(TIMED_OUT))
        .count();
    if n == 0 {
        Ok("no dequeueBuffer -110".to_owned())
    } else {
        Err(format!("{n} dequeueBuffer -110 lines"))
    }
}

/// The window's commits ([`crate::wayland_proxy::Commits`], CLOCK_MONOTONIC ns) placed on the fixture's phases (uptimeMillis, the same clock in ms).
struct Phases {
    /// Commit times in ms, ascending.
    commits_ms: Vec<f64>,
    block: (f64, f64),
    steady: (f64, f64),
    /// One display period.
    edge: f64,
}

impl Phases {
    fn of(app: &str, refresh_hz: f32, commits_ns: &[u64]) -> Result<Self, String> {
        const NS_PER_MS: f64 = 1_000_000.0;
        let line = result_line(app)?;
        let ms = |k| number(line, k).map(|v| v as f64);
        let mut commits_ms: Vec<f64> = commits_ns.iter().map(|t| *t as f64 / NS_PER_MS).collect();
        commits_ms.sort_by(f64::total_cmp);
        Ok(Phases {
            commits_ms,
            block: (ms("block_start_ms")?, ms("block_end_ms")?),
            steady: (ms("steady_start_ms")?, ms("steady_end_ms")?),
            edge: period_ms(refresh_hz),
        })
    }

    /// The judged span inside the block: one period in from each edge.
    fn inside(&self) -> (f64, f64) {
        (self.block.0 + self.edge, self.block.1 - self.edge)
    }

    fn count(&self, (lo, hi): (f64, f64), open: bool) -> u64 {
        let within = |t: f64| {
            if open {
                t > lo && t < hi
            } else {
                t >= lo && t <= hi
            }
        };
        self.commits_ms.iter().filter(|t| within(**t)).count() as u64
    }

    /// The steady phase's commit interval at [`STEADY_PERCENTILE`] (nearest rank), ms; None below two steady commits.
    fn steady_p95(&self) -> Option<f64> {
        let (lo, hi) = self.steady;
        let steady: Vec<f64> = self
            .commits_ms
            .iter()
            .copied()
            .filter(|t| *t >= lo && *t <= hi)
            .collect();
        let mut intervals: Vec<f64> = steady.windows(2).map(|w| w[1] - w[0]).collect();
        if intervals.is_empty() {
            return None;
        }
        intervals.sort_by(f64::total_cmp);
        let rank = (STEADY_PERCENTILE * intervals.len() as f64).ceil() as usize;
        Some(intervals[rank.clamp(1, intervals.len()) - 1])
    }
}

/// The steady p95, or the probe-silent failure.
fn steady_p95(phases: &Phases) -> Result<f64, String> {
    phases.steady_p95().ok_or_else(|| {
        format!(
            "probe silent: fewer than 2 window commits in the steady phase ({} in all)",
            phases.commits_ms.len()
        )
    })
}

/// The window kept committing through the block at no less than its own steady rate (module docs).
fn commits_check(phases: &Phases, p95: f64) -> Result<String, String> {
    let (lo, hi) = phases.inside();
    let inside = phases.count((lo, hi), true);
    let want = (((hi - lo) / p95).floor() as u64)
        .saturating_sub(SLACK)
        .max(1);
    let summary = format!(
        "{inside} inside the block ({lo:.1}..{hi:.1} ms, edge {:.2} ms; want >= {want}: steady p95 {p95:.1} ms), {} window commits in all",
        phases.edge,
        phases.commits_ms.len()
    );
    if inside >= want {
        Ok(summary)
    } else {
        Err(summary)
    }
}

/// The producer posted at least once per window commit during the block, less [`SLACK`] (module docs).
fn posts_check(app: &str, phases: &Phases) -> Result<String, String> {
    let line = result_line(app)?;
    let (during, gap) = (number(line, "posts_during")?, number(line, "max_gap_ms")?);
    let commits = phases.count(phases.block, false);
    let want = commits.saturating_sub(SLACK);
    let summary = format!(
        "{during} frames posted during the block (want >= {want}: {commits} window commits in it), longest gap {gap} ms"
    );
    if during >= want {
        Ok(summary)
    } else {
        Err(summary)
    }
}

/// The sub-gates; `refresh_hz`: the run's display (the block's edges); `commits_ns`: the proxy's window commit times, None on a remote run (another host's clock): commits and posts are then not judged.
/// commits is timed (starvation.rs): it assumes every interval inside the block within the steady p95; a slow host removes commits, never adds them, so only a fail is unjudged (Fakes::Fail). A
/// window stopped by the block paints nothing in it, so the lines covering the block hold only the renders around it, fast on a host that is not starved. Posts holds the producer to the window's
/// commits, whatever their rate: functional.
pub fn checks(app: &str, refresh_hz: f32, commits_ns: Option<&[u64]>) -> Checks {
    let mut subs = vec![("timeouts", timeouts_check(app))];
    let mut timed = Vec::new();
    if let Some(commits_ns) = commits_ns {
        match Phases::of(app, refresh_hz, commits_ns) {
            Ok(phases) => {
                match steady_p95(&phases) {
                    Ok(p95) => timed.push(Timed {
                        sub: ("commits", commits_check(&phases, p95)),
                        gpu: Some(Render {
                            limit_us: p95 * 1000.0,
                            limit: format!("steady p95 {p95:.1} ms"),
                            span: span(app, BLOCK_START, BLOCK_END),
                        }),
                        cpu: None,
                        fakes: Fakes::Fail,
                    }),
                    Err(e) => subs.push(("commits", Err(e))),
                }
                subs.push(("posts", posts_check(app, &phases)));
            }
            Err(e) => {
                subs.push(("commits", Err(e.clone())));
                subs.push(("posts", Err(e)));
            }
        }
    }
    Checks { subs, timed }
}

/// The probe: the window's configure is acked, then the fixture's result line (the phases are the app's own doing).
pub fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, Miss> {
    until(&Wait::event(
        "configure-ack",
        "the window never acked a configure",
        ACK_WAIT,
        &proxy.events,
        &|e: &ProxyEvent| matches!(e, ProxyEvent::Acked { .. }),
    ))?;
    until(&Wait::log(
        "ub-result",
        "no `ub result` line in time",
        RESULT_WAIT,
        app_log,
        &|t: &str| t.contains(RESULT),
    ))?;
    Ok(Liveness::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::starvation::{Run, apply};

    /// Steady phase 6000..10000 ms, block 10000..12000 ms on the app's uptimeMillis.
    fn edges(during: u64) -> String {
        format!(
            "9.0 I AtlWindow: ub result posts_during={during} max_gap_ms=20 block_ms=2000 block_start_ms=10000 block_end_ms=12000 steady_start_ms=6000 steady_end_ms=10000\n"
        )
    }
    const MS: u64 = 1_000_000;

    /// Commits every `step_ms` from `from` to `to` ms (inclusive start, exclusive end).
    fn every(from: u64, to: u64, step_ms: u64) -> Vec<u64> {
        (from..to)
            .step_by(step_ms as usize)
            .map(|t| t * MS)
            .collect()
    }

    fn phases(during: u64, commits: &[u64]) -> Phases {
        Phases::of(&edges(during), 60.0, commits).expect("phases")
    }

    /// commits as `checks` judges it on a host that is not starved.
    fn judge(phases: &Phases) -> Result<String, String> {
        steady_p95(phases).and_then(|p95| commits_check(phases, p95))
    }

    /// Steady commits every 20 ms (p95 = 20 ms). 60 Hz: EDGE = 16.666... ms, the judged span 10016.67..11983.33 ms = 1966.67 ms: floor(98.33) = 98 - SLACK 2 = 96 wanted.
    /// Commits every 20 ms in the block from 10020: 10020..11980 = 99 inside, pass; every 21 ms: 10020..11973 = 94 inside, fail. None inside (the UI thread's block stopping the window): fail.
    #[test]
    fn commits_through_the_block_at_the_steady_rate() {
        let steady = every(6_000, 10_000, 20);
        let at = |step| [steady.clone(), every(10_020, 11_984, step)].concat();
        assert!(judge(&phases(0, &at(20))).is_ok());
        assert!(judge(&phases(0, &at(21))).is_err());
        assert!(judge(&phases(0, &steady)).is_err());
    }

    /// Steady p95 20 ms (commits every 20 ms), none inside the block: red when judged. commits is timed against the p95 over the block (10.000 / 12.000 s markers), Fakes::Fail; starvation.rs
    /// `apply` decides. The only paint line, at 13.000 s with ms=3000, covers the block and is the run's median: a 20.001 ms render there leaves the failing commits unjudged; 20.000 ms exactly, or no
    /// paint line, judges it (red); posts stays judged throughout.
    #[test]
    fn a_starved_block_leaves_commits_undecided() {
        let steady = every(6_000, 10_000, 20);
        let app = |render_us: Option<u64>| {
            let paint = render_us.map_or(String::new(), |r| {
                format!("13.000  INFO atl-platform-ui: paint ms=3000 render_us=1/{r}\n")
            });
            format!(
                "10.000 I AtlWindow: ub block start\n12.000 I AtlWindow: ub block end\n{paint}{}",
                edges(0)
            )
        };
        let judge = |log: &str, commits: &[u64]| {
            let c = checks(log, 60.0, Some(commits));
            let names: Vec<&str> = c.subs.iter().map(|(n, _)| *n).collect();
            assert_eq!(names, ["timeouts", "posts"]);
            apply(
                &Run {
                    stage: "ui-block",
                    app: log,
                    from: 0,
                    samples: None,
                    hw: None,
                    subject: None,
                },
                c.timed,
            )
        };
        let (judged, undecided) = judge(&app(Some(20_001)), &steady);
        assert!(judged.is_empty());
        assert!(matches!(undecided.as_slice(), [("commits", _, Err(_))]));
        assert_eq!(
            undecided[0].1.to_string(),
            "host GPU starved (window render median 20 ms, max 20 ms in the phase > steady p95 20.0 ms)"
        );
        for log in [app(Some(20_000)), app(None)] {
            let (judged, undecided) = judge(&log, &steady);
            assert!(undecided.is_empty());
            assert!(matches!(judged.as_slice(), [("commits", Err(_))]));
        }
        // commits every 20 ms through the block pass (99 inside, want 96): the pass stands on the starved host
        let through = [steady.clone(), every(10_020, 11_984, 20)].concat();
        let (judged, undecided) = judge(&app(Some(20_001)), &through);
        assert!(undecided.is_empty());
        assert!(matches!(judged.as_slice(), [("commits", Ok(_))]));
    }

    /// Two steady commits 1000 ms apart: p95 1000 ms, floor(1966.67 / 1000) = 1 - 2 saturates to 0, so the floor of 1 holds: one inside passes, none fails. One steady commit: probe silent.
    #[test]
    fn commits_want_at_least_one_and_a_live_probe() {
        let steady = vec![7_000 * MS, 8_000 * MS];
        assert!(judge(&phases(0, &[steady.clone(), vec![11_000 * MS]].concat())).is_ok());
        assert!(judge(&phases(0, &steady)).is_err());
        assert!(judge(&phases(0, &[7_000 * MS, 11_000 * MS])).is_err());
    }

    /// p95 by nearest rank: 20 intervals, 19 of 10 ms and one of 100 ms: rank ceil(19.0) = 19 -> 10 ms (the one slow paint does not set the rate); with 2 slow of 20, rank 19 -> 100 ms.
    #[test]
    fn steady_rate_is_the_95th_percentile_interval() {
        let mut t: Vec<u64> = (0..20).map(|i| (6_000 + 10 * i) * MS).collect();
        t.push(6_290 * MS);
        assert_eq!(phases(0, &t).steady_p95(), Some(10.0));
        let mut t: Vec<u64> = (0..19).map(|i| (6_000 + 10 * i) * MS).collect();
        t.extend([6_280 * MS, 6_380 * MS]);
        assert_eq!(phases(0, &t).steady_p95(), Some(100.0));
    }

    /// Block commits counted over the whole block 10000..12000 ms inclusive: 100 there (every 20 ms) want 98 posts; 98 pass, 97 fail.
    #[test]
    fn posts_track_the_window_commits_less_two() {
        let commits = [every(6_000, 10_000, 20), every(10_000, 12_000, 20)].concat();
        assert!(posts_check(&edges(98), &phases(98, &commits)).is_ok());
        assert!(posts_check(&edges(97), &phases(97, &commits)).is_err());
    }

    /// Commits and posts exist only with commit times (a local run); a result line without phase edges fails both.
    #[test]
    fn commits_and_posts_only_judged_locally() {
        let app = edges(98);
        let names = |c: Checks| c.subs.into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(checks(&app, 60.0, None)), ["timeouts"]);
        assert_eq!(
            names(checks(&app, 60.0, Some(&[7_000 * MS]))),
            ["timeouts", "commits", "posts"]
        );
        let bare = "9.0 I AtlWindow: ub result posts_during=98 max_gap_ms=20\n";
        assert!(
            checks(bare, 60.0, Some(&[7_000 * MS]))
                .subs
                .iter()
                .skip(1)
                .all(|(_, r)| r.is_err())
        );
    }
}
