// SPDX-License-Identifier: MIT
//! The `host load:` line a test gate prints last, and its reader: the row line takes its stall share and peak from here.
//! `host load: tier=scope unit=<unit> span=<s>s stalled=<ms>ms own=<ms>ms foreign=<ms>ms late_max=<ms>ms io=<%> mem=<%> peak=<MiB>MiB`; `... unknown (<why>)` in place of the measures when the samples cannot answer; `host load: tier=unscoped unknown (<why>)` without a scope.

use std::fmt;

use crate::sampler::{Samples, Span};

const TAG: &str = "host load:";
const MIB: u64 = 1024 * 1024;

/// One gate's record: the tier it ran in, and the load over its span.
pub enum Report {
    /// Ran in `unit`. `load` is `Err` (unknown, with why) when the samples cannot answer.
    Scoped {
        unit: String,
        peak_bytes: Option<u64>,
        load: Result<Load, String>,
    },
    /// No scope: nothing was measured.
    Unscoped { why: String },
}

/// The load over one span, from the sampler. Times in us (`late_max_ns` in ns); shares are percent of the span's wall time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Load {
    pub span_us: u64,
    pub stalled_us: u64,
    pub own_us: u64,
    pub foreign_us: u64,
    pub late_max_ns: u64,
    pub io_pct: Option<f64>,
    pub mem_pct: Option<f64>,
}

impl Load {
    /// The load over the whole run: the first sample to the last.
    pub fn measure(samples: &Samples) -> Result<Self, String> {
        let span: Span = samples.span().ok_or("fewer than two samples")?;
        let unknown = "samples do not cover the run, or an interval lacks a gate reading";
        Ok(Self {
            span_us: u64::try_from(span.to_ns - span.from_ns).unwrap_or(0) / 1_000,
            stalled_us: samples.stalled(span).ok_or(unknown)?,
            own_us: samples.own(span).ok_or(unknown)?,
            foreign_us: samples.foreign(span).ok_or(unknown)?,
            late_max_ns: samples.late_max_ns().unwrap_or(0),
            io_pct: samples.psi_share(span, |s| s.io_us).map(|x| x * 100.0),
            mem_pct: samples.psi_share(span, |s| s.mem_us).map(|x| x * 100.0),
        })
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unscoped { why } => write!(f, "{TAG} tier=unscoped unknown ({why})"),
            Self::Scoped {
                unit,
                peak_bytes,
                load,
            } => {
                let peak = peak_bytes.map_or("unknown".to_owned(), |b| format!("{}MiB", b / MIB));
                match load {
                    Ok(l) => write!(
                        f,
                        "{TAG} tier=scope unit={unit} span={:.3}s stalled={}ms own={}ms foreign={}ms late_max={:.2}ms io={} mem={} peak={peak}",
                        l.span_us as f64 / 1e6,
                        l.stalled_us / 1_000,
                        l.own_us / 1_000,
                        l.foreign_us / 1_000,
                        l.late_max_ns as f64 / 1e6,
                        percent(l.io_pct),
                        percent(l.mem_pct),
                    ),
                    Err(why) => write!(
                        f,
                        "{TAG} tier=scope unit={unit} unknown ({why}) peak={peak}"
                    ),
                }
            }
        }
    }
}

fn percent(p: Option<f64>) -> String {
    p.map_or("unknown".to_owned(), |p| format!("{p:.1}%"))
}

/// What the row line reads back from a gate's log: its stall share over the span and its peak.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Seen {
    /// stalled / span, percent; None when the record is unknown or absent.
    pub stall_pct: Option<f64>,
    /// the scope's `memory.peak`, MiB; None when unknown.
    pub peak_mib: Option<u64>,
}

impl Seen {
    /// The last `host load:` line of a gate's log. No line reads as unknown for both fields.
    pub fn read(log: &str) -> Self {
        let Some(line) = log.lines().rev().find(|l| l.starts_with(TAG)) else {
            return Self::default();
        };
        let field = |key: &str| line.split_whitespace().find_map(|w| w.strip_prefix(key));
        let span_s = field("span=").and_then(|v| v.strip_suffix('s')?.parse::<f64>().ok());
        let stalled_ms = field("stalled=").and_then(|v| v.strip_suffix("ms")?.parse::<f64>().ok());
        let stall_pct = match (span_s, stalled_ms) {
            (Some(s), Some(ms)) if s > 0.0 => Some(ms / (s * 1_000.0) * 100.0),
            _ => None,
        };
        Self {
            stall_pct,
            peak_mib: field("peak=").and_then(|v| v.strip_suffix("MiB")?.parse().ok()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::{Sample, gate_at};

    /// The line as written, field by field: 4 s span, 1 s stalled, 1 GiB peak.
    #[test]
    fn record_line_as_written() {
        let report = Report::Scoped {
            unit: "atl-gate-7.scope".to_owned(),
            peak_bytes: Some(1024 * 1024 * 1024),
            load: Ok(Load {
                span_us: 4_000_000,
                stalled_us: 1_000_000,
                own_us: 2_000_000,
                foreign_us: 3_000_000,
                late_max_ns: 280_000,
                io_pct: Some(0.0),
                mem_pct: None,
            }),
        };
        assert_eq!(
            report.to_string(),
            "host load: tier=scope unit=atl-gate-7.scope span=4.000s stalled=1000ms own=2000ms foreign=3000ms late_max=0.28ms io=0.0% mem=unknown peak=1024MiB"
        );
    }

    /// 1000 ms stalled over 4.000 s is 25%; the unit name's digits are not a field.
    #[test]
    fn row_reads_stall_share_and_peak_back() {
        let log = "gates: other row\nhost load: tier=scope unit=atl-gate-7.scope span=4.000s stalled=1000ms own=2000ms foreign=3000ms late_max=0.28ms io=0.0% mem=unknown peak=1024MiB\n";
        assert_eq!(
            Seen::read(log),
            Seen {
                stall_pct: Some(25.0),
                peak_mib: Some(1024)
            }
        );
    }

    #[test]
    fn unscoped_and_unknown_read_as_unknown() {
        let unscoped = Report::Unscoped {
            why: "no user manager".to_owned(),
        }
        .to_string();
        assert_eq!(Seen::read(&unscoped), Seen::default());
        let unknown =
            "host load: tier=scope unit=u.scope unknown (fewer than two samples) peak=512MiB\n";
        assert_eq!(
            Seen::read(unknown),
            Seen {
                stall_pct: None,
                peak_mib: Some(512)
            }
        );
        assert_eq!(Seen::read("no record here\n"), Seen::default());
    }

    /// The same three readings as the sampler's counting test: stalled 100 ms, own 400 ms, foreign 400 ms over 2 s.
    #[test]
    fn measure_reads_the_whole_run() {
        let samples = Samples(vec![
            gate_at(0, 0, 0, 0),
            gate_at(1_000_000_000, 100_000, 300_000, 700_000),
            gate_at(2_000_000_000, 400_000, 400_000, 800_000),
        ]);
        let load = Load::measure(&samples).expect("measured");
        assert_eq!(load.span_us, 2_000_000);
        assert_eq!(load.stalled_us, 100_000);
        assert_eq!(load.own_us, 400_000);
        assert_eq!(load.foreign_us, 400_000);
        assert_eq!(load.io_pct, None);
        assert!(Load::measure(&Samples(vec![Sample::default()])).is_err());
    }
}
