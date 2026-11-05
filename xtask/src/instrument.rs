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
//! Run 3 (the converter-built `libcore-sdk-current-test`, androidx's AndroidJUnitRunner): LinkedHashMapTest passes; the runner's InstrumentationConnection broadcasts an Intent whose extras hold a binder and registers a receiver, which the services take (counted) without delivering.
//! The services' counters (unimplemented calls, refused parcelables) are printed for every run.
//! `xtask test cts` (cts.rs) reuses the run machinery ([`am_instrument`], [`am_shell`], [`libraries`]) and the parser for the cts.toml modules.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};

use atl_services::packages::Library;

use crate::atld_run::{Run, Runtime, Shell, counter_lines};
use crate::manifest;
use crate::verdict::{Failure, Verdict};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8, `am` and the android.test.* libraries (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build); build them with
    /// `xtask native --out <dir> platform-bootclasspath am android.test.runner.impl android.test.base.impl android.test.mock.impl`.
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Which test APKs to run: the hand-built fixture (two runs), the converter-built `libcore-sdk-current-test` (build it with the Java pipeline too; its `androidx.test` runner broadcasts a Bundle holding a binder and registers a receiver), or both.
    #[arg(long, value_enum, default_value = "all")]
    case: CaseSet,
    /// Print every run's whole `am` output.
    #[arg(long, short)]
    verbose: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/instrument";
/// The shell's uid (AID_SHELL, system/core/libcutils/include/private/android_filesystem_config.h): `am` runs as it, as `adb shell am` does.
const SHELL_UID: u32 = 2000;
/// The test package's uid: AID_APP_START + 123, any app id.
pub const APP_UID: u32 = 10_123;
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

/// The converter-built test APK (bp2ninja app.go `android_test`): libcore's JUnit4 test of LinkedHashMap, run by androidx's AndroidJUnitRunner (libcore/luni/src/test/androidsdkcurrent).
const CONVERTER_MODULE: &str = "libcore-sdk-current-test";
const CONVERTER_RUNNER: &str = "androidx.test.runner.AndroidJUnitRunner";
const CONVERTER_CLASS: &str = "libcore.sdkcurrent.java.util.LinkedHashMapTest";
const CONVERTER_TEST: &str = "test_entryCompatibility_compiletime";

/// Which APKs a run uses: the hand-built fixture's two runs, the converter-built APK's, or all.
#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
enum CaseSet {
    All,
    Fixture,
    Converter,
}

/// What a run must show.
#[derive(Clone, Copy)]
enum Expect {
    /// SampleTest: one passing, one failing, one erroring test
    Sample,
    /// CrashTest: the process kills itself mid-run
    Crash,
    /// these tests of the class all pass
    Passing(&'static [&'static str]),
}

/// One `am instrument` run of the gate: an installed package and the component it runs.
struct Case<'a> {
    label: &'a str,
    apk: PathBuf,
    manifest: atl_services::packages::Manifest,
    runner: &'a str,
    class: &'a str,
    expect: Expect,
}

/// Instrumentation status codes (Instrumentation.java REPORT_VALUE_RESULT_*: START 1, OK 0, ERROR -1, FAILURE -2; androidx.test.internal.runner.listener.InstrumentationResultPrinter adds IGNORED -3 and ASSUMPTION_FAILURE -4),
/// and the result codes (Activity.RESULT_OK -1, RESULT_CANCELED 0).
pub const STATUS_START: i32 = 1;
pub const STATUS_OK: i32 = 0;
pub const STATUS_ERROR: i32 = -1;
pub const STATUS_FAILURE: i32 = -2;
pub const STATUS_IGNORED: i32 = -3;
pub const STATUS_ASSUMPTION_FAILURE: i32 = -4;
pub const RESULT_OK: i32 = -1;
pub const RESULT_CANCELED: i32 = 0;

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

/// The checks of a run of tests that must all pass: each named test started (1) and ended OK (0), the final code RESULT_OK, no failed session.
fn check_passing(s: &Session, tests: &[&str]) -> Vec<String> {
    let mut fails = Vec::new();
    for test in tests {
        let want = vec![STATUS_START, STATUS_OK];
        let got = s.codes(test);
        if got != want {
            fails.push(format!("{test}: status codes {got:?}, want {want:?}"));
            if let Some(stack) = s.value(test, "stack") {
                fails.push(format!("{test}: stack {stack}"));
            }
        }
    }
    if s.code != Some(RESULT_OK) {
        fails.push(format!(
            "INSTRUMENTATION_CODE {:?}, want {RESULT_OK}",
            s.code
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

/// What one `am instrument` run left: the parsed session, atld's log, `am`'s lines, whether the time limit ended it and how long it took.
pub struct Outcome {
    pub session: Session,
    pub atld_log: String,
    pub lines: Vec<String>,
    pub timed_out: bool,
    pub elapsed: Duration,
}

/// One atld run of `am instrument -w -r ...` (the `Shell` of `run`), killed after `limit`.
pub fn am_instrument(rt: &Runtime, run: &Run, limit: Duration, verbose: bool) -> Result<Outcome> {
    let mut atld = rt.start_atld(run)?;
    let start = Instant::now();
    let timed_out = loop {
        if atld.try_wait()?.is_some() {
            break false;
        }
        if start.elapsed() > limit {
            let _ = atld.kill();
            atld.wait()?;
            break true;
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
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    Ok(Outcome {
        session: parse(&refs),
        atld_log: read(&run.atld_log()),
        lines,
        timed_out,
        elapsed: start.elapsed(),
    })
}

/// The jar of AOSP's `am` in the Java pipeline's out.
fn am_jar(rt: &Runtime) -> PathBuf {
    rt.java_out.join(AM_JAR)
}

/// The shared libraries every test run installs (name, library), from the Java pipeline's out; each must have been built.
pub fn libraries(rt: &Runtime) -> Result<Vec<(String, Library)>> {
    let mut libraries = Vec::new();
    for (name, module, dependencies) in LIBRARIES {
        let jar = rt
            .java_out
            .join(format!("device/{module}/aligned/{name}.jar"));
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
    ensure!(
        am_jar(rt).is_file(),
        "missing {}: build it with the Java pipeline (see --java-out)",
        am_jar(rt).display()
    );
    Ok(libraries)
}

/// `am <args>` as the shell user runs it.
pub fn am_shell(rt: &Runtime, args: Vec<String>) -> Shell {
    Shell {
        class: AM_CLASS.to_owned(),
        uid: SHELL_UID,
        name: "am".to_owned(),
        args,
        boot_jars: vec![am_jar(rt)],
    }
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
    let libraries = libraries(&rt)?;
    let test_base = java(TEST_BASE_CLASSES);
    ensure!(
        test_base.is_file(),
        "missing {}: build it with the Java pipeline (see --java-out)",
        test_base.display()
    );
    let work = rt.out.join("instrument");
    let mut cases = Vec::new();
    if args.case != CaseSet::Converter {
        let apk = rt.build_apk(&root.join(FIXTURE), &work.join("apk"), &[test_base], None)?;
        let m = manifest::read(&rt.aapt2, &apk)?;
        for (label, class, expect) in [
            ("sample", SAMPLE_TEST, Expect::Sample),
            ("crash", CRASH_TEST, Expect::Crash),
        ] {
            cases.push(Case {
                label,
                apk: apk.clone(),
                manifest: m.clone(),
                runner: RUNNER,
                class,
                expect,
            });
        }
    }
    if args.case != CaseSet::Fixture {
        // built by the converter from the AOSP tree (bp2ninja app.go): the apk is `device/<module>/<module>.apk` of the Java pipeline's out
        let apk = java(&format!("device/{CONVERTER_MODULE}/{CONVERTER_MODULE}.apk"));
        ensure!(
            apk.is_file(),
            "missing {}: build `{CONVERTER_MODULE}` with the Java pipeline (see --java-out)",
            apk.display()
        );
        cases.push(Case {
            label: "libcore",
            manifest: manifest::read(&rt.aapt2, &apk)?,
            apk,
            runner: CONVERTER_RUNNER,
            class: CONVERTER_CLASS,
            expect: Expect::Passing(&[CONVERTER_TEST]),
        });
    }
    let mut all_fails: Vec<Failure> = Vec::new();
    for Case {
        label,
        apk,
        manifest: m,
        runner,
        class,
        expect,
    } in cases
    {
        ensure!(
            m.instrumentation
                .iter()
                .any(|i| i.name == runner && i.target_package == m.name),
            "{label}: {} declares no {runner} instrumenting itself: {:?}",
            apk.display(),
            m.instrumentation
        );
        println!(
            "instrument: {label}: built {} ({} versionCode {}, instrumentation {runner}, libraries {:?})",
            apk.display(),
            m.name,
            m.version_code,
            m.libraries
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>()
        );
        let component = format!("{}/{runner}", m.name);
        let run = Run {
            root: work.join(format!("root-{label}")),
            scratch: work.join(format!("scratch-{label}")),
            packages: vec![(apk.clone(), m.clone())],
            first_uid: APP_UID,
            launch: None,
            libraries: libraries.clone(),
            shell: Some(am_shell(
                &rt,
                ["instrument", "-w", "-r", "-e", "class", class, &component]
                    .map(str::to_owned)
                    .to_vec(),
            )),
            log_tags: "*:i".to_owned(),
        };
        let Outcome {
            session,
            atld_log,
            lines,
            timed_out,
            elapsed,
        } = am_instrument(&rt, &run, LIMIT, args.verbose)?;
        println!(
            "instrument: atld {} after {elapsed:.1?}",
            if timed_out {
                "killed at the limit"
            } else {
                "ended"
            }
        );
        let life = lifecycle(&atld_log, &m.name);
        let mut fails = match expect {
            Expect::Sample => check_sample(&session),
            Expect::Crash => check_crash(&session),
            Expect::Passing(tests) => check_passing(&session, tests),
        };
        match (&life, matches!(expect, Expect::Crash)) {
            (None, _) => fails.push(format!("atld never started {}", m.name)),
            (Some(l), false) => {
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
        let services_log = std::fs::read_to_string(run.log("services")).unwrap_or_default();
        for l in counter_lines(&services_log) {
            println!("instrument: {label}: counter {l}");
        }
        if fails.is_empty() {
            let pid = life.as_ref().map_or(0, |l| l.pid);
            match expect {
                Expect::Sample => println!(
                    "instrument: sample pass (testPass 1,0; testFail 1,-2; testError 1,-1; final code -1; process {pid} killed by the finish)"
                ),
                Expect::Crash => println!(
                    "instrument: crash pass (process {pid} died mid-run: code 0, shortMsg \"Process crashed.\")"
                ),
                Expect::Passing(tests) => println!(
                    "instrument: {label} pass ({} of {class} 1,0; final code -1; process {pid} killed by the finish)",
                    tests.join(", ")
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
            println!("---- services log ({label})\n{}", tail(&services_log, TAIL));
            println!(
                "---- app log ({label})\n{}",
                tail(
                    &std::fs::read_to_string(run.log(&m.name)).unwrap_or_default(),
                    TAIL
                )
            );
        }
        all_fails.extend(fails.into_iter().map(|f| Failure::step(label, f)));
    }
    let failed = !all_fails.is_empty();
    if failed {
        let verdict = Verdict {
            cmd: "instrument",
            total: None,
            skipped: Vec::new(),
            failures: all_fails,
        };
        println!("{}", verdict.line());
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
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

    /// A run of passing tests: each test's blocks 1 then 0, the final code -1; a failing test, a missing one and a wrong final code each fail it (the probe responds both ways).
    #[test]
    fn passing_check_fires() {
        let pass = session(
            "INSTRUMENTATION_STATUS: test=t1\nINSTRUMENTATION_STATUS_CODE: 1\n\
             INSTRUMENTATION_STATUS: test=t1\nINSTRUMENTATION_STATUS_CODE: 0\n\
             INSTRUMENTATION_CODE: -1\n",
        );
        assert_eq!(check_passing(&pass, &["t1"]), Vec::<String>::new());
        assert_eq!(check_passing(&pass, &["t1", "t2"]).len(), 1);
        let failed = session(
            "INSTRUMENTATION_STATUS: test=t1\nINSTRUMENTATION_STATUS_CODE: 1\n\
             INSTRUMENTATION_STATUS: stack=boom\nINSTRUMENTATION_STATUS: test=t1\nINSTRUMENTATION_STATUS_CODE: -2\n\
             INSTRUMENTATION_CODE: -1\n",
        );
        assert_eq!(check_passing(&failed, &["t1"]).len(), 2);
        assert_eq!(check_passing(&Session::default(), &["t1"]).len(), 2);
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
