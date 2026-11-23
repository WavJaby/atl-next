// SPDX-License-Identifier: MIT
//! Host load over one test gate's run: how much CPU the gate's own processes waited for while other work held the CPUs.
//!
//! The gate runs in its own cgroup ([`scope`]), so the scope's `cpu.pressure` is the gate's own stall and its `cpu.stat` the gate's own CPU. `/proc/stat` gives the busy CPU of the gate's allowed set; the rest of it is foreign load.
//! [`Samples::stalled`] counts a stall interval only where foreign CPU covers the gate's own CPU: a runaway inside the gate makes its own CPU the larger part and cannot excuse its waits.
//! [`Gate::begin`] puts the test process into its scope when the host can and starts the sampler; [`Gate::finish`] returns the [`Report`] line. Without a scope the line says `unknown`, and nothing is measured.
//! The record judges nothing. The system-wide `some` share ([`Samples::stall_share`]) still feeds [`judge_window`], unchanged, until its callers take the record.

#![forbid(unsafe_code)]

mod report;
mod sampler;
mod scope;

pub use report::{Load, Report, Seen};
pub use sampler::{GateReading, SAMPLE_PERIOD, Sample, Sampler, Samples, Span, allowed_cpus};
pub use scope::cgroup_dir;

/// Stall share over a judged window above which a failed judgement is `undecidable` rather than FAIL. 10%: over every share a quiet host showed (16 CPUs, load 7-15: 0-7% in 8 passing frames runs), under the 87-97% of a host oversubscribed twice (32 busy loops), where the app's first frames miss vsyncs.
pub const STARVED_STALL_SHARE: f64 = 0.10;

/// The judgement over a window: `Ok` keeps its message and appends the share; `Err` becomes `undecidable` when the share is over [`STARVED_STALL_SHARE`], and stays a failure either way. `over` names the window in that text (`the window`, `the run`).
pub fn judge_window(
    judgement: Result<String, String>,
    share: Option<f64>,
    over: &str,
) -> Result<String, String> {
    let stall = share.map_or_else(
        || "cpu stall unknown".to_owned(),
        |s| format!("cpu stall {:.0}%", s * 100.0),
    );
    match judgement {
        Ok(msg) => Ok(format!("{msg}; {stall}")),
        Err(e) if share.is_some_and(|s| s > STARVED_STALL_SHARE) => Err(format!(
            "undecidable: {e}, but the host was starved ({stall} over {over}, threshold {:.0}%); rerun on a quieter host",
            STARVED_STALL_SHARE * 100.0
        )),
        Err(e) => Err(format!("{e}; {stall}")),
    }
}

/// A test gate's record: [`Gate::begin`] first, [`Gate::finish`] last.
pub struct Gate {
    tier: Tier,
}

enum Tier {
    Scoped {
        scope: scope::Scope,
        sampler: Sampler,
    },
    Unscoped {
        why: String,
    },
}

impl Gate {
    /// Begins the gate. Its own unit is the only scope it measures: a process not yet in `atl-gate-<pid>.scope` relaunches into it (the call then returns there, as the same process; see `scope::relaunch_self`). An enclosing scope is the job's, not the gate's, so it is never used.
    pub fn begin() -> Self {
        let tier = if scope::relaunched() {
            match scope::own() {
                Ok(scope) => Tier::Scoped {
                    sampler: Sampler::start_for(
                        Some(scope.dir.as_path()),
                        allowed_cpus().as_deref(),
                    ),
                    scope,
                },
                Err(why) => Tier::Unscoped { why },
            }
        } else {
            match scope::relaunch_self() {
                Err(why) => Tier::Unscoped { why },
                Ok(never) => match never {},
            }
        };
        Self { tier }
    }

    /// Ends the gate and returns its record.
    pub fn finish(self) -> Report {
        match self.tier {
            Tier::Scoped { scope, sampler } => {
                let samples = sampler.stop();
                Report::Scoped {
                    unit: scope.unit().to_owned(),
                    peak_bytes: scope::peak_bytes(&scope.dir),
                    load: Load::measure(&samples),
                }
            }
            Tier::Unscoped { why } => Report::Unscoped { why },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pass keeps its message; only a failure strictly over the threshold becomes undecidable (the threshold itself is not starved); no share is `unknown`, still a plain failure.
    #[test]
    fn judge_window_undecidable_only_over_threshold() {
        assert_eq!(
            judge_window(Ok("ok".into()), Some(0.5), "the run"),
            Ok("ok; cpu stall 50%".into())
        );
        assert_eq!(
            judge_window(Err("e".into()), Some(0.10), "the run"),
            Err("e; cpu stall 10%".into())
        );
        assert_eq!(
            judge_window(Err("e".into()), None, "the run"),
            Err("e; cpu stall unknown".into())
        );
        assert_eq!(
            judge_window(Err("e".into()), Some(0.20), "the run"),
            Err("undecidable: e, but the host was starved (cpu stall 20% over the run, threshold 10%); rerun on a quieter host".into())
        );
    }
}
