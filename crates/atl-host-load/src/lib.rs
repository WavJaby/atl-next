// SPDX-License-Identifier: MIT
//! Host load over one test gate's run: how much CPU the gate's own processes waited for while other work held the CPUs.
//!
//! The gate runs in its own cgroup ([`scope`]), so the scope's `cpu.pressure` is the gate's own stall and its `cpu.stat` the gate's own CPU. `/proc/stat` gives the busy CPU of the gate's allowed set; the rest of it is foreign load.
//! [`Samples::stalled`] counts a stall interval only where foreign CPU covers the gate's own CPU: a runaway inside the gate makes its own CPU the larger part and cannot excuse its waits.
//! [`Gate::begin`] puts the test process into its scope when the host can and starts the sampler; [`Gate::finish`] returns the [`Report`] line. Without a scope the line says `unknown`, and nothing is measured.
//! The record judges nothing itself: the harness's starvation rule reads the running gate's samples ([`live`]) over a late wait's span.

#![forbid(unsafe_code)]

mod report;
mod sampler;
mod scope;

pub use report::{Load, Report, Seen};
pub use sampler::{
    GateReading, SAMPLE_PERIOD, Sample, Sampler, Samples, Span, allowed_cpus, live, monotonic_ns,
};
pub use scope::cgroup_dir;

use std::time::Duration;

/// How long a gate's ended processes may take to go: a SIGKILLed sandbox tree is torn down asynchronously, in milliseconds on a quiet host.
const LEFTOVER_SETTLE: Duration = Duration::from_secs(10);

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
                Ok(scope) => {
                    let sampler =
                        Sampler::start_for(Some(scope.dir.as_path()), allowed_cpus().as_deref());
                    sampler.publish();
                    Tier::Scoped { sampler, scope }
                }
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

    /// The processes the gate leaves running: everything in its scope but this process, whoever its parent is now. Called once the gate's work is over. Empty without a scope: not measured there.
    pub fn leftovers(&self) -> Result<Vec<String>, String> {
        match &self.tier {
            Tier::Scoped { scope, .. } => scope::leftovers(&scope.dir, LEFTOVER_SETTLE),
            Tier::Unscoped { .. } => Ok(Vec::new()),
        }
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
