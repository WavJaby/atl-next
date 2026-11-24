// SPDX-License-Identifier: MIT
//! The gates' one starvation rule (gate-starved-red-design 2.2-2.4): a miss is `undecided` (exit 4, neither pass nor fail) only when the host provably took the margin its bound assumed.
//! Every other miss is judged: the default is red. Two supplies, one outcome:
//! - CPU ([`Budget`]): a declared wait that ran out its bound (`Miss::Late`). Starved when `bound - stalled(span) < need`: what the host left was less than a quiet run needs. `stalled` is the gate's own
//!   cgroup stall, counted only where foreign CPU covers its own (atl-host-load `Samples::stalled`), so a sleeping deadlock (no stall) or a runaway inside the gate (own CPU) stays red.
//!   `need` unset (not measured on a quiet host) = judged; no samples (no scope, a remote run's miss) = unknown = judged.
//! - GPU ([`Render`]): a sub-check whose bound assumes the window renders within an interval (slow-display held / replaced, ui-block commits). Starved when the median of the paint lines' longest
//!   renders after the marker exceeds the interval and a render covering the sub-check's span does too, both from the window's own `render_us` (GSK render + swap, measured by the window itself,
//!   so no defect under test lengthens it without the GPU being slow; measured 2026-11-22: median p99 53 ms over 521 runs with the GPU free, p10 210 ms over 54 runs with it held at 94-100 %).
//!   Only the outcomes starvation can produce are unjudged ([`Fakes`]).
//! - Product timers ([`TIMERS`]): a red whose services log holds a fired timer is judged by that timer, as a wait with `bound = period` over `[fire - period, fire]` (the fire's `at=` stamp).
//!   A timer that did not lose its margin makes the red real; a fired line without `at=` is unknown, so judged.
//!
//! Each judgement prints one `starvation: ` line (stage, what, outcome, measures) and one `starvation-ruling: ` JSON line (xtask harness.rs `Ruling`); an undecided verdict prints `starvation-verdict: `.
//! Logs count how often the rule fires and refuses; the parent reads the JSON, never the wording.

use std::process::ExitCode;
use std::time::Duration;

use atl_host_load::{Samples, Span};
use atl_services_types::hw_timeouts::{
    HwTimeouts, PENDING_COLD_START_ABANDON_BASE_MS, PROC_START_TIMEOUT_BASE_MS,
};
use xtask::harness::{RULING_PREFIX, Ruling, Supply, VERDICT_PREFIX};

use crate::composer::word;
use crate::wait::Miss;
use crate::window::{Sub, line_stamp};

/// The platform's per-second paint line (atl-platform-ui-linux paint_stats.rs): `ms=` the time it covers, `render_us=mean/max`.
const PAINT: &str = "atl-platform-ui: paint ";

/// The stamps (ms) of the first lines holding `from` and `to`; None without both.
pub(crate) fn span(app: &str, from: &str, to: &str) -> Option<(u64, u64)> {
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
pub(crate) fn render_max_us(app: &str, (start, end): (u64, u64)) -> Option<u64> {
    paints(app)
        .filter(|&(at, ms, _)| at >= start && at.saturating_sub(ms) <= end)
        .map(|(_, _, max)| max)
        .max()
}

/// The host's render over the run: median and max (us) of the paint lines' longest renders stamped at or after `from` (ms), and their count. The first paints (startup: shader compiles, first
/// texture uploads, 1.3 s median on a free GPU) come before the marker, so they do not count. None without a paint line.
pub(crate) fn host_render_us(app: &str, from: u64) -> Option<(u64, u64, usize)> {
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
    /// Only a fail: starvation only removes what the bound counts (ui-block commits, a late wait), so a pass stands.
    Fail,
    /// Both: starvation moves the count either way (slow-display held: expired holds post more, a starved producer posts less).
    Both,
}

/// The render interval a GPU-timed sub-check assumes: `limit_us` over `span` (log stamps, ms; None: no span found, so judged). `limit` names it in the reason, e.g. `hold bound 100 ms`.
pub struct Render {
    pub limit_us: f64,
    pub limit: String,
    pub span: Option<(u64, u64)>,
}

/// A declared wait's CPU budget (design 2.2): the wait `wait` had `bound`, a quiet host needs `need` (None: not measured yet, so judged), and it ran over `span` (CLOCK_MONOTONIC ns).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Budget {
    pub(crate) wait: String,
    pub(crate) bound: Duration,
    pub(crate) need: Option<Duration>,
    pub(crate) span: Span,
}

/// A sub-check whose bound assumes a supply: the window's render (`gpu`), the host's CPU over a declared wait (`cpu`), or both. Neither: judged.
pub struct Timed {
    pub sub: Sub,
    pub gpu: Option<Render>,
    pub cpu: Option<Budget>,
    pub fakes: Fakes,
}

/// A timed sub-check the rule left unjudged: name, its starved ruling, the result as measured (shown, judging nothing).
pub type Undecided = (&'static str, Ruling, Result<String, String>);

/// What the rule reads besides the sub-checks: the stage's name, the app log and the marker's stamp (GPU), and the gate's samples (CPU; None: unknown).
/// `hw`: the run's multiplier for the scaled product timers (None: atld logged none). `subject`: the gate's own timer line (design 2.4): a services line containing it is no fired timer of this run.
pub(crate) struct Run<'a> {
    pub(crate) stage: &'a str,
    pub(crate) app: &'a str,
    pub(crate) from: u64,
    pub(crate) samples: Option<&'a Samples>,
    pub(crate) hw: Option<HwTimeouts>,
    pub(crate) subject: Option<&'a str>,
}

impl<'a> Run<'a> {
    /// A run judged on CPU only (no window paint lines): rows other than `window`.
    pub(crate) fn cpu(stage: &'a str, samples: Option<&'a Samples>) -> Self {
        Self {
            stage,
            app: "",
            from: 0,
            samples,
            hw: None,
            subject: None,
        }
    }
}

/// The GPU form: the ruling on `t` when it is starved on this run, None when judged. `host`: the run's median render (us), None without paint lines.
fn gpu_starved(app: &str, host: Option<u64>, t: &Timed, render: &Render) -> Option<Ruling> {
    let faked = match t.fakes {
        Fakes::Fail => t.sub.1.is_err(),
        Fakes::Both => true,
    };
    let host = host.filter(|&m| faked && m as f64 > render.limit_us)?;
    let reached = render_max_us(app, render.span?).filter(|&r| r as f64 > render.limit_us)?;
    Some(Ruling::starved(
        Supply::Gpu,
        format!(
            "window render median {} ms, max {} ms in the phase > {}",
            host / 1000,
            reached / 1000,
            render.limit
        ),
    ))
}

/// Rule 2 for an event that never came: starved when `bound - stalled(span) < need`. The ruling's text names wait, bound, need and stall; unknown when nothing was measured.
pub(crate) fn cpu(b: &Budget, samples: Option<&Samples>) -> Ruling {
    let Some(samples) = samples else {
        return Ruling::unknown(format!("{}: no gate scope samples", b.wait));
    };
    let Some(stalled_us) = samples.stalled(b.span) else {
        return Ruling::unknown(format!("{}: the samples do not cover the wait", b.wait));
    };
    let stalled = Duration::from_micros(stalled_us);
    let need = b
        .need
        .map_or_else(|| "unset".to_owned(), |n| format!("{} ms", n.as_millis()));
    let text = format!(
        "{} bound {} ms need {need} stalled {} ms",
        b.wait,
        b.bound.as_millis(),
        stalled.as_millis()
    );
    match b.need {
        Some(need) if b.bound.saturating_sub(stalled) < need => Ruling::starved(Supply::Cpu, text),
        _ => Ruling::judged(text),
    }
}

/// Splits a stage's timed sub-checks into judged ones and the ones host starvation left unjudged; prints the judgement lines ([`print_ruling`]) of each unjudged or CPU-refused sub-check.
pub fn apply(run: &Run, timed: Vec<Timed>) -> (Vec<Sub>, Vec<Undecided>) {
    let host = host_render_us(run.app, run.from).map(|(median, _, _)| median);
    let (mut judged, mut undecided) = (Vec::new(), Vec::new());
    for t in timed {
        let gpu = t
            .gpu
            .as_ref()
            .and_then(|r| gpu_starved(run.app, host, &t, r));
        // a late wait only fails: a pass that ran inside its bound stands
        let cpu = match (&t.cpu, &t.sub.1) {
            (Some(b), Err(_)) if gpu.is_none() => {
                let ruling = cpu(b, run.samples);
                if ruling.supply().is_some() {
                    Some(ruling)
                } else {
                    print_ruling(run.stage, "late", None, &ruling);
                    None
                }
            }
            _ => None,
        };
        match gpu.or(cpu) {
            Some(ruling) => {
                let was = if t.sub.1.is_ok() { "pass" } else { "FAIL" };
                print_ruling(run.stage, t.sub.0, Some(was), &ruling);
                undecided.push((t.sub.0, ruling, t.sub.1));
            }
            None => judged.push(t.sub),
        }
    }
    (judged, undecided)
}

/// One judgement's two lines: the human `starvation:` line (`was`: a starved sub-check's pass/FAIL) and the machine `starvation-ruling:` line the gate's parent reads back (check_gates.rs, harness `rulings`).
fn print_ruling(stage: &str, what: &str, was: Option<&str>, ruling: &Ruling) {
    let was = was.map_or_else(String::new, |w| format!(" {w}"));
    println!(
        "starvation: {stage} {what}{was} -> {}: {ruling}",
        ruling.judgement.word()
    );
    println!(
        "{RULING_PREFIX}{}",
        serde_json::to_string(ruling).expect("a ruling serializes")
    );
}

/// A product timer's period: AOSP's constant (`Fixed`), or a base the product scales by the run's `ro.hw_timeout_multiplier` (`Scaled`, hw_timeouts.rs).
pub(crate) enum Period {
    Fixed(Duration),
    /// the base in ms, scaled by [`HwTimeouts::scale`]
    Scaled(u64),
}

/// A product timer a gate can trip (design 2.4): the services' line when it fires (text after `services: `), its period, and the time it needs on a quiet host.
pub(crate) struct Timer {
    pub(crate) name: &'static str,
    pub(crate) fired: &'static str,
    pub(crate) period: Period,
    pub(crate) need: Option<Duration>,
}

/// The product's timers whose firing the services log names (atl-services). Periods: AOSP's constants, `Scaled` ones x the run's `hw timeouts` multiplier; `need` only where kept runs measured it.
pub(crate) const TIMERS: &[Timer] = &[
    // activity.rs KILL_TASK_PROCESSES_TIMEOUT, `remove task N: no destroy within 1s`. need: remove -> kill 7-25 ms over 9 kept logs (gate-starved-red-design 2.4 table; n < 20, those runs' load
    // not recorded): the largest seen, until 20 quiet passes measure it
    Timer {
        name: "removeTask kill 1s",
        fired: ": no destroy within ",
        period: Period::Fixed(Duration::from_millis(1000)),
        need: Some(Duration::from_millis(25)),
    },
    // activity.rs PROC_START_TIMEOUT_MS / active_services.rs process_start_timed_out
    Timer {
        name: "process start 10s",
        fired: "failed to attach: start timeout",
        period: Period::Scaled(PROC_START_TIMEOUT_BASE_MS),
        need: None,
    },
    // broadcasts_am.rs: a cold start the dispatcher stops waiting for
    Timer {
        name: "cold-start abandon 300s",
        fired: "abandoned: not attached within",
        period: Period::Scaled(PENDING_COLD_START_ABANDON_BASE_MS),
        need: None,
    },
];

impl Timer {
    /// The period in this run: AOSP's constant, or the scaled base x the run's multiplier. Err when a scaled period has no multiplier (no `hw timeouts` line): unknown, so judged.
    fn period_of(&self, hw: Option<HwTimeouts>) -> Result<Duration, String> {
        match self.period {
            Period::Fixed(d) => Ok(d),
            Period::Scaled(base) => hw
                .map(|h| Duration::from_millis(h.scale(base)))
                .ok_or_else(|| format!("{}: the atld log has no `hw timeouts` line", self.name)),
        }
    }
}

/// The run's multiplier from atld's `hw timeouts: xN (<source>)` line (atld main.rs `hw_multiplier`); the last such line. None when the log has none.
pub(crate) fn hw_of(atld_log: &str) -> Option<HwTimeouts> {
    atld_log
        .lines()
        .filter_map(|l| {
            let (_, rest) = l.split_once("hw timeouts: x")?;
            HwTimeouts::parse_arg(rest.split(' ').next()?).ok()
        })
        .next_back()
}

/// The fire's wall stamp (ns since the epoch) from the line's `at=<s>.<ms>` (atl_diag::wall_stamp). None without one.
fn fire_wall_ns(line: &str) -> Option<i64> {
    let at = word(line, "at")?;
    let (s, ms) = at.split_once('.')?;
    Some(s.parse::<i64>().ok()? * 1_000_000_000 + ms.parse::<i64>().ok()? * 1_000_000)
}

/// The timer's budget over `[fire - period, fire]` on the samples' clock. Err: why it is unknown.
fn fired_budget(
    t: &Timer,
    line: &str,
    samples: Option<&Samples>,
    hw: Option<HwTimeouts>,
) -> Result<Budget, String> {
    let wall =
        fire_wall_ns(line).ok_or_else(|| format!("{}: fired line has no at= stamp", t.name))?;
    let period = t.period_of(hw)?;
    let to_ns = samples
        .and_then(|s| s.monotonic_of(wall))
        .ok_or_else(|| format!("{}: no gate scope samples", t.name))?;
    let period_ns = i64::try_from(period.as_nanos()).unwrap_or(i64::MAX);
    Ok(Budget {
        wait: t.name.to_owned(),
        bound: period,
        need: t.need,
        span: Span {
            from_ns: to_ns - period_ns,
            to_ns,
        },
    })
}

/// A red run's verdict under the rule: Some(starved ruling) when it is undecided, None when the red stands. `faults`: the run's failing checks (not `not reached`). `services`: the services log.
/// - A fired product timer decides alone (a death or a late answer it caused follows from it): undecided only when every fired timer lost its margin.
/// - Else the run is undecided only when its one fault is a late declared wait whose budget the host took (rule 3: no other check failed).
pub(crate) fn red(run: &Run, faults: &[&Miss], services: &str) -> Option<Ruling> {
    let fired: Vec<(&Timer, &str)> = services
        .lines()
        .filter(|l| !run.subject.is_some_and(|s| l.contains(s)))
        .filter_map(|l| {
            let text = l.split_once("services: ")?.1;
            TIMERS
                .iter()
                .find(|t| text.contains(t.fired))
                .map(|t| (t, l))
        })
        .collect();
    if !fired.is_empty() {
        let mut verdict = None;
        for (t, line) in fired {
            let answer = match fired_budget(t, line, run.samples, run.hw) {
                Ok(b) => cpu(&b, run.samples),
                Err(e) => Ruling::unknown(e),
            };
            print_ruling(run.stage, "timer", None, &answer);
            // a judged or unknown timer refuses the excuse: the red stands
            answer.supply()?;
            verdict.get_or_insert(answer);
        }
        return verdict;
    }
    let [Miss::Late { name, why, budget }] = faults else {
        return None;
    };
    let Some(budget) = budget.clone() else {
        print_ruling(
            run.stage,
            "late",
            None,
            &Ruling::unknown(format!("{name}: no budget (a remote or undeclared wait)")),
        );
        return None;
    };
    let timed = Timed {
        sub: ("wait", Err(why.clone())),
        gpu: None,
        cpu: Some(budget),
        fakes: Fakes::Fail,
    };
    let (_, undecided) = apply(run, vec![timed]);
    undecided.into_iter().next().map(|(_, ruling, _)| ruling)
}

/// The gate's live samples through now: what [`red`] reads in the gate process (atl-host-load `live`).
pub(crate) fn live() -> Option<Samples> {
    atl_host_load::live(atl_host_load::monotonic_ns())
}

/// A row's red under the rule (rows judged on CPU only): prints the undecided verdict and returns exit 4 when [`red`] excuses `faults`; None when the red stands. The caller never passes
/// a run whose atld exited (a death is never excused).
pub(crate) fn judge_row(
    cmd: &str,
    faults: &[Miss],
    services: &str,
    atld_log: &str,
) -> Option<ExitCode> {
    let samples = live();
    let faults: Vec<&Miss> = faults.iter().collect();
    let run = Run {
        hw: hw_of(atld_log),
        ..Run::cpu(cmd, samples.as_ref())
    };
    let ruling = red(&run, &faults, services)?;
    Some(undecided(cmd, cmd, &ruling))
}

/// Prints the undecided verdict: the human line `<cmd>: undecided <stage>: <ruling>` and the machine `starvation-verdict:` line the gate's parent reads (check_gates.rs). Returns exit 4.
/// `starved`: a ruling with a supply, as [`red`] and [`apply`] return.
pub(crate) fn undecided(cmd: &str, stage: &str, starved: &Ruling) -> ExitCode {
    println!(
        "{cmd}: {}{stage}: {starved}",
        crate::verdict::UNDECIDED_WORD
    );
    println!(
        "{VERDICT_PREFIX}{}",
        serde_json::to_string(starved).expect("a ruling serializes")
    );
    ExitCode::from(crate::verdict::UNDECIDED_EXIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atl_host_load::{GateReading, Sample};
    use xtask::harness::Judgement;

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
            gpu: Some(Render {
                limit_us: 100_000.0,
                limit: "bound 100 ms".to_owned(),
                span,
            }),
            cpu: None,
            fakes,
        }
    }

    fn window(app: &str) -> Run<'_> {
        Run {
            stage: "stage",
            app,
            from: 0,
            samples: None,
            hw: None,
            subject: None,
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
        let run = |app: &str, t| apply(&window(app), vec![t]);
        let (j, u) = run(&starved_app, timed(false, Fakes::Fail, phase));
        assert!(j.is_empty());
        assert_eq!(
            u[0].1,
            Ruling::starved(
                Supply::Gpu,
                "window render median 300 ms, max 300 ms in the phase > bound 100 ms"
            )
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

    const S: i64 = 1_000_000_000;

    /// Samples every 0.5 s over 0..2 s; each interval the gate stalls `stall_ms` and runs `own_ms`, with `busy_ms` busy CPU on its set. Wall 1000 s at mono 0.
    fn load(stall_ms: u64, own_ms: u64, busy_ms: u64) -> Samples {
        Samples(
            (0..=4_u64)
                .map(|i| Sample {
                    at_ns: i as i64 * S / 2,
                    wall_ns: 1000 * S + i as i64 * S / 2,
                    gate: Some(GateReading {
                        stall_us: i * stall_ms * 1000,
                        usage_us: i * own_ms * 1000,
                    }),
                    busy_us: Some(i * busy_ms * 1000),
                    ..Default::default()
                })
                .collect(),
        )
    }

    fn budget(need_ms: Option<u64>) -> Budget {
        Budget {
            wait: "event".to_owned(),
            bound: Duration::from_millis(1000),
            need: need_ms.map(Duration::from_millis),
            span: Span {
                from_ns: S / 2,
                to_ns: 3 * S / 2,
            },
        }
    }

    /// Bound 1 s, need 100 ms, span 0.5..1.5 s (two 0.5 s intervals). Stall 480 ms per interval with foreign >= own: stalled 960 ms, 40 ms left < 100 ms: starved.
    /// Stall 400 ms per interval: 200 ms left >= need: judged. Exactly need left (450 ms per interval: 100 ms) is not less: judged. Need unset: judged whatever the stall.
    /// A runaway (own CPU over foreign) counts no stall: judged. No samples, or samples short of the span: unknown.
    #[test]
    fn a_late_wait_is_starved_only_when_the_host_took_its_margin() {
        let starved = cpu(&budget(Some(100)), Some(&load(480, 5, 500)));
        assert_eq!(
            starved,
            Ruling::starved(
                Supply::Cpu,
                "event bound 1000 ms need 100 ms stalled 960 ms"
            )
        );
        let judged = |b: Budget, s: Option<&Samples>| cpu(&b, s).judgement;
        assert_eq!(
            judged(budget(Some(100)), Some(&load(400, 5, 500))),
            Judgement::Judged
        );
        assert_eq!(
            judged(budget(Some(100)), Some(&load(450, 5, 500))),
            Judgement::Judged
        );
        assert_eq!(
            judged(budget(Some(100)), Some(&load(480, 400, 500))),
            Judgement::Judged
        );
        let unset = cpu(&budget(None), Some(&load(480, 5, 500)));
        assert_eq!(unset.judgement, Judgement::Judged);
        assert!(unset.text.contains("need unset"), "{}", unset.text);
        assert_eq!(judged(budget(Some(100)), None), Judgement::Unknown);
        let short = Samples(load(480, 5, 500).0[..2].to_vec());
        assert_eq!(judged(budget(Some(100)), Some(&short)), Judgement::Unknown);
    }

    fn late(budget: Option<Budget>) -> Miss {
        Miss::Late {
            name: "event".to_owned(),
            why: "no event within 1s".to_owned(),
            budget,
        }
    }

    /// One fault, a late wait whose budget the host took: undecided. A second fault (rule 3), a death, a late wait without a budget (remote): the red stands.
    #[test]
    fn a_red_is_undecided_only_when_its_one_fault_is_a_starved_late_wait() {
        let samples = load(480, 5, 500);
        let run = Run::cpu("host-load", Some(&samples));
        let starved = late(Some(budget(Some(100))));
        assert_eq!(
            red(&run, &[&starved], ""),
            Some(Ruling::starved(
                Supply::Cpu,
                "event bound 1000 ms need 100 ms stalled 960 ms"
            ))
        );
        let wrong = Miss::Wrong("picture differs".to_owned());
        assert_eq!(red(&run, &[&starved, &wrong], ""), None);
        let died = Miss::Exited {
            name: "event".to_owned(),
            status: "signal: 6".to_owned(),
        };
        assert_eq!(red(&run, &[&died], ""), None);
        assert_eq!(red(&run, &[&late(None)], ""), None);
        assert_eq!(red(&run, &[], ""), None);
    }

    /// A fired timer decides the red alone. The 1 s kill timer fired at wall 1001.5 s = mono 1.5 s, over 0.5..1.5 s: stalled 980 ms of its 1 s, 20 ms left < need 25 ms: undecided, naming the timer, even with a
    /// wrong check beside it (the kill caused it). On a quiet host (stall 10 ms per interval) it is judged and the red stands. Without `at=` (product lines before H6): unknown, the red stands.
    #[test]
    fn a_fired_timer_is_judged_over_its_own_period() {
        let fired = "1001.600 I services: remove task 7: no destroy within 1s at=1001.500\n";
        let wrong = Miss::Wrong("ended: the process outlived the close".to_owned());
        let starved = load(490, 5, 500);
        assert_eq!(
            red(&Run::cpu("capture", Some(&starved)), &[&wrong], fired),
            Some(Ruling::starved(
                Supply::Cpu,
                "removeTask kill 1s bound 1000 ms need 25 ms stalled 980 ms"
            ))
        );
        let quiet = load(10, 5, 500);
        assert_eq!(
            red(&Run::cpu("capture", Some(&quiet)), &[&wrong], fired),
            None
        );
        let unstamped = "1001.600 I services: remove task 7: no destroy within 1s\n";
        assert_eq!(
            red(&Run::cpu("capture", Some(&starved)), &[&wrong], unstamped),
            None
        );
    }

    /// `at=` is atl_diag::wall_stamp's `<s>.<ms>`.
    #[test]
    fn fire_stamp_reads_wall_ms() {
        assert_eq!(
            fire_wall_ns("services: x at=1001.050"),
            Some(1001 * S + 50_000_000)
        );
        assert_eq!(fire_wall_ns("services: x"), None);
    }

    /// The last `hw timeouts: xN` line of atld's log; none without one.
    #[test]
    fn the_multiplier_is_the_last_hw_timeouts_line() {
        assert_eq!(
            hw_of("atld: hw timeouts: x2 (host file)\n"),
            HwTimeouts::parse_arg("2").ok()
        );
        assert_eq!(
            hw_of("atld: hw timeouts: x3 (host file)\natld: hw timeouts: x1 (default)\n"),
            Some(HwTimeouts::default())
        );
        assert_eq!(hw_of("atld: started\n"), None);
    }

    /// A scaled period is the base x the run's multiplier; the kill timer (AOSP's constant) needs none. A scaled period without the multiplier is unknown.
    #[test]
    fn scaled_timers_take_the_runs_multiplier() {
        let samples = load(0, 0, 0);
        let row = |name: &str| TIMERS.iter().find(|t| t.name == name).expect("a table row");
        let start =
            "1001.600 I services: process p (pid 9) failed to attach: start timeout at=1001.500";
        let x2 = HwTimeouts::parse_arg("2").ok();
        let b = fired_budget(row("process start 10s"), start, Some(&samples), x2)
            .expect("stamped line, multiplier known");
        assert_eq!(b.bound, Duration::from_secs(20));
        assert_eq!(b.span.to_ns - b.span.from_ns, 20 * S);
        assert!(fired_budget(row("process start 10s"), start, Some(&samples), None).is_err());
        let kill = "1001.600 I services: remove task 7: no destroy within 1s at=1001.500";
        let b = fired_budget(row("removeTask kill 1s"), kill, Some(&samples), None)
            .expect("the fixed period needs no multiplier");
        assert_eq!(b.bound, Duration::from_secs(1));
    }

    /// design 2.4: the gate's own timer line is no fired timer of its run. The other task's line lies outside the samples (unknown, judged), so only the exclusion lets the starved line decide.
    #[test]
    fn the_gates_own_timer_line_is_no_fired_timer_of_its_run() {
        let starved = load(490, 5, 500);
        let services = "1001.600 I services: remove task 1: no destroy within 1s at=1001.500\n1002.100 I services: remove task 2: no destroy within 1s at=1002.050\n";
        let wrong = Miss::Wrong("ended: killed by the timeout".to_owned());
        assert_eq!(
            red(&Run::cpu("capture", Some(&starved)), &[&wrong], services),
            None
        );
        let own = Run {
            subject: Some("remove task 2: no destroy within "),
            ..Run::cpu("capture", Some(&starved))
        };
        assert_eq!(
            red(&own, &[&wrong], services),
            Some(Ruling::starved(
                Supply::Cpu,
                "removeTask kill 1s bound 1000 ms need 25 ms stalled 980 ms"
            ))
        );
    }

    /// The table's text against lines the product printed (testdata/timers/services.log, copied from runs): the kill line of the H6 `--starve-close` run (stamped), the start-timeout line of a kept run (before H6, no `at=`).
    /// No run has printed the cold-start abandon line, so that row is not checked here.
    #[test]
    fn table_texts_match_lines_the_product_printed() {
        let log = include_str!("../testdata/timers/services.log");
        for name in ["removeTask kill 1s", "process start 10s"] {
            let t = TIMERS.iter().find(|t| t.name == name).expect("a table row");
            assert!(
                log.lines().any(|l| l.contains(t.fired)),
                "{name}: no product line"
            );
        }
        let kill = log
            .lines()
            .find(|l| l.contains(": no destroy within "))
            .expect("the kill line");
        assert!(
            fire_wall_ns(kill).is_some(),
            "the stamped kill line parses: {kill}"
        );
    }
}
