// SPDX-License-Identifier: MIT
//! `xtask test instrument`: the device-side path of the CTS runner (owner decision 2026-11-03: our own `xtask test cts` drives AOSP's `am instrument`, not Tradefed). A JUnit3 test APK
//! (xtask/testdata/instrument: its own instrumentation target, `android.test.InstrumentationTestRunner` from frameworks/base/test-runner) is installed by atld next to the shared libraries
//! android.test.{runner,base,mock}; AOSP's `am` (frameworks/base/cmds/am, built by the converter) runs as the shell uid and calls ActivityManager.startInstrumentation; the services start the
//! process through atld's process control, bind it with the instrumentation, merge and report its results to `am`'s watcher, and kill it when the run finishes.
//! `am -w -r` prints Tradefed's text format (INSTRUMENTATION_STATUS / _STATUS_CODE / _RESULT / _CODE); [`parse`] reads it as Tradefed's InstrumentationResultParser does.
//! `am`'s System.out is redirected into liblog by ZygoteInit.zygoteInit (RuntimeInit.redirectLogStreams), as for any app process, so its lines are `liblog/System.out` records in its own log
//! (`log/am.log`); [`stdout_lines`] takes the text back out of them.
//! Run 1 (SampleTest: passes, fails an assertion, throws): per test the codes 1 then 0 / -2 / -1, the final code -1 (RESULT_OK), the instrumented process killed by the services' finish.
//! Run 2 (CrashTest: the process kills itself mid-run): the run ends with code 0 (RESULT_CANCELED) and `shortMsg=Process crashed.`, without a kill from the services.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};

use atl_services::packages::Library;

use crate::atld_run::{Run, Runtime, Shell};
use crate::manifest;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default <workspace>/out/native/<host>, as `xtask native`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8, `am` and the android.test.* libraries (default <workspace>/out/java/build); build them with
    /// `xtask native --out <dir> platform-bootclasspath am android.test.runner.impl android.test.base.impl android.test.mock.impl`.
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Print every run's whole `am` output.
    #[arg(long, short)]
    verbose: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/instrument";
/// The shell's uid (AID_SHELL, system/core/libcutils/include/private/android_filesystem_config.h): `am` runs as it, as `adb shell am` does.
const SHELL_UID: u32 = 2000;
/// The test package's uid: AID_APP_START + 123, any app id.
const APP_UID: u32 = 10_123;
/// AOSP's `am` main class (frameworks/base/cmds/am/Android.bp `am`; am.sh runs it through app_process).
const AM_CLASS: &str = "com.android.commands.am.Am";
/// Where the Java pipeline puts the dex jars (bp2ninja java.go): `device/<module>/aligned/<stem>.jar`.
const AM_JAR: &str = "device/am/aligned/am.jar";
/// The shared libraries the fixture's manifest uses (`<uses-library>`): library name, the java_sdk_library `.impl` module whose dex jar is it.
/// and the libraries it depends on, as frameworks/base/data/etc/platform.xml declares them (`<library name="android.test.runner" ... dependency="android.test.base:android.test.mock" />`).
const LIBRARIES: [(&str, &str, &[&str]); 3] = [
    (
        "android.test.runner",
        "android.test.runner.impl",
        &["android.test.base", "android.test.mock"],
    ),
    ("android.test.base", "android.test.base.impl", &[]),
    ("android.test.mock", "android.test.mock.impl", &[]),
];
/// The class jar the fixture's tests compile against (JUnit3's TestCase is in android.test.base, not in android.jar): javac's output of that module.
const TEST_BASE_CLASSES: &str = "device/android.test.base.impl/javac/android.test.base.jar";
/// The instrumentation the fixture's manifest declares and the test classes the runs name (`am instrument -e class`).
const RUNNER: &str = "android.test.InstrumentationTestRunner";
const SAMPLE_TEST: &str = "com.example.atlinstr.SampleTest";
const CRASH_TEST: &str = "com.example.atlinstr.CrashTest";
/// How long one run may take: two cold ART starts (`am` and the instrumented process), each interpreting framework.jar's class initialisation; 300 s for a loaded machine.
const LIMIT: Duration = Duration::from_secs(300);
/// Lines of a log shown on failure.
const TAIL: usize = 40;

/// Instrumentation status codes (Instrumentation.java REPORT_VALUE_RESULT_*: START 1, OK 0, ERROR -1, FAILURE -2), and the result codes (Activity.RESULT_OK -1, RESULT_CANCELED 0).
const STATUS_START: i32 = 1;
const STATUS_OK: i32 = 0;
const STATUS_ERROR: i32 = -1;
const STATUS_FAILURE: i32 = -2;
const RESULT_OK: i32 = -1;
const RESULT_CANCELED: i32 = 0;

/// One `INSTRUMENTATION_STATUS` block: its key=value lines and the code that closed it.
#[derive(Debug, Default, PartialEq)]
pub struct Status {
    pub values: BTreeMap<String, String>,
    pub code: i32,
}

/// What `am instrument -r` printed.
#[derive(Debug, Default, PartialEq)]
pub struct Session {
    pub statuses: Vec<Status>,
    /// INSTRUMENTATION_RESULT's key=value lines.
    pub result: BTreeMap<String, String>,
    /// INSTRUMENTATION_CODE
    pub code: Option<i32>,
    /// INSTRUMENTATION_FAILED / INSTRUMENTATION_ABORTED text
    pub failed: Option<String>,
}

/// The text a process wrote to System.out, from its log: ZygoteInit.zygoteInit redirects System.out into liblog (tag `System.out`), whose records the launcher's logger writes as
/// `<realtime> <monotonic> <pid> <tid> <level> liblog/System.out: <line>` (atl-app-launcher log.rs `format`: one record per line of the message).
pub fn stdout_lines(log: &str) -> Vec<&str> {
    const SOURCE: &str = " liblog/System.out: ";
    log.lines()
        .filter_map(|l| l.split_once(SOURCE).map(|(_, text)| text))
        .collect()
}

/// Tradefed's InstrumentationResultParser (tradefed/result/InstrumentationResultParser.java), the part the gate uses: `INSTRUMENTATION_STATUS: key=value` lines gather into a block that
/// `INSTRUMENTATION_STATUS_CODE: n` closes; `INSTRUMENTATION_RESULT: key=value` gather into the final result, closed by `INSTRUMENTATION_CODE: n`; a line with none of those prefixes continues the
/// current value on a new line (stack traces, `stream=` text); `INSTRUMENTATION_FAILED:` / `INSTRUMENTATION_ABORTED:` end the session with that text. Every value is kept verbatim.
pub fn parse(lines: &[&str]) -> Session {
    #[derive(PartialEq)]
    enum In {
        Nothing,
        Status,
        Result,
    }
    const STATUS: &str = "INSTRUMENTATION_STATUS: ";
    const STATUS_CODE: &str = "INSTRUMENTATION_STATUS_CODE: ";
    const RESULT: &str = "INSTRUMENTATION_RESULT: ";
    const CODE: &str = "INSTRUMENTATION_CODE: ";
    const FAILED: &str = "INSTRUMENTATION_FAILED: ";
    const ABORTED: &str = "INSTRUMENTATION_ABORTED: ";
    let mut session = Session::default();
    let mut block = BTreeMap::<String, String>::new();
    let mut section = In::Nothing;
    let mut last_key = String::new();
    let put = |map: &mut BTreeMap<String, String>, key: &mut String, kv: &str| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        *key = k.to_owned();
        map.insert(k.to_owned(), v.to_owned());
    };
    for line in lines {
        if let Some(kv) = line.strip_prefix(STATUS) {
            section = In::Status;
            put(&mut block, &mut last_key, kv);
        } else if let Some(code) = line.strip_prefix(STATUS_CODE) {
            if let Ok(code) = code.trim().parse() {
                session.statuses.push(Status {
                    values: std::mem::take(&mut block),
                    code,
                });
            }
            section = In::Nothing;
        } else if let Some(kv) = line.strip_prefix(RESULT) {
            section = In::Result;
            put(&mut session.result, &mut last_key, kv);
        } else if let Some(code) = line.strip_prefix(CODE) {
            session.code = code.trim().parse().ok();
            section = In::Nothing;
        } else if let Some(text) = line
            .strip_prefix(FAILED)
            .or_else(|| line.strip_prefix(ABORTED))
        {
            session.failed = Some(text.to_owned());
        } else {
            let map = match section {
                In::Status => Some(&mut block),
                In::Result => Some(&mut session.result),
                In::Nothing => None,
            };
            if let Some(value) = map.and_then(|m| m.get_mut(&last_key)) {
                value.push('\n');
                value.push_str(line);
            }
        }
    }
    session
}

impl Session {
    /// The codes the status blocks of `test` carried, in order.
    fn codes(&self, test: &str) -> Vec<i32> {
        self.statuses
            .iter()
            .filter(|s| s.values.get("test").is_some_and(|t| t == test))
            .map(|s| s.code)
            .collect()
    }

    fn value(&self, test: &str, key: &str) -> Option<&str> {
        self.statuses
            .iter()
            .rev()
            .filter(|s| s.values.get("test").is_some_and(|t| t == test))
            .find_map(|s| s.values.get(key).map(String::as_str))
    }
}

/// The checks of run 1; each failure as a line.
fn check_sample(s: &Session) -> Vec<String> {
    let mut fails = Vec::new();
    for (test, end) in [
        ("testPass", STATUS_OK),
        ("testFail", STATUS_FAILURE),
        ("testError", STATUS_ERROR),
    ] {
        let want = vec![STATUS_START, end];
        let got = s.codes(test);
        if got != want {
            fails.push(format!("{test}: status codes {got:?}, want {want:?}"));
        }
    }
    for (test, needle) in [
        ("testFail", "deliberate failure"),
        ("testError", "IllegalStateException: deliberate error"),
    ] {
        if !s.value(test, "stack").is_some_and(|st| st.contains(needle)) {
            fails.push(format!("{test}: no `stack` with {needle:?}"));
        }
    }
    if s.code != Some(RESULT_OK) {
        fails.push(format!(
            "INSTRUMENTATION_CODE {:?}, want {RESULT_OK}",
            s.code
        ));
    }
    if !s
        .result
        .get("stream")
        .is_some_and(|r| r.contains("Tests run: 3"))
    {
        fails.push(format!(
            "INSTRUMENTATION_RESULT stream lacks `Tests run: 3`: {:?}",
            s.result.get("stream")
        ));
    }
    if let Some(f) = &s.failed {
        fails.push(format!("INSTRUMENTATION_FAILED/ABORTED: {f}"));
    }
    fails
}

/// The checks of run 2: started, never finished, reported cancelled by the activity manager.
fn check_crash(s: &Session) -> Vec<String> {
    let mut fails = Vec::new();
    let got = s.codes("testCrash");
    if got != [STATUS_START] {
        fails.push(format!(
            "testCrash: status codes {got:?}, want [{STATUS_START}] (started, never finished)"
        ));
    }
    if s.code != Some(RESULT_CANCELED) {
        fails.push(format!(
            "INSTRUMENTATION_CODE {:?}, want {RESULT_CANCELED} (cancelled)",
            s.code
        ));
    }
    if s.result.get("shortMsg").map(String::as_str) != Some("Process crashed.") {
        fails.push(format!(
            "INSTRUMENTATION_RESULT shortMsg {:?}, want \"Process crashed.\"",
            s.result.get("shortMsg")
        ));
    }
    fails
}

/// What atld's log says of the instrumented process: its start (`started <process> (package <p>): uid .., host pid N`), whether the services killed it (`killed process N`), when it ended
/// (`process N ended: <status>`), and whether it outlived the run (`left at the end`).
#[derive(Debug, PartialEq)]
struct Lifecycle {
    pid: i32,
    killed_by_services: bool,
    ended: Option<String>,
    /// the kill came before `am` ended, i.e. it was the finish that killed the process and not atld's teardown
    killed_before_main: bool,
    left_running: bool,
}

fn lifecycle(atld_log: &str, process: &str) -> Option<Lifecycle> {
    let started = format!("started {process} ");
    let pid: i32 = atld_log
        .lines()
        .find(|l| l.contains(&started))?
        .rsplit("host pid ")
        .next()?
        .trim()
        .parse()
        .ok()?;
    let line_of = |needle: &str| atld_log.lines().position(|l| l.contains(needle));
    let ended_at = line_of(&format!("process {pid} ended: "));
    let killed_at = line_of(&format!("killed process {pid}"));
    let main_at = line_of("main process ended");
    Some(Lifecycle {
        pid,
        killed_by_services: killed_at.is_some(),
        ended: ended_at.and_then(|i| {
            atld_log
                .lines()
                .nth(i)
                .and_then(|l| l.split(" ended: ").nth(1))
                .map(str::to_owned)
        }),
        killed_before_main: matches!((killed_at, main_at), (Some(k), Some(m)) if k < m),
        left_running: atld_log.contains(&format!("left at the end: {pid}")),
    })
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// One atld run of `am instrument -w -r -e class <class> <package>/<RUNNER>`; the parsed session, atld's log and `am`'s lines.
fn am_instrument(rt: &Runtime, run: &Run, verbose: bool) -> Result<(Session, String, Vec<String>)> {
    let mut atld = rt.start_atld(run)?;
    let start = Instant::now();
    let status = loop {
        if let Some(s) = atld.try_wait()? {
            break Some(s);
        }
        if start.elapsed() > LIMIT {
            let _ = atld.kill();
            atld.wait()?;
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let am_log = read(&run.log("am"));
    let lines: Vec<String> = stdout_lines(&am_log)
        .into_iter()
        .map(str::to_owned)
        .collect();
    if verbose {
        println!("---- am output\n{}", lines.join("\n"));
    }
    let atld_log = read(&run.atld_log());
    println!(
        "instrument: atld {} after {:.1?}",
        status.map_or_else(|| "killed at the limit".to_owned(), |s| s.to_string()),
        start.elapsed()
    );
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    Ok((parse(&refs), atld_log, lines))
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "instrument-art",
        args.verbose,
    )?;
    let java = |rel: &str| rt.java_out.join(rel);
    let am_jar = java(AM_JAR);
    let mut libraries = Vec::new();
    for (name, module, dependencies) in LIBRARIES {
        let jar = java(&format!("device/{module}/aligned/{name}.jar"));
        ensure!(
            jar.is_file(),
            "missing {}: build `{module}` with the Java pipeline",
            jar.display()
        );
        libraries.push((
            name.to_owned(),
            Library {
                file: jar.display().to_string(),
                dependencies: dependencies.iter().map(|d| (*d).to_owned()).collect(),
            },
        ));
    }
    let test_base = java(TEST_BASE_CLASSES);
    for p in [&am_jar, &test_base] {
        ensure!(
            p.is_file(),
            "missing {}: build it with the Java pipeline (see --java-out)",
            p.display()
        );
    }
    let work = rt.out.join("instrument");
    let apk = rt.build_apk(&root.join(FIXTURE), &work.join("apk"), &[test_base])?;
    let m = manifest::read(&rt.aapt2, &apk)?;
    ensure!(
        m.instrumentation
            .iter()
            .any(|i| i.name == RUNNER && i.target_package == m.name),
        "the fixture declares no {RUNNER} instrumenting itself: {:?}",
        m.instrumentation
    );
    println!(
        "instrument: built {} ({} versionCode {}, instrumentation {RUNNER}, libraries {:?})",
        apk.display(),
        m.name,
        m.version_code,
        m.libraries
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>()
    );
    let component = format!("{}/{RUNNER}", m.name);
    let mut all_fails = Vec::new();
    for (label, class) in [("sample", SAMPLE_TEST), ("crash", CRASH_TEST)] {
        let run = Run {
            root: work.join(format!("root-{label}")),
            scratch: work.join(format!("scratch-{label}")),
            packages: vec![(apk.clone(), m.clone())],
            first_uid: APP_UID,
            launch: None,
            libraries: libraries.clone(),
            shell: Some(Shell {
                class: AM_CLASS.to_owned(),
                uid: SHELL_UID,
                name: "am".to_owned(),
                args: ["instrument", "-w", "-r", "-e", "class", class, &component]
                    .map(str::to_owned)
                    .to_vec(),
                boot_jars: vec![am_jar.clone()],
            }),
            log_tags: "*:i".to_owned(),
        };
        let (session, atld_log, lines) = am_instrument(&rt, &run, args.verbose)?;
        let life = lifecycle(&atld_log, &m.name);
        let mut fails = match label {
            "sample" => check_sample(&session),
            _ => check_crash(&session),
        };
        match (&life, label) {
            (None, _) => fails.push(format!("atld never started {}", m.name)),
            (Some(l), "sample") => {
                if !l.killed_by_services {
                    fails.push(format!(
                        "finishing the run did not kill process {} (no `killed process`)",
                        l.pid
                    ));
                }
                if !l.killed_before_main {
                    fails.push(format!(
                        "process {} was not killed before `am` ended (atld's teardown would be what ended it)",
                        l.pid
                    ));
                }
                if l.left_running {
                    fails.push(format!("process {} was still running at the end", l.pid));
                }
            }
            (Some(l), _) => {
                if l.killed_by_services {
                    fails.push(format!(
                        "process {} died by itself but the services killed it",
                        l.pid
                    ));
                }
                if l.ended.is_none() {
                    fails.push(format!("process {} never ended", l.pid));
                }
            }
        }
        for l in read_counters(&run) {
            println!("instrument: {label}: counter {l}");
        }
        if fails.is_empty() {
            match label {
                "sample" => println!(
                    "instrument: sample pass (testPass 1,0; testFail 1,-2; testError 1,-1; final code -1; process {} killed by the finish)",
                    life.as_ref().map_or(0, |l| l.pid)
                ),
                _ => println!(
                    "instrument: crash pass (process {} died mid-run: code 0, shortMsg \"Process crashed.\")",
                    life.as_ref().map_or(0, |l| l.pid)
                ),
            }
        } else {
            for f in &fails {
                println!("instrument: {label}: FAIL {f}");
            }
            println!(
                "---- am output ({label}, last {TAIL} lines)\n{}",
                tail(&lines.join("\n"), TAIL)
            );
            println!("---- atld log ({label})\n{}", tail(&atld_log, TAIL));
            println!(
                "---- services log ({label})\n{}",
                tail(
                    &std::fs::read_to_string(run.log("services")).unwrap_or_default(),
                    TAIL
                )
            );
            println!(
                "---- app log ({label})\n{}",
                tail(
                    &std::fs::read_to_string(run.log(&m.name)).unwrap_or_default(),
                    TAIL
                )
            );
        }
        all_fails.extend(fails);
    }
    Ok(if all_fails.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The services' unimplemented-call counter lines (atl_services::counters), as `<interface>.<method> <n>`, last count per call.
fn read_counters(run: &Run) -> Vec<String> {
    const COUNTER: &str = "services: unimplemented ";
    let log = std::fs::read_to_string(run.log("services")).unwrap_or_default();
    let mut last = BTreeMap::new();
    for l in log.lines().filter_map(|l| l.strip_prefix(COUNTER)) {
        if let Some((call, n)) = l.rsplit_once(' ') {
            last.insert(call.to_owned(), n.to_owned());
        }
    }
    last.into_iter().map(|(c, n)| format!("{c} x{n}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(text: &str) -> Session {
        parse(&text.lines().collect::<Vec<_>>())
    }

    /// The text format Instrument.java's TextStatusReporter prints (raw mode: every key; frameworks/base/cmds/am Instrument.java:176-205) and InstrumentationTestRunner's keys: a start and a
    /// failure block of one test, a multi-line stack and stream, then the result and the code.
    const RUN: &str = "\
INSTRUMENTATION_STATUS: class=com.example.T
INSTRUMENTATION_STATUS: current=1
INSTRUMENTATION_STATUS: numtests=2
INSTRUMENTATION_STATUS: stream=
com.example.T:
INSTRUMENTATION_STATUS: test=testA
INSTRUMENTATION_STATUS_CODE: 1
INSTRUMENTATION_STATUS: class=com.example.T
INSTRUMENTATION_STATUS: stack=junit.framework.AssertionFailedError: x
\tat com.example.T.testA(T.java:9)
INSTRUMENTATION_STATUS: test=testA
INSTRUMENTATION_STATUS_CODE: -2
INSTRUMENTATION_RESULT: stream=
Tests run: 2,  Failures: 1
INSTRUMENTATION_CODE: -1
";

    #[test]
    fn parses_tradefed_text() {
        let s = session(RUN);
        assert_eq!(s.statuses.len(), 2);
        assert_eq!(s.statuses[0].code, 1);
        assert_eq!(s.statuses[0].values["test"], "testA");
        assert_eq!(s.statuses[0].values["stream"], "\ncom.example.T:");
        assert_eq!(s.statuses[1].code, -2);
        assert_eq!(
            s.statuses[1].values["stack"],
            "junit.framework.AssertionFailedError: x\n\tat com.example.T.testA(T.java:9)"
        );
        assert_eq!(s.result["stream"], "\nTests run: 2,  Failures: 1");
        assert_eq!(s.code, Some(-1));
        assert_eq!(s.codes("testA"), [1, -2]);
    }

    #[test]
    fn failure_text_and_noise() {
        // lines outside a section are ignored; a failed start ends the session with its text
        let s = session("noise\nINSTRUMENTATION_FAILED: com.x/.Runner\nmore noise\n");
        assert_eq!(
            s,
            Session {
                failed: Some("com.x/.Runner".into()),
                ..Session::default()
            }
        );
        // a status without its code is not a block
        assert!(session("INSTRUMENTATION_STATUS: a=b\n").statuses.is_empty());
    }

    /// atl-app-launcher's log format (log.rs `format`): one record per line, the System.out tag.
    #[test]
    fn stdout_is_taken_out_of_the_log_records() {
        let log = "1762000000.007 5.250    42    43 I liblog/System.out: INSTRUMENTATION_CODE: -1\n\
                   1762000000.008 5.251    42    43 W liblog/Other: not stdout\n\
                   1762000000.009 5.252    42    43 I liblog/System.out: line two\n";
        assert_eq!(stdout_lines(log), ["INSTRUMENTATION_CODE: -1", "line two"]);
    }

    #[test]
    fn the_checks_fire() {
        let ok = session(
            "INSTRUMENTATION_STATUS: test=testPass\nINSTRUMENTATION_STATUS_CODE: 1\n\
             INSTRUMENTATION_STATUS: test=testPass\nINSTRUMENTATION_STATUS_CODE: 0\n\
             INSTRUMENTATION_STATUS: test=testFail\nINSTRUMENTATION_STATUS_CODE: 1\n\
             INSTRUMENTATION_STATUS: stack=AssertionFailedError: deliberate failure\nINSTRUMENTATION_STATUS: test=testFail\nINSTRUMENTATION_STATUS_CODE: -2\n\
             INSTRUMENTATION_STATUS: test=testError\nINSTRUMENTATION_STATUS_CODE: 1\n\
             INSTRUMENTATION_STATUS: stack=java.lang.IllegalStateException: deliberate error\nINSTRUMENTATION_STATUS: test=testError\nINSTRUMENTATION_STATUS_CODE: -1\n\
             INSTRUMENTATION_RESULT: stream=Tests run: 3,  Failures: 1,  Errors: 1\nINSTRUMENTATION_CODE: -1\n",
        );
        assert_eq!(check_sample(&ok), Vec::<String>::new());
        assert!(!check_sample(&Session::default()).is_empty());
        let crashed = session(
            "INSTRUMENTATION_STATUS: test=testCrash\nINSTRUMENTATION_STATUS_CODE: 1\n\
             INSTRUMENTATION_RESULT: shortMsg=Process crashed.\nINSTRUMENTATION_CODE: 0\n",
        );
        assert_eq!(check_crash(&crashed), Vec::<String>::new());
        // a run that finished normally is not a crash
        assert_eq!(check_crash(&ok).len(), 3);
    }

    #[test]
    fn atld_log_lifecycle() {
        let log = "atld: started com.example.atlinstr (package com.example.atlinstr): uid 10123, host pid 77\n\
                   atld: killed process 77\n\
                   atld: process 77 ended: signal: 9 (SIGKILL)\n\
                   atld: process 80 ended: exit status: 0\n\
                   atld: main process ended: exit status: 0\n";
        assert_eq!(
            lifecycle(log, "com.example.atlinstr"),
            Some(Lifecycle {
                pid: 77,
                killed_by_services: true,
                ended: Some("signal: 9 (SIGKILL)".into()),
                killed_before_main: true,
                left_running: false,
            })
        );
        assert_eq!(lifecycle("atld: nothing\n", "p"), None);
    }
}
