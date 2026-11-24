// SPDX-License-Identifier: MIT
//! Declared waits (gate-starved-red-design section 2.2, 2.6): one function runs a wait, bounds it and prints its record.
//!
//! H2a judges nothing: `bound` is the limit each call site already had, and `need` stays unset until quiet passes measure it.
//! `until()` returns `Miss::Late` when the bound passes and the caller decides what that means: a probe `?`s it (its first late wait ends the probe), an absence wait takes it as the answer.

use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::wayland_proxy::Event as ProxyEvent;

/// How often a log is read while a wait polls it (the app's log is written by another process).
pub(crate) const POLL: Duration = Duration::from_millis(50);

/// What a wait ends on: a log's text (polled), or a proxy event (received; its arrival time is the answer).
pub(crate) enum Until<'a> {
    Log {
        file: &'a Path,
        want: &'a dyn Fn(&str) -> bool,
    },
    Event {
        events: &'a Receiver<(Instant, ProxyEvent)>,
        want: &'a dyn Fn(&ProxyEvent) -> bool,
    },
}

/// A declared wait. `name` is its record; `why` is what the check needed, printed when the bound passes; `need` is the quiet-host time (None: not measured yet).
pub(crate) struct Wait<'a> {
    pub(crate) name: &'static str,
    pub(crate) why: String,
    pub(crate) bound: Duration,
    pub(crate) need: Option<Duration>,
    pub(crate) until: Until<'a>,
}

impl<'a> Wait<'a> {
    /// A text `want` accepts in the log `file`, polled for `bound`.
    pub(crate) fn log(
        name: &'static str,
        why: impl Into<String>,
        bound: Duration,
        file: &'a Path,
        want: &'a dyn Fn(&str) -> bool,
    ) -> Self {
        Self {
            name,
            why: why.into(),
            bound,
            need: None,
            until: Until::Log { file, want },
        }
    }

    /// A proxy event `want` accepts, received for `bound`.
    pub(crate) fn event(
        name: &'static str,
        why: impl Into<String>,
        bound: Duration,
        events: &'a Receiver<(Instant, ProxyEvent)>,
        want: &'a dyn Fn(&ProxyEvent) -> bool,
    ) -> Self {
        Self {
            name,
            why: why.into(),
            bound,
            need: None,
            until: Until::Event { events, want },
        }
    }
}

/// Why a check did not pass. `Wrong`: its own judgement, or a probe's proxy/spawn error (never a wait). `Late`: a declared wait ran out its bound. `NotReached`: an earlier late wait ended the probe, so this check did not run.
/// Owned strings: the probe's miss travels in `live.json` (LiveRecord) to the judge of a remote run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Miss {
    Wrong(String),
    Late { name: String, why: String },
    NotReached { after: String },
}

impl Miss {
    /// The reason printed for a check that failed with this.
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Wrong(s) | Self::Late { why: s, .. } => s.clone(),
            Self::NotReached { after } => format!("not reached (after {after})"),
        }
    }

    /// The probe's miss named for a `not reached` line: its late wait, or `probe` for a probe error.
    fn after(&self) -> String {
        match self {
            Self::Late { name, .. } | Self::NotReached { after: name } => name.clone(),
            Self::Wrong(_) => "probe".to_owned(),
        }
    }
}

impl From<String> for Miss {
    fn from(s: String) -> Self {
        Self::Wrong(s)
    }
}

/// Runs `wait` until its condition holds (Ok: a log text seen, or an event's arrival time) or its bound passes (Late). Prints the record once, on either outcome.
pub(crate) fn until(wait: &Wait) -> Result<Instant, Miss> {
    let start = Instant::now();
    let deadline = start + wait.bound;
    let held = match &wait.until {
        Until::Log { file, want } => {
            let mut seen = None;
            while Instant::now() < deadline {
                if want(&atl_diag::logfiles::read(file)) {
                    seen = Some(Instant::now());
                    break;
                }
                std::thread::sleep(POLL);
            }
            seen
        }
        Until::Event { events, want } => loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break None;
            };
            match events.recv_timeout(left) {
                Ok((at, event)) if want(&event) => break Some(at),
                Ok(_) => {}
                Err(_) => break None,
            }
        },
    };
    let took = held
        .unwrap_or_else(Instant::now)
        .saturating_duration_since(start);
    record(wait.name, took, wait.bound, wait.need);
    held.ok_or_else(|| Miss::Late {
        name: wait.name.to_owned(),
        why: wait.why.clone(),
    })
}

/// The record line of a declared wait: `wait: <name> took=<ms> bound=<ms> need=<ms|unset>`.
pub(crate) fn record(name: &str, took: Duration, bound: Duration, need: Option<Duration>) {
    println!("{}", line(name, took, bound, need));
}

fn line(name: &str, took: Duration, bound: Duration, need: Option<Duration>) -> String {
    let need = need.map_or_else(|| "unset".to_owned(), |n| n.as_millis().to_string());
    format!(
        "wait: {name} took={} bound={} need={need}",
        took.as_millis(),
        bound.as_millis()
    )
}

/// One probe's checks, judged once: a check whose text is the probe's miss needed the probe and propagates its text. The first such check keeps the miss (its FAIL); each later one is `NotReached`. Checks that did not need the probe keep their own result.
pub(crate) fn settle<N>(
    checks: Vec<(N, Result<String, String>)>,
    probe: Option<&Miss>,
) -> Vec<(N, Result<String, Miss>)> {
    let mut stopped = false;
    checks
        .into_iter()
        .map(|(name, result)| {
            let result = match (result, probe) {
                (Ok(s), _) => Ok(s),
                (Err(s), Some(miss)) if s == miss.text() => {
                    if std::mem::replace(&mut stopped, true) {
                        Err(Miss::NotReached {
                            after: miss.after(),
                        })
                    } else {
                        Err(miss.clone())
                    }
                }
                (Err(s), _) => Err(Miss::Wrong(s)),
            };
            (name, result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland_proxy::Event as ProxyEvent;
    use std::sync::mpsc::channel;

    fn late(name: &str, why: &str) -> Miss {
        Miss::Late {
            name: name.to_owned(),
            why: why.to_owned(),
        }
    }

    #[test]
    fn record_line_prints_unset_need_until_measured() {
        let ms = Duration::from_millis;
        assert_eq!(
            line("draw", ms(12), ms(5_000), None),
            "wait: draw took=12 bound=5000 need=unset"
        );
        assert_eq!(
            line("draw", ms(3_100), ms(5_000), Some(ms(100))),
            "wait: draw took=3100 bound=5000 need=100"
        );
    }

    #[test]
    fn late_event_wait_is_a_typed_miss_with_its_reason() {
        let (_tx, rx) = channel::<(Instant, ProxyEvent)>();
        let want = |e: &ProxyEvent| *e == ProxyEvent::Pong { serial: 1 };
        let w = Wait::event("pong", "no pong", Duration::from_millis(20), &rx, &want);
        assert_eq!(until(&w).unwrap_err(), late("pong", "no pong"));
    }

    #[test]
    fn event_wait_skips_other_events_and_ends_on_its_own() {
        let (tx, rx) = channel::<(Instant, ProxyEvent)>();
        let now = Instant::now();
        tx.send((now, ProxyEvent::Pong { serial: 7 })).unwrap();
        tx.send((now, ProxyEvent::Pong { serial: 1 })).unwrap();
        let want = |e: &ProxyEvent| *e == ProxyEvent::Pong { serial: 1 };
        let w = Wait::event("pong", "no pong", Duration::from_secs(5), &rx, &want);
        assert!(until(&w).is_ok());
    }

    #[test]
    fn log_wait_sees_a_line_written_by_another_writer() {
        let dir = std::env::temp_dir().join(format!("wait-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app.log");
        std::fs::write(&file, "AtlWindow: onCreate reached\n").unwrap();
        let want = |t: &str| t.contains("onDraw reached");
        let w = Wait::log("draw", "never drew", Duration::from_secs(1), &file, &want);
        assert!(until(&w).is_err());
        std::fs::write(&file, "AtlWindow: onCreate reached\nonDraw reached\n").unwrap();
        assert!(until(&w).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_stopped_at_its_first_late_wait_prints_one_fail_and_the_rest_not_reached() {
        // the 11-23 ime red's shape: the first wait ran out, every later check reads the probe's answer
        let probe = late("ime-commit", "no commit line within 10 s");
        let checks = vec![
            ("commit", Err("no commit line within 10 s".to_owned())),
            ("edit", Err("no commit line within 10 s".to_owned())),
            ("layout", Ok("pass".to_owned())),
            ("copy", Err("no commit line within 10 s".to_owned())),
            ("delete", Err("no commit line within 10 s".to_owned())),
        ];
        let out = settle(checks, Some(&probe));
        let shape: Vec<String> = out
            .iter()
            .map(|(name, r)| match r {
                Ok(_) => format!("{name}: pass"),
                Err(Miss::NotReached { after }) => format!("{name}: not reached after {after}"),
                Err(m) => format!("{name}: FAIL {}", m.text()),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "commit: FAIL no commit line within 10 s",
                "edit: not reached after ime-commit",
                "layout: pass",
                "copy: not reached after ime-commit",
                "delete: not reached after ime-commit",
            ]
        );
    }

    #[test]
    fn a_check_that_did_not_need_the_probe_keeps_its_own_judgement() {
        let probe = late("draw", "the fixture never drew");
        let checks = vec![
            ("picture", Err("wrong pixel at (0,0)".to_owned())),
            ("pong", Err("the fixture never drew".to_owned())),
        ];
        let out = settle(checks, Some(&probe));
        assert_eq!(
            out[0].1,
            Err(Miss::Wrong("wrong pixel at (0,0)".to_owned()))
        );
        assert_eq!(out[1].1, Err(late("draw", "the fixture never drew")));
    }
}
