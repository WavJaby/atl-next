// SPDX-License-Identifier: MIT
//! Host CPU starvation over a time window, from the kernel's pressure stall information (`/proc/pressure/cpu`, PSI).
//!
//! The `some` line's `total` is the cumulative time (us) in which at least one runnable task waited for a CPU. A sampler thread reads it with a CLOCK_MONOTONIC stamp every [`SAMPLE_PERIOD`];
//! [`Samples::stall_share`] is the stall time over wall time between the samples bracketing a window. CLOCK_MONOTONIC is the clock Choreographer frame times are on, so a cadence window maps
//! onto the samples directly. A timing gate whose judgement fails while the share is over its threshold reports `undecidable` (the host, not the code, may have missed the deadline); an
//! undecidable verdict is still a failed gate, so `land` needs a rerun that decides.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustix::time::{ClockId, clock_gettime};

/// The PSI file. Absent on kernels without CONFIG_PSI: the sampler then records nothing and every window is unknown.
const PSI_CPU: &str = "/proc/pressure/cpu";
/// How often the sampler reads [`PSI_CPU`]: a tenth of the shortest judged window (31 callbacks at 60 Hz = 500 ms) keeps the bracketing samples within 20% of the window's span.
const SAMPLE_PERIOD: Duration = Duration::from_millis(50);

/// One sample: CLOCK_MONOTONIC ns, cumulative `some` stall us.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub at_ns: i64,
    pub stall_us: u64,
}

/// The `total=` of the `some` line of a PSI file.
fn some_total(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|w| w.strip_prefix("total="))?
        .parse()
        .ok()
}

fn now_ns() -> i64 {
    let t = clock_gettime(ClockId::Monotonic);
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

/// Samples in time order.
#[derive(Default, Debug)]
pub struct Samples(pub Vec<Sample>);

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
}

/// The running sampler; [`Sampler::stop`] ends it and returns what it read.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<Sample>>>,
    thread: JoinHandle<()>,
}

impl Sampler {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = Arc::clone(&stop);
            let samples = Arc::clone(&samples);
            thread::spawn(move || {
                loop {
                    let done = stop.load(Ordering::Relaxed);
                    if let Some(stall_us) = fs::read_to_string(PSI_CPU)
                        .ok()
                        .as_deref()
                        .and_then(some_total)
                    {
                        let at_ns = now_ns();
                        samples
                            .lock()
                            .expect("sampler lock")
                            .push(Sample { at_ns, stall_us });
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

    pub fn stop(self) -> Samples {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
        let v = std::mem::take(&mut *self.samples.lock().expect("sampler lock"));
        Samples(v)
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

    /// Samples at 0, 1 s, 2 s with 0, 0.1 s, 0.6 s stalled: a window inside the second second uses samples 1 and 2 -> 50%; one past the last sample is unknown.
    #[test]
    fn share_from_bracketing_samples() {
        let s = Samples(vec![
            Sample {
                at_ns: 0,
                stall_us: 0,
            },
            Sample {
                at_ns: 1_000_000_000,
                stall_us: 100_000,
            },
            Sample {
                at_ns: 2_000_000_000,
                stall_us: 600_000,
            },
        ]);
        let share = s
            .stall_share(1_200_000_000, 1_800_000_000)
            .expect("bracketed");
        assert!((share - 0.5).abs() < 1e-9, "{share}");
        assert!((s.stall_share(0, 2_000_000_000).expect("whole") - 0.3).abs() < 1e-9);
        assert_eq!(s.stall_share(500, 3_000_000_000), None);
    }

    /// The sampler reads the live file (the dev host has CONFIG_PSI): at least the final sample, stamps rising.
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
