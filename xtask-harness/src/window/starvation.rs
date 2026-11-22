// SPDX-License-Identifier: MIT
//! The window gates' one host-GPU-starvation rule. A sub-check whose bound assumes the window renders within some interval (a rate or count over a phase: slow-display held / replaced, ui-block
//! commits) declares that interval ([`Timed`]); every other sub-check is functional (a value, an order, a crash, an answer the probe waits for) and declares nothing, so it is always judged.
//! [`apply`] (window.rs `judge`, every stage) shows a timed sub-check unjudged (`undecided`, neither pass nor fail) only when both hold, each measured from this run's own paint lines (the platform's
//! per-second `render_us` = GSK render + swap, measured by the window itself, so no defect in the code under test can lengthen it without the GPU being slow):
//! - the host was starved: the median of the paint lines' longest renders after the marker exceeds the interval -- the window's typical render, not one spike (measured 2026-11-22: median p99 53 ms
//!   over 521 runs from before the GPU was taken, p10 210 ms over 54 runs with it held at 94-100 %);
//! - starvation reached the bound: a render covering the span the sub-check judges exceeds the interval.
//!
//! Only the outcomes starvation can produce are unjudged ([`Fakes`]): a pass stays a pass where starvation can only remove what the bound counts. Each turn prints one `window: starvation: ` line
//! (stage, sub-check, its outcome, both measurements) so land logs count how often the rule fires; every run prints the median and max (`window: host gpu: `), fired or not.

use super::{Sub, line_stamp};
use crate::composer::word;

/// The platform's per-second paint line (atl-platform-ui-linux paint_stats.rs): `ms=` the time it covers, `render_us=mean/max`.
const PAINT: &str = "atl-platform-ui: paint ";

/// The stamps (ms) of the first lines holding `from` and `to`; None without both.
pub(super) fn span(app: &str, from: &str, to: &str) -> Option<(u64, u64)> {
    let stamp = |marker: &str| {
        app.lines()
            .find(|l| l.contains(marker))
            .and_then(line_stamp)
    };
    Some((stamp(from)?, stamp(to)?))
}

/// Each paint line's stamp (ms), the time it covers (ms) and its longest render (us).
fn paints(app: &str) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
    app.lines().filter(|l| l.contains(PAINT)).filter_map(|l| {
        let at = line_stamp(l)?;
        let ms = word(l, "ms")?.parse().ok()?;
        let max = word(l, "render_us")?.split_once('/')?.1.parse().ok()?;
        Some((at, ms, max))
    })
}

/// The longest window render (us) among the paint lines covering any part of `[start, end]` (log stamps, ms). A line covers `[stamp - ms, stamp]`: its counts are deltas since the previous line,
/// which on a starved host comes seconds later (3 s lines measured at load 10), so a span shorter than that holds no stamped line at all and only the covering line measures it. None without one.
pub(super) fn render_max_us(app: &str, (start, end): (u64, u64)) -> Option<u64> {
    paints(app)
        .filter(|&(at, ms, _)| at >= start && at.saturating_sub(ms) <= end)
        .map(|(_, _, max)| max)
        .max()
}

/// The host's render over the run: median and max (us) of the paint lines' longest renders stamped at or after `from` (ms), and their count. The first paints (startup: shader compiles, first
/// texture uploads, 1.3 s median on a free GPU) come before the marker, so they do not count. None without a paint line.
pub(super) fn host_render_us(app: &str, from: u64) -> Option<(u64, u64, usize)> {
    let mut v: Vec<u64> = paints(app)
        .filter(|&(at, _, _)| at >= from)
        .map(|(_, _, max)| max)
        .collect();
    v.sort_unstable();
    // lower median: an even count takes the smaller middle value, so a starved verdict needs at least half the lines over
    let median = *v.get(v.len().checked_sub(1)? / 2)?;
    Some((median, *v.last()?, v.len()))
}

/// Which outcomes of a timed sub-check host starvation can produce, so which it leaves unjudged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fakes {
    /// Only a fail: starvation only removes what the bound counts (ui-block commits), so a pass stands.
    Fail,
    /// Both: starvation moves the count either way (slow-display held: expired holds post more, a starved producer posts less).
    Both,
}

/// A sub-check whose bound assumes the window renders within `limit_us`, over `span` (log stamps, ms; None: no span found, so judged).
pub struct Timed {
    pub sub: Sub,
    pub limit_us: f64,
    /// The interval's name in the reason, e.g. `hold bound 100 ms`.
    pub limit: String,
    pub span: Option<(u64, u64)>,
    pub fakes: Fakes,
}

/// A timed sub-check the rule left unjudged: name, reason, the result as measured (shown, judging nothing).
pub type Undecided = (&'static str, String, Result<String, String>);

/// The reason `t` is unjudged on this run, None when it is judged ([`self`] module docs). `host`: the run's median render (us), None without paint lines.
fn starved(app: &str, host: Option<u64>, t: &Timed) -> Option<String> {
    let faked = match t.fakes {
        Fakes::Fail => t.sub.1.is_err(),
        Fakes::Both => true,
    };
    let host = host.filter(|&m| faked && m as f64 > t.limit_us)?;
    let reached = render_max_us(app, t.span?).filter(|&r| r as f64 > t.limit_us)?;
    Some(format!(
        "host GPU starved (window render median {} ms, max {} ms in the phase > {})",
        host / 1000,
        reached / 1000,
        t.limit
    ))
}

/// Splits a stage's timed sub-checks into judged ones and the ones host starvation left unjudged; prints one `window: starvation: ` line per turn. `from`: the marker's stamp (ms).
pub fn apply(app: &str, stage: &str, from: u64, timed: Vec<Timed>) -> (Vec<Sub>, Vec<Undecided>) {
    let host = host_render_us(app, from).map(|(median, _, _)| median);
    let (mut judged, mut undecided) = (Vec::new(), Vec::new());
    for t in timed {
        match starved(app, host, &t) {
            Some(why) => {
                let was = if t.sub.1.is_ok() { "pass" } else { "FAIL" };
                println!(
                    "window: starvation: {stage} {} {was} -> undecided: {why}",
                    t.sub.0
                );
                undecided.push((t.sub.0, why, t.sub.1));
            }
            None => judged.push(t.sub),
        }
    }
    (judged, undecided)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A paint line at `at` s covering `ms` with longest render `max_us`.
    fn paint(at: &str, ms: u64, max_us: u64) -> String {
        format!("{at}  INFO atl-platform-ui: paint ms={ms} render_us=1/{max_us}\n")
    }

    fn timed(ok: bool, fakes: Fakes, span: Option<(u64, u64)>) -> Timed {
        let r = if ok {
            Ok("x".to_owned())
        } else {
            Err("x".to_owned())
        };
        Timed {
            sub: ("s", r),
            limit_us: 100_000.0,
            limit: "bound 100 ms".to_owned(),
            span,
            fakes,
        }
    }

    /// A paint line covers `[stamp - ms, stamp]`: over the span 10.000..12.000 s, a line at 13.000 s with ms=3000 (10..13 s) and one at 10.500 s with ms=1000 count, their max 900 us; lines at 9.000 s
    /// (ms=1000: 8..9 s) and 15.000 s (ms=2000: 13..15 s) do not. No covering line: None.
    #[test]
    fn a_paint_line_counts_where_its_span_covers() {
        let app = [
            paint("9.000", 1000, 5000),
            paint("10.500", 1000, 700),
            paint("13.000", 3000, 900),
            paint("15.000", 2000, 8000),
        ]
        .concat();
        assert_eq!(render_max_us(&app, (10_000, 12_000)), Some(900));
        assert_eq!(
            render_max_us(&paint("9.000", 1000, 5000), (10_000, 12_000)),
            None
        );
    }

    /// Median and max after the marker at 2 s: the 1.000 s startup line (3 s render) does not count; 4 lines 50/150/150/400 ms -> lower median 150 ms, max 400 ms.
    #[test]
    fn host_render_skips_startup() {
        let app = [
            paint("1.000", 1000, 3_000_000),
            paint("3.000", 1000, 50_000),
            paint("4.000", 1000, 150_000),
            paint("5.000", 1000, 150_000),
            paint("6.000", 1000, 400_000),
        ]
        .concat();
        assert_eq!(host_render_us(&app, 2_000), Some((150_000, 400_000, 4)));
        assert_eq!(host_render_us("", 0), None);
    }

    /// Bound 100 ms, phase 3..5 s. Unjudged only when the median after the marker AND a render covering the phase both exceed it: a spike in the phase with a fast median (a healthy host's
    /// hiccup), or a slow median with fast phase renders, stay judged; 100 ms exactly is not over. Fakes::Fail keeps a pass judged even when starved; Fakes::Both does not; no span: judged.
    #[test]
    fn unjudged_only_when_the_host_and_the_phase_are_starved() {
        let starved_app = [
            paint("3.000", 1000, 300_000),
            paint("4.000", 1000, 300_000),
            paint("5.000", 1000, 300_000),
        ]
        .concat();
        let spike = [
            paint("3.000", 1000, 10_000),
            paint("4.000", 1000, 300_000),
            paint("5.000", 1000, 10_000),
            paint("8.000", 1000, 10_000),
        ]
        .concat();
        let slow_elsewhere = [
            paint("2.000", 500, 10_000),
            paint("5.000", 3000, 90_000),
            paint("8.000", 1000, 300_000),
            paint("9.000", 1000, 300_000),
            paint("10.000", 1000, 300_000),
        ]
        .concat();
        let exact = [paint("3.000", 1000, 100_000), paint("4.000", 1000, 100_000)].concat();
        let phase = Some((3_000, 5_000));
        let run = |app: &str, t| apply(app, "stage", 0, vec![t]);
        let (j, u) = run(&starved_app, timed(false, Fakes::Fail, phase));
        assert!(j.is_empty());
        assert_eq!(
            u[0].1,
            "host GPU starved (window render median 300 ms, max 300 ms in the phase > bound 100 ms)"
        );
        for app in [&spike, &slow_elsewhere, &exact] {
            assert_eq!(
                run(app, timed(false, Fakes::Fail, phase)).1.len(),
                0,
                "{app}"
            );
        }
        assert_eq!(
            run(&starved_app, timed(true, Fakes::Fail, phase)).1.len(),
            0
        );
        assert_eq!(
            run(&starved_app, timed(true, Fakes::Both, phase)).1.len(),
            1
        );
        assert_eq!(
            run(&starved_app, timed(false, Fakes::Both, None)).1.len(),
            0
        );
    }
}
