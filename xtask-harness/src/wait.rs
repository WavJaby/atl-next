// SPDX-License-Identifier: MIT
//! Declared waits (gate-starved-red-design section 2.2, 2.6): one function runs a wait, bounds it and prints its record.
//!
//! H2a/H2b judge nothing: `bound` is the limit each call site already had, and `need` stays unset until quiet passes measure it.
//! `until()` returns `Miss::Late` when the bound passes and the caller decides what that means: a probe `?`s it (its first late wait ends the probe), an absence wait takes it as the answer.
//! `until_child()` also ends a wait when the child it watches exits (`Miss::Exited`: a death, never a late wait).

use std::path::Path;
use std::process::{Child, ExitStatus};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crate::wayland_proxy::Event as ProxyEvent;

/// How often a log is read while a wait polls it (the app's log is written by another process).
pub(crate) const POLL: Duration = Duration::from_millis(50);
/// How often an exit-only wait polls its child (`try_wait` is cheap; the 10 ms that binder's `wait_limited` polled before, so `took=` keeps its precision).
const EXIT_POLL: Duration = Duration::from_millis(10);
/// The poll of a converted loop wait: the sleep of the loop it replaced on main (broadcast's run loops, app's marker loop, the connectivity steps: 200 ms). H2 moves no poll interval.
pub(crate) const LOOP_POLL: Duration = Duration::from_millis(200);
/// The poll of the app's post-marker watch: its sleep on main (app.rs, 100 ms).
pub(crate) const WATCH_POLL: Duration = Duration::from_millis(100);

/// What a wait ends on: a log's text (polled), a proxy event (received; its arrival time is the answer), the child's exit (`until_child` only: the exit is the answer),
/// or a condition the caller reads from any number of logs (`Logs`, polled like a log). `Log` and `Logs` run through `until_child` also end on that child's exit (`Miss::Exited`).
pub(crate) enum Until<'a> {
    Log {
        file: &'a Path,
        want: &'a dyn Fn(&str) -> bool,
    },
    Event {
        events: &'a Receiver<(Instant, ProxyEvent)>,
        want: &'a dyn Fn(&ProxyEvent) -> bool,
    },
    Exit,
    /// `held` is the whole judgement: the caller reads each log it needs. Multi-log fences (broadcast's run end, app's marker or app end) are this kind.
    /// `every` is the poll interval: the sleep of the loop the wait replaced.
    Logs {
        held: &'a dyn Fn() -> bool,
        every: Duration,
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

    /// The child's exit within `bound`, for `until_child`: the exit is the answer, its status the caller reads.
    pub(crate) fn exit(name: &'static str, why: impl Into<String>, bound: Duration) -> Self {
        Self {
            name,
            why: why.into(),
            bound,
            need: None,
            until: Until::Exit,
        }
    }

    /// A condition `held` reads from any number of logs, polled every `every` for `bound`. Holds only while `held` does: a conjunction of logs is one `held`.
    pub(crate) fn logs(
        name: &'static str,
        why: impl Into<String>,
        bound: Duration,
        every: Duration,
        held: &'a dyn Fn() -> bool,
    ) -> Self {
        Self {
            name,
            why: why.into(),
            bound,
            need: None,
            until: Until::Logs { held, every },
        }
    }
}

/// Why a check did not pass. `Wrong`: its own judgement, or a probe's proxy/spawn error (never a wait). `Late`: a declared wait ran out its bound. `NotReached`: an earlier late wait ended the probe, so this check did not run.
/// `Exited`: the child a wait watched ended before the wait's condition held. A death, not a late wait: section 2.3 never excuses it, so it is never `Late`.
/// Owned strings: the probe's miss travels in `live.json` (LiveRecord) to the judge of a remote run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Miss {
    Wrong(String),
    Late { name: String, why: String },
    Exited { name: String, status: String },
    NotReached { after: String },
}

impl Miss {
    /// The reason printed for a check that failed with this.
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Wrong(s) | Self::Late { why: s, .. } => s.clone(),
            Self::Exited { name, status } => format!("{name}: the process exited {status}"),
            Self::NotReached { after } => format!("not reached (after {after})"),
        }
    }

    /// The probe's miss named for a `not reached` line: its late wait, or `probe` for a probe error.
    fn after(&self) -> String {
        match self {
            Self::Late { name, .. }
            | Self::Exited { name, .. }
            | Self::NotReached { after: name } => name.clone(),
            Self::Wrong(_) => "probe".to_owned(),
        }
    }
}

impl From<String> for Miss {
    fn from(s: String) -> Self {
        Self::Wrong(s)
    }
}

/// Runs `wait` until its condition holds (Ok: the instant it did) or its bound passes (Late). Prints the record once, on either outcome.
pub(crate) fn until(wait: &Wait) -> Result<Instant, Miss> {
    run(wait, None)
}

/// [`until`], also ended by `child`'s exit (`Miss::Exited`). An [`Until::Exit`] wait is that exit (Ok). An event wait does not watch its child.
pub(crate) fn until_child(wait: &Wait, child: &mut Child) -> Result<Instant, Miss> {
    run(wait, Some(child))
}

/// How a wait ended: its condition's instant, the watched child's exit, or the bound.
enum Ended {
    Held(Instant),
    Exited(Instant, ExitStatus),
    Late,
}

fn run(wait: &Wait, child: Option<&mut Child>) -> Result<Instant, Miss> {
    let start = Instant::now();
    let deadline = start + wait.bound;
    let ended = match (&wait.until, child) {
        (Until::Event { events, want }, _) => Ok(received(events, want, deadline)),
        (Until::Log { file, want }, child) => polled(
            deadline,
            child,
            &|| want(&atl_diag::logfiles::read(file)),
            false,
            POLL,
        ),
        (Until::Logs { held, every }, child) => polled(deadline, child, *held, false, *every),
        (Until::Exit, Some(child)) => polled(deadline, Some(child), &|| false, true, EXIT_POLL),
        (Until::Exit, None) => {
            return Err(Miss::Wrong(format!(
                "{}: an exit wait needs its child (until_child)",
                wait.name
            )));
        }
    }
    .map_err(|e| Miss::Wrong(format!("{}: {e}", wait.name)))?;
    let at = match &ended {
        Ended::Held(at) | Ended::Exited(at, _) => *at,
        Ended::Late => Instant::now(),
    };
    record(
        wait.name,
        at.saturating_duration_since(start),
        wait.bound,
        wait.need,
    );
    match ended {
        Ended::Held(at) => Ok(at),
        Ended::Exited(_, status) => Err(Miss::Exited {
            name: wait.name.to_owned(),
            status: status.to_string(),
        }),
        Ended::Late => Err(Miss::Late {
            name: wait.name.to_owned(),
            why: wait.why.clone(),
        }),
    }
}

/// Receives events until `want` accepts one (Held at its arrival) or `deadline` passes. Other events are skipped.
fn received(
    events: &Receiver<(Instant, ProxyEvent)>,
    want: &dyn Fn(&ProxyEvent) -> bool,
    deadline: Instant,
) -> Ended {
    loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            return Ended::Late;
        };
        match events.recv_timeout(left) {
            Ok((at, event)) if want(&event) => return Ended::Held(at),
            Ok(_) => {}
            Err(_) => return Ended::Late,
        }
    }
}

/// Polls every `interval` until `held` holds, then the child (if any) exits (`exit_holds`: that exit is the answer; else Exited), else `deadline` passes (Late). Each poll checks in that order.
fn polled(
    deadline: Instant,
    mut child: Option<&mut Child>,
    held: &dyn Fn() -> bool,
    exit_holds: bool,
    interval: Duration,
) -> std::io::Result<Ended> {
    loop {
        if held() {
            return Ok(Ended::Held(Instant::now()));
        }
        let exited = match child.as_mut() {
            Some(c) => c.try_wait()?,
            None => None,
        };
        if let Some(status) = exited {
            let now = Instant::now();
            return Ok(if exit_holds {
                Ended::Held(now)
            } else {
                Ended::Exited(now, status)
            });
        }
        if Instant::now() >= deadline {
            return Ok(Ended::Late);
        }
        std::thread::sleep(interval);
    }
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

/// A row's bool checks, each printed `<row>: pass|FAIL <what>`. Returns the failing `what`s: the verdict's list.
pub(crate) fn judge_checks(row: &str, checks: &[(&str, bool)]) -> Vec<String> {
    let mut fails = Vec::new();
    for (what, ok) in checks {
        println!("{row}: {} {what}", if *ok { "pass" } else { "FAIL" });
        if !*ok {
            fails.push((*what).to_owned());
        }
    }
    fails
}

/// A run-end wait `late` ran out before the checks could read the run's end: one FAIL (its why, the verdict's only failure), then every check `not reached`, judging nothing.
pub(crate) fn stopped_at(row: &str, checks: &[(&str, bool)], late: &Miss) -> Vec<String> {
    let not_reached = Miss::NotReached {
        after: late.after(),
    }
    .text();
    println!("{row}: FAIL {}", late.text());
    for (what, _) in checks {
        println!("{row}: {not_reached} {what}");
    }
    vec![late.text()]
}

/// A timed settle with no condition: `bound` passes (Ok, its record printed) or `child` exits first (`Miss::Exited`, the caller's death).
/// Not a fence: it stays a sleep until the thing it lets land has a fence of its own (broadcast's REINSTALL_SETTLE).
pub(crate) fn settle_for(
    name: &'static str,
    bound: Duration,
    child: &mut Child,
) -> Result<(), Miss> {
    let never = || false;
    let w = Wait::logs(name, "", bound, LOOP_POLL, &never);
    match until_child(&w, child) {
        Err(Miss::Late { .. }) => Ok(()),
        Err(miss) => Err(miss),
        Ok(_) => Ok(()),
    }
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

    fn sh(script: &str) -> Child {
        std::process::Command::new("sh")
            .args(["-c", script])
            .spawn()
            .expect("sh")
    }

    #[test]
    fn exit_wait_is_the_childs_exit_or_late_on_the_bound() {
        let mut quick = sh("exit 3");
        let w = Wait::exit("quick", "never exited", Duration::from_secs(5));
        assert!(until_child(&w, &mut quick).is_ok());
        assert_eq!(quick.wait().expect("status").code(), Some(3));
        let mut slow = sh("sleep 30");
        let w = Wait::exit("slow", "still running", Duration::from_millis(100));
        assert_eq!(
            until_child(&w, &mut slow).unwrap_err(),
            late("slow", "still running")
        );
        slow.kill().expect("kill");
        slow.wait().expect("reap");
    }

    #[test]
    fn exit_wait_without_its_child_is_a_wrong_miss_not_a_late_one() {
        let w = Wait::exit("orphan", "no child", Duration::from_millis(10));
        assert!(matches!(until(&w), Err(Miss::Wrong(_))));
    }

    #[test]
    fn log_wait_with_a_child_ends_early_when_the_child_exits() {
        let dir = std::env::temp_dir().join(format!("wait-child-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("app.log");
        std::fs::write(&file, "starting\n").unwrap();
        let want = |t: &str| t.contains("finished");
        let w = Wait::log(
            "done",
            "never finished",
            Duration::from_secs(30),
            &file,
            &want,
        );
        let mut dead = sh("exit 4");
        let err = until_child(&w, &mut dead).unwrap_err();
        assert_eq!(
            err,
            Miss::Exited {
                name: "done".to_owned(),
                status: "exit status: 4".to_owned(),
            }
        );
        // the text arrives while the child still runs: the wait ends on the text, not on the child
        let mut alive = sh("sleep 30");
        std::fs::write(&file, "starting\nfinished\n").unwrap();
        assert!(until_child(&w, &mut alive).is_ok());
        alive.kill().expect("kill");
        alive.wait().expect("reap");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn judge_checks_lists_the_failing_checks_in_order() {
        let checks = [("a", true), ("b", false), ("c", false)];
        assert_eq!(
            judge_checks("row", &checks),
            vec!["b".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn a_late_run_end_wait_is_one_fail_and_every_check_not_reached() {
        let miss = late("provider-done", "no done line within 180s");
        let checks = [("first", true), ("second", false)];
        assert_eq!(
            stopped_at("row", &checks, &miss),
            vec!["no done line within 180s".to_owned()]
        );
    }

    #[test]
    fn logs_wait_holds_only_when_every_log_it_reads_holds() {
        let dir = std::env::temp_dir().join(format!("wait-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (sender, receiver) = (dir.join("sender.log"), dir.join("receiver.log"));
        std::fs::write(&sender, "AtlBcast: done received\n").unwrap();
        std::fs::write(&receiver, "").unwrap();
        let held = || {
            atl_diag::logfiles::read(&sender).contains("done received")
                && atl_diag::logfiles::read(&receiver).contains("alarm started")
        };
        // the first log holds and the second does not: late, and it does not count the first as enough
        let w = Wait::logs(
            "run-end",
            "no alarm line",
            Duration::from_millis(150),
            LOOP_POLL,
            &held,
        );
        assert_eq!(until(&w).unwrap_err(), late("run-end", "no alarm line"));
        std::fs::write(&receiver, "alarm started\n").unwrap();
        assert!(until(&w).is_ok());
        // the same condition with the child's exit: a death, not a late wait (bound long enough for the exit to land)
        std::fs::write(&receiver, "").unwrap();
        let w = Wait::logs(
            "run-end",
            "no alarm line",
            Duration::from_secs(5),
            LOOP_POLL,
            &held,
        );
        let mut dead = sh("exit 5");
        assert_eq!(
            until_child(&w, &mut dead).unwrap_err(),
            Miss::Exited {
                name: "run-end".to_owned(),
                status: "exit status: 5".to_owned(),
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settle_for_is_settled_on_its_bound_and_a_death_when_its_child_exits() {
        let mut slow = sh("sleep 30");
        assert!(settle_for("settle", Duration::from_millis(60), &mut slow).is_ok());
        slow.kill().expect("kill");
        slow.wait().expect("reap");
        let mut quick = sh("exit 2");
        assert_eq!(
            settle_for("settle", Duration::from_secs(5), &mut quick).unwrap_err(),
            Miss::Exited {
                name: "settle".to_owned(),
                status: "exit status: 2".to_owned(),
            }
        );
    }
}
