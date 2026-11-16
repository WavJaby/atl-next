// SPDX-License-Identifier: MIT
//! Wall time per step of a multi-step command (`check` steps, `land` stages): shown on each step's own line and as a closing table, so a build or check optimisation is chosen by measured share, not by guess.
//! [`phase`] is the finer clock of one process (the drive-tap step): exclusive time per named phase, the cargo lock waits inside each, and [`report`] of both.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A duration as `<s>.<tenths>s`: tenths, since steps run from milliseconds to many minutes and the table ranks them, not profiles them.
pub fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

/// Each step's wall time, in run order, and its pool wait when the step took a grant (build_pool.rs).
#[derive(Default)]
pub struct Timings(Vec<(String, Duration, Option<Duration>)>);

impl Timings {
    pub fn push(&mut self, step: &str, took: Duration) {
        self.0.push((step.to_owned(), took, None));
    }

    /// A step that queued `wait` for its pool grant before running `took`.
    pub fn push_waited(&mut self, step: &str, took: Duration, wait: Duration) {
        self.0.push((step.to_owned(), took, Some(wait)));
    }

    /// `<cmd> times:` then one row per step, longest first (ties keep run order): name, seconds, share of the total; then the total.
    /// With pool waits recorded: a `wait` column (excluded from the step's seconds) and a `pool waits: <n>, <s>` line.
    pub fn table(&self, cmd: &str) -> String {
        let total: Duration = self.0.iter().map(|r| r.1).sum();
        let pooled = self.0.iter().any(|r| r.2.is_some());
        let mut rows: Vec<&(String, Duration, Option<Duration>)> = self.0.iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        let width = rows
            .iter()
            .map(|r| r.0.len())
            .max()
            .unwrap_or(0)
            .max("total".len());
        let mut out = format!("{cmd} times:\n");
        for (name, took, wait) in rows {
            let share = if total.is_zero() {
                0.0
            } else {
                100.0 * took.as_secs_f64() / total.as_secs_f64()
            };
            out.push_str(&format!(
                "  {name:<width$} {:>8} {share:>5.1}%",
                secs(*took)
            ));
            if let Some(w) = wait.filter(|w| !w.is_zero()) {
                out.push_str(&format!(" wait {}", secs(w)));
            }
            out.push('\n');
        }
        out.push_str(&format!("  {:<width$} {:>8}\n", "total", secs(total)));
        if pooled {
            let waits: Vec<Duration> = self
                .0
                .iter()
                .filter_map(|r| r.2)
                .filter(|w| !w.is_zero())
                .collect();
            out.push_str(&format!(
                "{cmd} pool waits: {}, {}\n",
                waits.len(),
                secs(waits.iter().sum())
            ));
        }
        out
    }
}

/// The open phases of this process, and each phase name's totals.
struct Clock {
    frames: Vec<Frame>,
    rows: BTreeMap<String, Row>,
    waits: BTreeMap<String, Wait>,
}

struct Frame {
    name: String,
    started: Instant,
    /// Time of the phases nested in this one, closed or added.
    nested: Duration,
}

#[derive(Default)]
struct Row {
    /// Time not inside a nested phase: the rows sum to the outermost phase's wall time.
    exclusive: Duration,
    /// Time including nested phases: overlaps other rows when `composite`.
    inclusive: Duration,
    count: u32,
    composite: bool,
    /// Lock waits that ended inside this phase (part of its exclusive time).
    waited: Duration,
}

#[derive(Default)]
struct Wait {
    count: u32,
    total: Duration,
    longest: Duration,
    /// The holder named by the last announced wait on this lock.
    holder: Option<String>,
}

impl Clock {
    const fn new() -> Self {
        Clock {
            frames: Vec::new(),
            rows: BTreeMap::new(),
            waits: BTreeMap::new(),
        }
    }

    fn enter(&mut self, name: &str, at: Instant) {
        self.frames.push(Frame {
            name: name.to_owned(),
            started: at,
            nested: Duration::ZERO,
        });
    }

    fn exit(&mut self, at: Instant) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let took = at.saturating_duration_since(frame.started);
        let row = self.rows.entry(frame.name).or_default();
        row.exclusive += took.saturating_sub(frame.nested);
        row.inclusive += took;
        row.count += 1;
        row.composite |= !frame.nested.is_zero();
        if let Some(parent) = self.frames.last_mut() {
            parent.nested += took;
        }
    }

    /// An interval of `took` inside the open phase, for a span no call wraps: boot and watch of a run, measured between two marks.
    fn add(&mut self, name: &str, took: Duration) {
        let row = self.rows.entry(name.to_owned()).or_default();
        row.exclusive += took;
        row.inclusive += took;
        row.count += 1;
        if let Some(parent) = self.frames.last_mut() {
            parent.nested += took;
        }
    }

    fn wait(&mut self, what: &str, took: Duration, holder: Option<String>) {
        let wait = self.waits.entry(what.to_owned()).or_default();
        wait.count += 1;
        wait.total += took;
        wait.longest = wait.longest.max(took);
        if holder.is_some() {
            wait.holder = holder;
        }
        if let Some(frame) = self.frames.last() {
            self.rows.entry(frame.name.clone()).or_default().waited += took;
        }
    }

    fn render(&self, cmd: &str) -> String {
        let mut table = Timings::default();
        for (name, row) in &self.rows {
            let mut label = format!("{name} (x{}", row.count);
            if !row.waited.is_zero() {
                label.push_str(&format!(", lock wait {}", secs(row.waited)));
            }
            label.push(')');
            table.push(&label, row.exclusive);
        }
        let mut out = table.table(cmd);
        let composite: Vec<String> = self
            .rows
            .iter()
            .filter(|(_, row)| row.composite)
            .map(|(name, row)| format!("{name} {} (x{})", secs(row.inclusive), row.count))
            .collect();
        if !composite.is_empty() {
            out.push_str(&format!("{cmd} inclusive: {}\n", composite.join(", ")));
        }
        for (what, wait) in &self.waits {
            out.push_str(&format!(
                "{cmd} lock wait: {what}: {} x{}, longest {}",
                secs(wait.total),
                wait.count,
                secs(wait.longest)
            ));
            if let Some(holder) = &wait.holder {
                out.push_str(&format!(", last announced holder {holder}"));
            }
            out.push('\n');
        }
        out
    }
}

static CLOCK: Mutex<Clock> = Mutex::new(Clock::new());

fn clock() -> MutexGuard<'static, Clock> {
    CLOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `f` as phase `name`: its wall time less the phases it nests is `name`'s exclusive time.
pub fn phase<T>(name: &str, f: impl FnOnce() -> T) -> T {
    clock().enter(name, Instant::now());
    let out = f();
    clock().exit(Instant::now());
    out
}

/// Adds `took` to phase `name` as an interval inside the open phase.
pub fn add(name: &str, took: Duration) {
    clock().add(name, took);
}

/// A lock wait of `took` on `what` (cargo's name for the lock), counted in the open phase; `holder` when it was announced.
pub fn record_wait(what: &str, took: Duration, holder: Option<String>) {
    clock().wait(what, took, holder);
}

/// This process's phase report: `<cmd> times:` (the exclusive rows, summing to the outermost phase), then the composite phases' inclusive times and each lock's waits.
pub fn report(cmd: &str) -> String {
    clock().render(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_longest_first_with_their_share() {
        let mut t = Timings::default();
        t.push("fmt", Duration::from_millis(1_000));
        t.push("clippy", Duration::from_millis(3_000));
        t.push("spdx", Duration::from_millis(0));
        // shares by hand: 3/4 = 75 %, 1/4 = 25 %, 0
        let want = "check times:\n  clippy     3.0s  75.0%\n  fmt        1.0s  25.0%\n  spdx       0.0s   0.0%\n  total      4.0s\n";
        assert_eq!(t.table("check"), want);
    }

    #[test]
    fn pool_waits_show_beside_their_step_and_are_summed() {
        let mut t = Timings::default();
        t.push_waited(
            "test",
            Duration::from_millis(3_000),
            Duration::from_millis(2_000),
        );
        t.push_waited("fmt", Duration::from_millis(1_000), Duration::ZERO);
        // the wait is not in the step's seconds: shares 3/4 and 1/4; one nonzero wait of 2 s; name column width 5 ("total")
        let want = "check times:\n  test      3.0s  75.0% wait 2.0s\n  fmt       1.0s  25.0%\n  total     4.0s\ncheck pool waits: 1, 2.0s\n";
        assert_eq!(t.table("check"), want);
    }

    #[test]
    fn an_empty_run_has_a_zero_total() {
        assert_eq!(
            Timings::default().table("land"),
            "land times:\n  total     0.0s\n"
        );
    }

    #[test]
    fn nested_phases_count_only_their_own_time() {
        let t0 = Instant::now();
        let mut c = Clock::new();
        c.enter("outer", t0);
        c.enter("inner", t0 + Duration::from_secs(1));
        c.exit(t0 + Duration::from_secs(3));
        c.add("span", Duration::from_millis(500));
        c.exit(t0 + Duration::from_secs(5));
        assert_eq!(c.rows["inner"].exclusive, Duration::from_secs(2));
        assert_eq!(c.rows["span"].exclusive, Duration::from_millis(500));
        // 5 s outer, less inner (2 s) and the span (0.5 s)
        assert_eq!(c.rows["outer"].exclusive, Duration::from_millis(2_500));
        assert_eq!(c.rows["outer"].inclusive, Duration::from_secs(5));
        assert!(c.rows["outer"].composite);
    }

    #[test]
    fn a_lock_wait_counts_in_its_open_phase_and_is_listed_last() {
        let t0 = Instant::now();
        let mut c = Clock::new();
        c.enter("drive: run-apk", t0);
        c.wait(
            "package cache",
            Duration::from_secs(4),
            Some("pid 9 cargo build".to_owned()),
        );
        c.exit(t0 + Duration::from_secs(10));
        let text = c.render("drive-tap");
        assert!(
            text.contains("drive: run-apk (x1, lock wait 4.0s)"),
            "{text}"
        );
        assert!(
            text.ends_with(
                "drive-tap lock wait: package cache: 4.0s x1, longest 4.0s, last announced holder pid 9 cargo build\n"
            ),
            "{text}"
        );
    }
}
