// SPDX-License-Identifier: MIT
//! The result side of `xtask test cts`: one `am instrument` session -> a module's state and per-test outcomes, the run's report (`report.json`), its one-screen summary and the comparison with a saved report.
//! Pure functions over the parsed INSTRUMENTATION_* text (instrument.rs `Session`), so every rule is unit-tested without a runtime.
//!
//! A module's state, best first: `RAN` (the runner finished and reported as many tests as it announced with `numtests`), `INCOMPLETE` (it finished or was stopped at the time limit with fewer, or announced none),
//! `CRASH` (tests started, then the run ended without the final OK code: the instrumented process died, or `am` did), `START_FAIL` (no test started: the runner was not found, died in setup or matched nothing), `BUILD_FAIL` (the APK did not build).
//! Tests are counted from the status blocks as Tradefed's InstrumentationResultParser sees them: 0 pass, -2 fail, -1 error, -3 ignored and -4 assumption failure (both `ignored`); a test whose start block has no end block is `incomplete`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atld_run::Counter;
use crate::instrument::{
    RESULT_OK, STATUS_ASSUMPTION_FAILURE, STATUS_ERROR, STATUS_FAILURE, STATUS_IGNORED, STATUS_OK,
    STATUS_START, Session,
};

/// How a module's run ended; the declaration order is best to worst (the baseline comparison's order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum State {
    Ran,
    Incomplete,
    Crash,
    StartFail,
    BuildFail,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Ran => "RAN",
            State::Incomplete => "INCOMPLETE",
            State::Crash => "CRASH",
            State::StartFail => "START_FAIL",
            State::BuildFail => "BUILD_FAIL",
        }
    }
}

/// One test's result; the declaration order is best to worst, so merging two results of one name keeps the worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Ignored,
    Incomplete,
    Fail,
    Error,
}

/// One module's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Module {
    pub module: String,
    pub state: State,
    /// why the state is not `RAN` (empty when it is)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    pub pass: u32,
    pub fail: u32,
    pub error: u32,
    pub ignored: u32,
    /// the tests the runner announced (`numtests`); the tests seen when it announced none
    pub total: u32,
    /// every test seen -> its outcome (the worse one when a name repeats)
    pub tests: BTreeMap<String, Outcome>,
}

impl Module {
    /// A module that never ran: no tests, `state` and why.
    pub fn without_run(module: &str, state: State, detail: String) -> Module {
        Module {
            module: module.to_owned(),
            state,
            detail,
            pass: 0,
            fail: 0,
            error: 0,
            ignored: 0,
            total: 0,
            tests: BTreeMap::new(),
        }
    }
}

/// The whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// layout version of this file
    pub schema: u32,
    pub host: String,
    pub modules: Vec<Module>,
    /// the services' counters summed over the modules, most hits first
    pub counters: Vec<Counter>,
}

pub const SCHEMA: u32 = 1;

/// What the status blocks say of the tests: the outcomes by `<class>#<test>`, how many end blocks came (each counted by its kind) and the largest `numtests`.
struct Tally {
    tests: BTreeMap<String, Outcome>,
    pass: u32,
    fail: u32,
    error: u32,
    ignored: u32,
    started: u32,
    announced: Option<u32>,
}

fn tally(s: &Session) -> Tally {
    let mut t = Tally {
        tests: BTreeMap::new(),
        pass: 0,
        fail: 0,
        error: 0,
        ignored: 0,
        started: 0,
        announced: None,
    };
    let mut pending = std::collections::BTreeSet::new();
    for status in &s.statuses {
        if let Some(n) = status
            .values
            .get("numtests")
            .and_then(|n| n.trim().parse().ok())
        {
            t.announced = t.announced.max(Some(n));
        }
        let Some(test) = status.values.get("test") else {
            continue;
        };
        let id = match status.values.get("class") {
            Some(class) => format!("{class}#{test}"),
            None => test.clone(),
        };
        let outcome = match status.code {
            STATUS_START => {
                t.started += 1;
                pending.insert(id);
                continue;
            }
            STATUS_OK => {
                t.pass += 1;
                Outcome::Pass
            }
            STATUS_FAILURE => {
                t.fail += 1;
                Outcome::Fail
            }
            STATUS_ERROR => {
                t.error += 1;
                Outcome::Error
            }
            STATUS_IGNORED | STATUS_ASSUMPTION_FAILURE => {
                t.ignored += 1;
                Outcome::Ignored
            }
            // IN_PROGRESS (2) and codes no runner sends
            _ => continue,
        };
        pending.remove(&id);
        let slot = t.tests.entry(id).or_insert(outcome);
        *slot = (*slot).max(outcome);
    }
    for id in pending {
        t.tests.entry(id).or_insert(Outcome::Incomplete);
    }
    t
}

/// The module's state and counts from a finished (or time-limited) session; `timed_out` = the run was stopped at the limit (`limit_secs` only names it in the detail).
pub fn classify(module: &str, s: &Session, timed_out: bool, limit_secs: u64) -> Module {
    let t = tally(s);
    let ran = t.pass + t.fail + t.error + t.ignored;
    let total = t.announced.unwrap_or(t.tests.len() as u32);
    let message = || {
        ["shortMsg", "longMsg"]
            .iter()
            .filter_map(|k| s.result.get(*k))
            .map(|m| m.lines().next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(": ")
    };
    let (state, detail) = if let Some(f) = &s.failed {
        (State::StartFail, format!("INSTRUMENTATION_FAILED: {f}"))
    } else if s.code == Some(RESULT_OK) && !timed_out {
        match t.announced {
            _ if ran == 0 => (
                State::StartFail,
                "the runner finished without a test (filter matched nothing?)".to_owned(),
            ),
            Some(n) if ran < n => (
                State::Incomplete,
                format!("{ran} of {n} announced tests ended"),
            ),
            None => (
                State::Incomplete,
                "the runner announced no `numtests`".to_owned(),
            ),
            Some(_) => (State::Ran, String::new()),
        }
    } else if t.started == 0 && ran == 0 {
        let why = if timed_out {
            format!("no test started within {limit_secs} s")
        } else {
            match (s.code, message()) {
                (_, m) if !m.is_empty() => m,
                (Some(c), _) => format!("INSTRUMENTATION_CODE {c}, no test started"),
                (None, _) => "no INSTRUMENTATION_CODE, no test started".to_owned(),
            }
        };
        (State::StartFail, why)
    } else if timed_out {
        (
            State::Incomplete,
            format!("stopped after {limit_secs} s with {ran} of {total} tests ended"),
        )
    } else {
        let why = match (s.code, message()) {
            (_, m) if !m.is_empty() => m,
            (Some(c), _) => format!("INSTRUMENTATION_CODE {c}"),
            (None, _) => "no INSTRUMENTATION_CODE".to_owned(),
        };
        (State::Crash, format!("{why} after {ran} of {total} tests"))
    };
    Module {
        module: module.to_owned(),
        state,
        detail,
        pass: t.pass,
        fail: t.fail,
        error: t.error,
        ignored: t.ignored,
        total,
        tests: t.tests,
    }
}

/// `counters` of every module summed by (kind, name), most hits first, then by name.
pub fn sum_counters(per_module: &[Vec<Counter>]) -> Vec<Counter> {
    let mut sum: BTreeMap<(String, String), u64> = BTreeMap::new();
    for c in per_module.iter().flatten() {
        *sum.entry((c.kind.clone(), c.name.clone())).or_default() += c.count;
    }
    let mut out: Vec<Counter> = sum
        .into_iter()
        .map(|((kind, name), count)| Counter { kind, name, count })
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.count));
    out
}

/// The one-screen summary: a table row per module, the totals, then the `top` counters.
pub fn summary(report: &Report, top: usize) -> Vec<String> {
    let width = report
        .modules
        .iter()
        .map(|m| m.module.len())
        .max()
        .unwrap_or(0)
        .max("module".len());
    let mut lines = vec![format!(
        "{:<width$}  {:<10}  {:>6} {:>6} {:>6} {:>7} {:>6}",
        "module", "state", "pass", "fail", "error", "ignored", "total"
    )];
    let sum = |f: fn(&Module) -> u32| report.modules.iter().map(f).sum::<u32>();
    for m in &report.modules {
        lines.push(format!(
            "{:<width$}  {:<10}  {:>6} {:>6} {:>6} {:>7} {:>6}",
            m.module,
            m.state.name(),
            m.pass,
            m.fail,
            m.error,
            m.ignored,
            m.total
        ));
        if !m.detail.is_empty() {
            lines.push(format!("{:<width$}    {}", "", m.detail));
        }
    }
    lines.push(format!(
        "{:<width$}  {:<10}  {:>6} {:>6} {:>6} {:>7} {:>6}",
        "all",
        "",
        sum(|m| m.pass),
        sum(|m| m.fail),
        sum(|m| m.error),
        sum(|m| m.ignored),
        sum(|m| m.total)
    ));
    if !report.counters.is_empty() {
        lines.push(format!(
            "top counters (of {}, summed over the modules):",
            report.counters.len()
        ));
        for c in report.counters.iter().take(top) {
            lines.push(format!("  {:>6}  {} {}", c.count, c.kind, c.name));
        }
    }
    lines
}

/// What got worse against `base`: a module whose state is worse (or, when `full`, absent), and a test that passed in `base` and does not pass now. Tests that fail in both, and new modules and tests, are not findings.
pub fn regressions(base: &Report, now: &Report, full: bool) -> Vec<String> {
    let mut found = Vec::new();
    for b in &base.modules {
        let Some(n) = now.modules.iter().find(|m| m.module == b.module) else {
            if full {
                found.push(format!("{}: missing from the run", b.module));
            }
            continue;
        };
        if n.state > b.state {
            found.push(format!(
                "{}: {} -> {}{}",
                b.module,
                b.state.name(),
                n.state.name(),
                if n.detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", n.detail)
                }
            ));
        }
        for (test, was) in &b.tests {
            if *was != Outcome::Pass {
                continue;
            }
            match n.tests.get(test) {
                Some(Outcome::Pass) => {}
                Some(o) => found.push(format!("{}: {test}: pass -> {o:?}", b.module)),
                None => found.push(format!("{}: {test}: pass -> not run", b.module)),
            }
        }
    }
    found
}

/// `text` with the workspace's own `<ws>/` prefix cut: a BUILD_FAIL detail names ninja causes, the load error and the APK a failed build did not write, all under the workspace, so `report.json` and a saved baseline read the same on any checkout.
/// Paths outside the workspace stay absolute.
pub fn relative_to_workspace(text: &str, ws: &Path) -> String {
    text.replace(&format!("{}/", ws.display()), "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::parse;

    fn session(text: &str) -> Session {
        parse(&text.lines().collect::<Vec<_>>())
    }

    /// The status blocks of one test: a start block and its end block with `code`.
    fn blocks(class: &str, test: &str, end: Option<i32>, numtests: u32) -> String {
        let mut out = format!(
            "INSTRUMENTATION_STATUS: class={class}\nINSTRUMENTATION_STATUS: numtests={numtests}\nINSTRUMENTATION_STATUS: test={test}\nINSTRUMENTATION_STATUS_CODE: 1\n"
        );
        if let Some(code) = end {
            out += &format!(
                "INSTRUMENTATION_STATUS: class={class}\nINSTRUMENTATION_STATUS: numtests={numtests}\nINSTRUMENTATION_STATUS: test={test}\nINSTRUMENTATION_STATUS_CODE: {code}\n"
            );
        }
        out
    }

    #[test]
    fn a_finished_run_counts_each_kind() {
        let text = [
            blocks("p.C", "a", Some(0), 5),
            blocks("p.C", "b", Some(-2), 5),
            blocks("p.C", "c", Some(-1), 5),
            blocks("p.C", "d", Some(-3), 5),
            blocks("p.C", "e", Some(-4), 5),
            "INSTRUMENTATION_CODE: -1\n".to_owned(),
        ]
        .concat();
        let m = classify("M", &session(&text), false, 60);
        assert_eq!(m.state, State::Ran);
        assert_eq!(
            (m.pass, m.fail, m.error, m.ignored, m.total),
            (1, 1, 1, 2, 5)
        );
        assert_eq!(m.tests["p.C#b"], Outcome::Fail);
        assert_eq!(m.tests["p.C#e"], Outcome::Ignored);
        assert!(m.detail.is_empty());
    }

    #[test]
    fn fewer_tests_than_announced_is_incomplete() {
        let text = [
            blocks("p.C", "a", Some(0), 3),
            "INSTRUMENTATION_CODE: -1\n".to_owned(),
        ]
        .concat();
        let m = classify("M", &session(&text), false, 60);
        assert_eq!(m.state, State::Incomplete);
        assert_eq!(m.detail, "1 of 3 announced tests ended");
        // a run that announced nothing cannot be judged complete
        let none = session(
            "INSTRUMENTATION_STATUS: test=a\nINSTRUMENTATION_STATUS_CODE: 1\nINSTRUMENTATION_STATUS: test=a\nINSTRUMENTATION_STATUS_CODE: 0\nINSTRUMENTATION_CODE: -1\n",
        );
        assert_eq!(classify("M", &none, false, 60).state, State::Incomplete);
    }

    /// The process dies mid-run: the last test started and never ended, the code is 0 with `shortMsg` (instrument.rs's crash run).
    #[test]
    fn a_dead_process_is_a_crash() {
        let text = [
            blocks("p.C", "a", Some(0), 4),
            blocks("p.C", "b", None, 4),
            "INSTRUMENTATION_RESULT: shortMsg=Process crashed.\nINSTRUMENTATION_CODE: 0\n"
                .to_owned(),
        ]
        .concat();
        let m = classify("M", &session(&text), false, 60);
        assert_eq!(m.state, State::Crash);
        assert_eq!(m.detail, "Process crashed. after 1 of 4 tests");
        assert_eq!(m.tests["p.C#b"], Outcome::Incomplete);
        assert_eq!((m.pass, m.total), (1, 4));
    }

    #[test]
    fn no_test_started_is_a_start_failure() {
        let failed = session("INSTRUMENTATION_FAILED: com.x/.Runner\n");
        let m = classify("M", &failed, false, 60);
        assert_eq!(m.state, State::StartFail);
        assert_eq!(m.detail, "INSTRUMENTATION_FAILED: com.x/.Runner");
        let died = session(
            "INSTRUMENTATION_RESULT: shortMsg=java.lang.IllegalStateException\nINSTRUMENTATION_RESULT: longMsg=boom\nINSTRUMENTATION_CODE: 0\n",
        );
        assert_eq!(
            classify("M", &died, false, 60).detail,
            "java.lang.IllegalStateException: boom"
        );
        assert_eq!(
            classify("M", &Session::default(), false, 60).state,
            State::StartFail
        );
        assert_eq!(
            classify("M", &Session::default(), true, 60).detail,
            "no test started within 60 s"
        );
        // an OK run without a test: the filter matched nothing
        let empty = session("INSTRUMENTATION_CODE: -1\n");
        assert_eq!(classify("M", &empty, false, 60).state, State::StartFail);
    }

    #[test]
    fn the_time_limit_stops_a_run_incomplete() {
        let text = [blocks("p.C", "a", Some(0), 9), blocks("p.C", "b", None, 9)].concat();
        let m = classify("M", &session(&text), true, 60);
        assert_eq!(m.state, State::Incomplete);
        assert_eq!(m.detail, "stopped after 60 s with 1 of 9 tests ended");
    }

    /// One name twice keeps the worse outcome and a start after an end does not undo it.
    #[test]
    fn a_repeated_test_keeps_its_worse_outcome() {
        let text = [
            blocks("p.C", "a", Some(-2), 2),
            blocks("p.C", "a", Some(0), 2),
            "INSTRUMENTATION_CODE: -1\n".to_owned(),
        ]
        .concat();
        let m = classify("M", &session(&text), false, 60);
        assert_eq!(m.tests["p.C#a"], Outcome::Fail);
        assert_eq!((m.pass, m.fail, m.state), (1, 1, State::Ran));
    }

    fn module(name: &str, state: State, tests: &[(&str, Outcome)]) -> Module {
        Module {
            tests: tests.iter().map(|(t, o)| ((*t).to_owned(), *o)).collect(),
            ..Module::without_run(name, state, String::new())
        }
    }

    fn report(modules: Vec<Module>) -> Report {
        Report {
            schema: SCHEMA,
            host: "x86_64".into(),
            modules,
            counters: Vec::new(),
        }
    }

    #[test]
    fn regressions_are_worse_states_and_lost_passes() {
        let base = report(vec![
            module(
                "A",
                State::Ran,
                &[("t1", Outcome::Pass), ("t2", Outcome::Fail)],
            ),
            module("B", State::Crash, &[]),
            module("C", State::Ran, &[]),
        ]);
        // nothing worse: t2 may stay failing or start passing, B may improve, a new module is no finding
        let better = report(vec![
            module(
                "A",
                State::Ran,
                &[("t1", Outcome::Pass), ("t2", Outcome::Pass)],
            ),
            module("B", State::Ran, &[]),
            module("C", State::Ran, &[]),
            module("D", State::BuildFail, &[]),
        ]);
        assert_eq!(regressions(&base, &better, true), Vec::<String>::new());
        let worse = report(vec![
            module("A", State::Incomplete, &[("t2", Outcome::Pass)]),
            module("B", State::StartFail, &[]),
        ]);
        let found = regressions(&base, &worse, true);
        assert_eq!(
            found,
            [
                "A: RAN -> INCOMPLETE",
                "A: t1: pass -> not run",
                "B: CRASH -> START_FAIL",
                "C: missing from the run",
            ]
        );
        // a run of selected modules does not report the others missing
        assert_eq!(regressions(&base, &worse, false).len(), 3);
        let failing = report(vec![module("A", State::Ran, &[("t1", Outcome::Error)])]);
        assert_eq!(
            regressions(&base, &failing, false),
            ["A: t1: pass -> Error"]
        );
    }

    #[test]
    fn counters_sum_across_modules_and_rank() {
        let c = |name: &str, count| Counter {
            kind: "unimplemented".into(),
            name: name.into(),
            count,
        };
        let sum = sum_counters(&[vec![c("a", 2), c("b", 5)], vec![c("a", 4)]]);
        assert_eq!(sum, [c("a", 6), c("b", 5)]);
    }

    #[test]
    fn the_summary_names_each_module_and_the_top_counters() {
        let mut r = report(vec![
            Module {
                pass: 3,
                fail: 1,
                total: 4,
                ..module("Alpha", State::Ran, &[])
            },
            Module::without_run("Beta", State::BuildFail, "no apk".into()),
        ]);
        r.counters = vec![
            Counter {
                kind: "refused".into(),
                name: "x".into(),
                count: 9,
            },
            Counter {
                kind: "unimplemented".into(),
                name: "y".into(),
                count: 1,
            },
        ];
        let lines = summary(&r, 1);
        assert!(lines[1].starts_with("Alpha"));
        assert!(lines[1].contains("RAN"));
        assert!(lines[3].trim_start().starts_with("no apk"));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("all") && l.ends_with("4"))
        );
        assert!(lines.last().is_some_and(|l| l.ends_with("refused x")));
        assert!(!lines.iter().any(|l| l.ends_with("unimplemented y")));
    }

    /// A BUILD_FAIL detail loses the workspace prefix of its paths; a path outside the workspace keeps its own.
    #[test]
    fn build_fail_paths_are_workspace_relative() {
        let detail = "device/libcts_jni: /home/wavjaby/atl-next/src/aosp/cts/x.cpp:22:10: fatal error: 'sys/hwprobe.h' file not found; /home/wavjaby/atl-next/out/java/build/device/X/X.apk does not exist; /usr/include/y.h";
        assert_eq!(
            relative_to_workspace(detail, Path::new("/home/wavjaby/atl-next")),
            "device/libcts_jni: src/aosp/cts/x.cpp:22:10: fatal error: 'sys/hwprobe.h' file not found; out/java/build/device/X/X.apk does not exist; /usr/include/y.h"
        );
    }

    /// The saved baseline is what a later run is compared against: it names no machine path, so the same file reads the same on every checkout.
    #[test]
    fn the_saved_baseline_names_no_machine_path() {
        let base: Report = serde_json::from_str(include_str!("../testdata/cts/baseline.json"))
            .expect("testdata baseline parses");
        for m in &base.modules {
            assert!(
                !m.detail.contains("/home/"),
                "{}: machine path in detail: {}",
                m.module,
                m.detail
            );
        }
    }
}
