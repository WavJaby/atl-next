// SPDX-License-Identifier: MIT
//! The sampler: one thread reads the system PSI, the gate's scope and the busy CPU of the gate's allowed set every [`SAMPLE_PERIOD`]; [`Samples`] answers over a [`Span`].
//! Stamps: `at_ns` CLOCK_MONOTONIC (the clock frame times are on), `wall_ns` CLOCK_REALTIME, both read after the files.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustix::param::clock_ticks_per_second;
use rustix::time::{ClockId, clock_gettime};

use crate::scope;

/// System-wide PSI. CPU is the input of `judge_window`; io and memory are printed, not judged.
const PSI_CPU: &str = "/proc/pressure/cpu";
const PSI_IO: &str = "/proc/pressure/io";
const PSI_MEMORY: &str = "/proc/pressure/memory";
const PROC_STAT: &str = "/proc/stat";
const PROC_SELF_STATUS: &str = "/proc/self/status";
/// This thread's scheduler stats, `run_time run_delay timeslices` in ns (CONFIG_SCHED_INFO).
const THREAD_SCHEDSTAT: &str = "/proc/thread-self/schedstat";
/// How often the sampler reads. A tenth of the shortest judged window (31 callbacks at 60 Hz = 500 ms) keeps the bracketing samples within 20% of the window's span.
pub const SAMPLE_PERIOD: Duration = Duration::from_millis(50);
const PERIOD_NS: i64 = SAMPLE_PERIOD.as_nanos() as i64;

/// The gate's scope at one reading: `cpu.pressure` `some total` and `cpu.stat` `usage_usec`, both since the scope began.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GateReading {
    pub stall_us: u64,
    pub usage_us: u64,
}

/// One reading. `stall_us`, `io_us`, `mem_us`: system-wide `some` totals (us). `busy_us`: busy CPU of the sampler's CPU set since boot (user, nice, system, irq, softirq, steal).
/// `late_ns`: the sampler's wake lateness since the previous reading less its own run delay, i.e. how late the sampler itself woke, not the host's.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Sample {
    pub at_ns: i64,
    pub wall_ns: i64,
    pub stall_us: u64,
    pub gate: Option<GateReading>,
    pub busy_us: Option<u64>,
    pub io_us: Option<u64>,
    pub mem_us: Option<u64>,
    pub late_ns: u64,
}

/// A window of the run, CLOCK_MONOTONIC ns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub from_ns: i64,
    pub to_ns: i64,
}

/// Samples in time order.
#[derive(Default, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Samples(pub Vec<Sample>);

/// Between two consecutive samples: the gate's `some` stall, its own CPU, and the foreign CPU (busy minus own) of its allowed set.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Interval {
    stall_us: u64,
    own_us: u64,
    foreign_us: u64,
}

impl Samples {
    /// Stall time / wall time from the last sample at or before `from_ns` to the first at or after `to_ns`. `None` when the samples do not bracket the window.
    pub fn stall_share(&self, from_ns: i64, to_ns: i64) -> Option<f64> {
        let a = self.0.iter().rev().find(|s| s.at_ns <= from_ns)?;
        let b = self.0.iter().find(|s| s.at_ns >= to_ns)?;
        let wall_us = (b.at_ns - a.at_ns) as f64 / 1e3;
        if wall_us <= 0.0 {
            return None;
        }
        Some(b.stall_us.saturating_sub(a.stall_us) as f64 / wall_us)
    }

    /// [`Self::stall_share`] over the whole sampled span, i.e. the sampler's lifetime (one run). `None` with fewer than two samples.
    pub fn span_share(&self) -> Option<f64> {
        let span = self.span()?;
        self.stall_share(span.from_ns, span.to_ns)
    }

    /// The first sample to the last. `None` under two samples.
    pub fn span(&self) -> Option<Span> {
        let (first, last) = (self.0.first()?, self.0.last()?);
        (self.0.len() >= 2).then_some(Span {
            from_ns: first.at_ns,
            to_ns: last.at_ns,
        })
    }

    /// The intervals wholly inside `span`. Unknown (`None`) when the samples do not cover `span`, or an interval inside it lacks the gate's scope or the busy CPU: a partial record is never summed.
    fn intervals(&self, span: Span) -> Option<Vec<Interval>> {
        let covered = self.0.first()?.at_ns <= span.from_ns && self.0.last()?.at_ns >= span.to_ns;
        if !covered {
            return None;
        }
        let intervals: Vec<Interval> = self
            .0
            .windows(2)
            .filter(|w| w[0].at_ns >= span.from_ns && w[1].at_ns <= span.to_ns)
            .map(|w| {
                let (a, b) = (&w[0], &w[1]);
                let (ga, gb) = (a.gate?, b.gate?);
                let own_us = gb.usage_us.saturating_sub(ga.usage_us);
                let busy_us = b.busy_us?.saturating_sub(a.busy_us?);
                Some(Interval {
                    stall_us: gb.stall_us.saturating_sub(ga.stall_us),
                    own_us,
                    foreign_us: busy_us.saturating_sub(own_us),
                })
            })
            .collect::<Option<_>>()?;
        (!intervals.is_empty()).then_some(intervals)
    }

    /// The gate's stall (us) over `span`, counting only the intervals whose foreign CPU is at least its own. A runaway inside the gate makes its own CPU the larger part, so its waits are not excused.
    pub fn stalled(&self, span: Span) -> Option<u64> {
        let counted = self
            .intervals(span)?
            .into_iter()
            .filter(|i| i.foreign_us >= i.own_us);
        Some(counted.map(|i| i.stall_us).sum())
    }

    /// The gate's own CPU (us) over `span`.
    pub fn own(&self, span: Span) -> Option<u64> {
        Some(self.intervals(span)?.iter().map(|i| i.own_us).sum())
    }

    /// The busy CPU (us) of the sampler's CPU set that was not the gate's, over `span`.
    pub fn foreign(&self, span: Span) -> Option<u64> {
        Some(self.intervals(span)?.iter().map(|i| i.foreign_us).sum())
    }

    /// The largest wake lateness (ns) of the sampler over the run.
    pub fn late_max_ns(&self) -> Option<u64> {
        self.0.iter().map(|s| s.late_ns).max()
    }

    /// The system-wide PSI share (`field`: stall time over wall) between the first and last sample inside `span`. `None` where one of them lacks the file.
    pub fn psi_share(&self, span: Span, field: fn(&Sample) -> Option<u64>) -> Option<f64> {
        let inside: Vec<&Sample> = self
            .0
            .iter()
            .filter(|s| s.at_ns >= span.from_ns && s.at_ns <= span.to_ns)
            .collect();
        let (first, last) = (inside.first()?, inside.last()?);
        let wall_us = (last.at_ns - first.at_ns) as f64 / 1e3;
        if wall_us <= 0.0 {
            return None;
        }
        Some(field(last)?.saturating_sub(field(first)?) as f64 / wall_us)
    }
}

/// The running sampler; [`Sampler::stop`] ends it and returns what it read.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<Sample>>>,
    thread: JoinHandle<()>,
}

impl Sampler {
    /// Starts the thread, reading every [`SAMPLE_PERIOD`]: the system PSI, the scope at `gate` (a cgroup dir) when given, and the busy CPU of `cpus` when given.
    /// Without `cpus` no interval is complete (no foreign CPU), so no stall is counted.
    pub fn start_for(gate: Option<&Path>, cpus: Option<&[u32]>) -> Self {
        let gate: Option<PathBuf> = gate.map(Path::to_path_buf);
        let cpus: Option<Vec<u32>> = cpus.map(<[u32]>::to_vec);
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = Arc::clone(&stop);
            let samples = Arc::clone(&samples);
            thread::spawn(move || {
                let mut prev: Option<(i64, u64)> = None;
                loop {
                    let done = stop.load(Ordering::Relaxed);
                    if let Some(mut sample) = read(gate.as_deref(), cpus.as_deref()) {
                        let delay = run_delay_ns();
                        if let Some((at_ns, was)) = prev {
                            sample.late_ns =
                                lateness_ns(sample.at_ns - at_ns, delay.saturating_sub(was));
                        }
                        prev = Some((sample.at_ns, delay));
                        samples.lock().expect("sampler lock").push(sample);
                    }
                    if done {
                        break;
                    }
                    thread::sleep(SAMPLE_PERIOD);
                }
            })
        };
        Self {
            stop,
            samples,
            thread,
        }
    }

    /// [`Self::start_for`] on this process's own scope and allowed CPUs: what the window and g4 gates judge with.
    pub fn start() -> Self {
        let gate = scope::own().ok().map(|s| s.dir);
        Self::start_for(gate.as_deref(), allowed_cpus().as_deref())
    }

    pub fn stop(self) -> Samples {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
        let v = std::mem::take(&mut *self.samples.lock().expect("sampler lock"));
        Samples(v)
    }
}

/// One reading. None when the system PSI CPU file is unreadable (no CONFIG_PSI): nothing is recorded.
fn read(gate: Option<&Path>, cpus: Option<&[u32]>) -> Option<Sample> {
    let stall_us = psi_some(PSI_CPU)?;
    let gate = gate.and_then(read_gate);
    let busy_us = cpus.and_then(|cpus| {
        let stat = fs::read_to_string(PROC_STAT).ok()?;
        busy_us_of(&stat, cpus, clock_ticks_per_second())
    });
    let (io_us, mem_us) = (psi_some(PSI_IO), psi_some(PSI_MEMORY));
    Some(Sample {
        at_ns: clock_ns(ClockId::Monotonic),
        wall_ns: clock_ns(ClockId::Realtime),
        stall_us,
        gate,
        busy_us,
        io_us,
        mem_us,
        late_ns: 0,
    })
}

fn read_gate(dir: &Path) -> Option<GateReading> {
    Some(GateReading {
        stall_us: psi_some(dir.join("cpu.pressure"))?,
        usage_us: usage_us_of(&fs::read_to_string(dir.join("cpu.stat")).ok()?)?,
    })
}

fn psi_some(path: impl AsRef<Path>) -> Option<u64> {
    some_total(&fs::read_to_string(path).ok()?)
}

/// The `total=` (us) of the `some` line of a PSI file (Documentation/accounting/psi.rst), not the `full` line's.
fn some_total(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|w| w.strip_prefix("total="))?
        .parse()
        .ok()
}

/// `usage_usec` (us) of a cgroup's `cpu.stat`.
fn usage_us_of(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("usage_usec "))?
        .trim()
        .parse()
        .ok()
}

/// Busy CPU (us) of the CPUs in `cpus` from `/proc/stat`'s `cpuN` lines. Busy is user, nice, system, irq, softirq and steal: every field but idle and iowait.
/// Fields are USER_HZ ticks, `hz` per second.
fn busy_us_of(stat: &str, cpus: &[u32], hz: u64) -> Option<u64> {
    const FIELDS: [usize; 6] = [0, 1, 2, 5, 6, 7];
    let mut ticks = 0_u64;
    let mut seen = false;
    for line in stat.lines() {
        let mut words = line.split_whitespace();
        let Some(cpu) = words
            .next()
            .and_then(|name| name.strip_prefix("cpu")?.parse::<u32>().ok())
        else {
            continue;
        };
        if !cpus.contains(&cpu) {
            continue;
        }
        let fields: Vec<u64> = words.map(|w| w.parse().ok()).collect::<Option<_>>()?;
        ticks += FIELDS
            .iter()
            .map(|&i| fields.get(i).copied())
            .sum::<Option<u64>>()?;
        seen = true;
    }
    seen.then(|| ticks * 1_000_000 / hz.max(1))
}

/// The CPUs this process may run on: `Cpus_allowed_list` of `/proc/self/status` (`0-3,8`).
pub fn allowed_cpus() -> Option<Vec<u32>> {
    let status = fs::read_to_string(PROC_SELF_STATUS).ok()?;
    let list = status
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))?;
    parse_cpu_list(list.trim())
}

/// A kernel CPU list: numbers and `a-b` ranges, comma-separated.
fn parse_cpu_list(list: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in list.split(',') {
        match part.split_once('-') {
            Some((first, last)) => {
                cpus.extend(first.trim().parse::<u32>().ok()?..=last.trim().parse().ok()?)
            }
            None => cpus.push(part.trim().parse().ok()?),
        }
    }
    (!cpus.is_empty()).then_some(cpus)
}

/// Wake lateness of one step (ns): the time between two readings less the period and the sampler's own run delay over the step, clamped at 0.
fn lateness_ns(step_ns: i64, run_delay_ns: u64) -> u64 {
    let delay = i64::try_from(run_delay_ns).unwrap_or(i64::MAX);
    u64::try_from(step_ns - PERIOD_NS - delay).unwrap_or(0)
}

/// This thread's run delay (ns): time runnable and waiting for a CPU. 0 when the kernel does not count it.
fn run_delay_ns() -> u64 {
    fs::read_to_string(THREAD_SCHEDSTAT)
        .ok()
        .and_then(|t| t.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0)
}

fn clock_ns(id: ClockId) -> i64 {
    let t = clock_gettime(id);
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

/// A reading with the gate's scope and the busy CPU of its set (tests: the system PSI is not used).
#[cfg(test)]
pub(crate) fn gate_at(at_ns: i64, stall_us: u64, usage_us: u64, busy_us: u64) -> Sample {
    Sample {
        at_ns,
        gate: Some(GateReading { stall_us, usage_us }),
        busy_us: Some(busy_us),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `some` line's total, not the `full` line's (format of Documentation/accounting/psi.rst).
    #[test]
    fn reads_some_total() {
        let text = "some avg10=19.01 avg60=24.66 avg300=24.53 total=1843699894\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(some_total(text), Some(1_843_699_894));
        assert_eq!(some_total("full total=3\n"), None);
    }

    #[test]
    fn reads_usage_usec() {
        assert_eq!(
            usage_us_of("usage_usec 11537\nuser_usec 9000\nsystem_usec 2537\n"),
            Some(11_537)
        );
        assert_eq!(usage_us_of("user_usec 9000\n"), None);
    }

    /// Kernel CPU lists: ranges and single numbers; an empty list is no list.
    #[test]
    fn expands_cpu_lists() {
        assert_eq!(
            parse_cpu_list("0-3,8,10-11"),
            Some(vec![0, 1, 2, 3, 8, 10, 11])
        );
        assert_eq!(parse_cpu_list("5"), Some(vec![5]));
        assert_eq!(parse_cpu_list(""), None);
    }

    /// Busy = user + nice + system + irq + softirq + steal, in ticks, then us at 100 ticks per second: cpu0 = 100+0+50+5+5+0 = 160 ticks = 1.6 s; cpu1 = 40+0+10 = 50 ticks; idle and iowait never count.
    #[test]
    fn busy_counts_the_set_in_us() {
        let stat = "cpu 999 0 999 999 0 0 0 0 0 0\ncpu0 100 0 50 800 10 5 5 0 0 0\ncpu1 40 0 10 900 0 0 0 0 0 0\nintr 1\n";
        assert_eq!(busy_us_of(stat, &[0], 100), Some(1_600_000));
        assert_eq!(busy_us_of(stat, &[0, 1], 100), Some(2_100_000));
        assert_eq!(busy_us_of(stat, &[7], 100), None);
    }

    /// Step 50.2 ms with 0.1 ms of run delay: 0.1 ms late. On time, or early by timer rounding: 0.
    #[test]
    fn lateness_is_the_step_less_period_and_run_delay() {
        assert_eq!(lateness_ns(50_200_000, 100_000), 100_000);
        assert_eq!(lateness_ns(50_000_000, 0), 0);
        assert_eq!(lateness_ns(49_900_000, 0), 0);
    }

    /// Samples every 1 s. Interval 1 (own 0.3 s, busy 0.7 s -> foreign 0.4 s >= own) counts its 0.1 s stall; interval 2 (own 0.1 s, busy 0.1 s -> foreign 0) does not, though it stalled 0.3 s.
    /// Totals over 0..2 s: stalled 0.1 s, own 0.4 s, foreign 0.4 s.
    #[test]
    fn counts_stall_only_where_foreign_covers_own() {
        let s = Samples(vec![
            gate_at(0, 0, 0, 0),
            gate_at(1_000_000_000, 100_000, 300_000, 700_000),
            gate_at(2_000_000_000, 400_000, 400_000, 800_000),
        ]);
        let span = s.span().expect("span");
        assert_eq!(s.stalled(span), Some(100_000));
        assert_eq!(s.own(span), Some(400_000));
        assert_eq!(s.foreign(span), Some(400_000));
    }

    /// Only intervals wholly inside the span count: 1..2 s of a 0..2 s run is the second interval alone (own 0.1 s, foreign 0.0, not counted).
    #[test]
    fn span_takes_only_its_own_intervals() {
        let s = Samples(vec![
            gate_at(0, 0, 0, 0),
            gate_at(1_000_000_000, 100_000, 300_000, 700_000),
            gate_at(2_000_000_000, 400_000, 400_000, 800_000),
        ]);
        let inner = Span {
            from_ns: 1_000_000_000,
            to_ns: 2_000_000_000,
        };
        assert_eq!(s.own(inner), Some(100_000));
        assert_eq!(s.foreign(inner), Some(0));
        assert_eq!(s.stalled(inner), Some(0));
    }

    /// Unknown, never partial: a span the samples do not cover, or an interval inside it without a busy reading.
    #[test]
    fn unbracketed_or_incomplete_is_unknown() {
        let full = Samples(vec![
            gate_at(0, 0, 0, 0),
            gate_at(1_000_000_000, 0, 0, 0),
            gate_at(2_000_000_000, 0, 0, 0),
        ]);
        let beyond = Span {
            from_ns: 0,
            to_ns: 3_000_000_000,
        };
        assert_eq!(full.stalled(beyond), None);
        let gap = Samples(vec![
            gate_at(0, 0, 0, 0),
            Sample {
                busy_us: None,
                ..gate_at(1_000_000_000, 0, 0, 0)
            },
            gate_at(2_000_000_000, 0, 0, 0),
        ]);
        let whole = gap.span().expect("span");
        assert_eq!(gap.stalled(whole), None);
        assert_eq!(gap.own(whole), None);
    }

    /// The largest lateness step is the answer; a run with no samples has none.
    #[test]
    fn late_max_is_the_largest_step() {
        let s = Samples(vec![
            Sample {
                late_ns: 0,
                ..Default::default()
            },
            Sample {
                late_ns: 180_000,
                ..Default::default()
            },
            Sample {
                late_ns: 280_000,
                ..Default::default()
            },
            Sample {
                late_ns: 120_000,
                ..Default::default()
            },
        ]);
        assert_eq!(s.late_max_ns(), Some(280_000));
        assert_eq!(Samples::default().late_max_ns(), None);
    }

    /// io `some` 250 ms over 1 s of wall: 25%.
    #[test]
    fn psi_share_over_the_span() {
        let s = Samples(vec![
            Sample {
                at_ns: 0,
                io_us: Some(0),
                ..Default::default()
            },
            Sample {
                at_ns: 1_000_000_000,
                io_us: Some(250_000),
                ..Default::default()
            },
        ]);
        let span = s.span().expect("span");
        assert_eq!(s.psi_share(span, |x| x.io_us), Some(0.25));
        assert_eq!(s.psi_share(span, |x| x.mem_us), None);
    }

    /// The live host: at least two samples, stamps rising.
    #[test]
    fn sampler_reads_the_host() {
        if fs::read_to_string(PSI_CPU).is_err() {
            return;
        }
        let s = Sampler::start();
        thread::sleep(SAMPLE_PERIOD * 2);
        let got = s.stop();
        assert!(got.0.len() >= 2, "{got:?}");
        assert!(got.0.windows(2).all(|w| w[0].at_ns < w[1].at_ns));
    }
}
