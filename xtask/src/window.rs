// SPDX-License-Identifier: MIT
//! `xtask test window`: the window path, one stage at a time. Stage `resume` (M3 plan step 1): the window fixture (xtask/testdata/window: a software activity painting four solid colours)
//! is started the Android way by atld, as `xtask test app` starts its fixture; the services send ResumeActivityItem after LaunchActivityItem (activity.rs), so ActivityThread runs onResume.
//! Pass: the stage's marker is logged, and the app neither logs FATAL EXCEPTION, nor dies, nor waits on a service nothing serves ([`UNSERVED_WAIT`]: a hang shows only as that line) within [`WINDOW_MS`] after it. The app's log lines and the services' counter lines carry the same
//! wall clock (logcat's first field; `at=` on a counter line, atl_services::counters), so every counted call is printed as before or after the marker, with its offset.
//! Always printed: the counters before the marker, the calls after it, the services' crash reports and the app's log after it; on failure also the atld, app and services log tails.
//! `--self-test` checks the verdict on synthetic logs: a crash or a death inside the window fails, one just past it does not, a missing marker and an atld exit fail.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::atld_run::{Counter, Event, MS_PER_S, Run, Runtime, events, parse_stamp, totals};
use crate::manifest;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default <workspace>/out/native/<host>, as `xtask native`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// The stage to run. `resume`: onResume is logged, and the app survives the window after it.
    #[arg(long, value_enum, default_value = "resume")]
    stage: Stage,
    /// Instead of the stage: the verdict on synthetic logs (what the stage must fail and must pass). Needs no device.
    #[arg(long)]
    self_test: bool,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Stage {
    /// The launch is followed by the resume (ResumeActivityItem): onResume is logged.
    Resume,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Resume => "resume",
        }
    }
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/window";
/// The test app's uid: AID_APP_START + 123, any app id (as `xtask test app`).
const APP_UID: u32 = 10_123;
/// How long a cold start may take to reach the marker (as `xtask test app`).
const LIMIT: Duration = Duration::from_secs(180);
/// How long the app must survive after the marker. The resume path (WindowManagerGlobal.addView, the window of the activity) runs right after onResume and fails within
/// milliseconds when it fails, so two seconds leaves the failure time to show.
const WINDOW_MS: u64 = 2_000;
/// How often the logs are read while waiting.
const POLL: Duration = Duration::from_millis(50);
/// Lines shown of a log on failure.
const TAIL: usize = 40;

/// What the stage waits for: the fixture's Log.i line (tag `AtlWindow`, Main.java), as the launcher's logger writes it (`<tag>: <message>`).
fn marker(stage: Stage) -> &'static str {
    match stage {
        Stage::Resume => "AtlWindow: onResume reached",
    }
}

/// The wall clock, ms since the epoch: the clock of the logcat stamps and of the services' `at=` stamps.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `secs.mmm` for `ms` since the epoch: the form of a stamp (parse_stamp reads it back).
fn stamp_text(ms: u64) -> String {
    format!("{}.{:03}", ms / MS_PER_S, ms % MS_PER_S)
}

/// A logcat line's time: its first field.
fn line_stamp(line: &str) -> Option<u64> {
    parse_stamp(line.split_whitespace().next()?)
}

/// onResume's time: the first stamped app-log line that contains the marker.
fn marker_stamp(app_log: &str, marker: &str) -> Option<u64> {
    app_log
        .lines()
        .filter(|l| l.contains(marker))
        .find_map(line_stamp)
}

/// The stage's verdict: `marker_ms` is when onResume was logged (None: never); `fault` is the first thing inside the window after it that fails the stage (None: pass);
/// `frontier` is when the app crashed at [`FRONTIER_FRAME`], which is not a fault yet.
#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    pub marker_ms: Option<u64>,
    pub fault: Option<String>,
    pub frontier: Option<u64>,
}

/// libbinder's line when a service nothing serves holds a call up (IServiceManager.cpp:727, once a second): the app is hung, not crashed, so only this line tells.
const UNSERVED_WAIT: &str = "Waited one second for";

/// The one crash the window path is allowed until the services implement it: ViewRootImpl.setView's `addToDisplayAsUser` fails because IWindowSession is not implemented (its LayoutParams argument is refused,
/// atl-parcelables; plan M3 slice 5 = WMS-lite + the codecs). A crash whose stack holds this frame is the stage's frontier, reported and passed; any other crash fails.
/// Delete this allowance, and the `frontier` handling below, in the change that implements `addToDisplayAsUser`: then the stage's contract (the app survives the window) holds without it.
const FRONTIER_FRAME: &str = "IWindowSession$Stub$Proxy.addToDisplayAsUser";
/// Lines after a `FATAL EXCEPTION` line that its stack can span (the stack, the cause, the log lines in between).
const CRASH_LINES: usize = 80;
/// How long after a crash the process kill it causes is logged (`Process.killProcess` right after the report): the crash's own consequence, not a second fault.
const KILL_AFTER_CRASH_MS: u64 = 500;

/// The verdict on an app log. Inside [marker, marker + WINDOW_MS]: a FATAL EXCEPTION, the process killed (`Sending signal`, Process.killProcess after a crash), a wait on an unserved service ([`UNSERVED_WAIT`]),
/// and `atld_exited` (the services went away before the window ended). A line outside the window, or without a stamp, does not count. A crash at [`FRONTIER_FRAME`], and the kill and the atld exit it causes, count as the frontier instead.
pub fn verdict(app_log: &str, marker: &str, atld_exited: bool) -> Verdict {
    let Some(marker_ms) = marker_stamp(app_log, marker) else {
        return Verdict {
            marker_ms: None,
            fault: None,
            frontier: None,
        };
    };
    let end = marker_ms + WINDOW_MS;
    let lines: Vec<&str> = app_log.lines().collect();
    // when the crash at the frontier was reported: the first FATAL EXCEPTION whose stack holds the frame
    let frontier = lines.iter().enumerate().find_map(|(i, l)| {
        let at = line_stamp(l).filter(|t| (marker_ms..=end).contains(t))?;
        (l.contains("FATAL EXCEPTION")
            && lines[i..lines.len().min(i + CRASH_LINES)]
                .iter()
                .any(|s| s.contains(FRONTIER_FRAME)))
        .then_some(at)
    });
    let consequence = |t: u64| frontier.is_some_and(|f| (f..=f + KILL_AFTER_CRASH_MS).contains(&t));
    let mut faults: Vec<(u64, String)> = lines
        .iter()
        .filter_map(|l| Some((line_stamp(l)?, *l)))
        .filter(|(t, _)| (marker_ms..=end).contains(t))
        .filter_map(|(t, l)| {
            let what = if l.contains("FATAL EXCEPTION") {
                if consequence(t) {
                    return None;
                }
                "FATAL EXCEPTION"
            } else if l.contains("Sending signal") {
                if consequence(t) {
                    return None;
                }
                "the process was killed"
            } else if l.contains(UNSERVED_WAIT) {
                "the app waits on a service nothing serves"
            } else {
                return None;
            };
            Some((t, format!("{what} at +{} ms after onResume", t - marker_ms)))
        })
        .collect();
    if atld_exited && frontier.is_none() {
        faults.push((end, "atld exited inside the window".to_owned()));
    }
    faults.sort_by_key(|(t, _)| *t);
    Verdict {
        marker_ms: Some(marker_ms),
        fault: faults.into_iter().next().map(|(_, f)| f),
        frontier,
    }
}

/// One synthetic app log and what the verdict must say about it.
struct SelfCase {
    name: &'static str,
    log: String,
    atld_exited: bool,
    marked: bool,
    fault: bool,
    /// whether the verdict must report the frontier crash
    frontier: bool,
}

/// A logcat line of the app at `at_ms`, with the fields logcat puts between the stamp and the tag.
fn app_line(at_ms: u64, tag: &str, text: &str) -> String {
    format!("{} 1 2 I {tag}: {text}\n", stamp_text(at_ms))
}

/// The self-test's cases. `T` is onResume's time, so the window is [T, T + WINDOW_MS].
fn self_cases() -> Vec<SelfCase> {
    const T: u64 = 1_000_000;
    let mark = app_line(T, "liblog/AtlWindow", "onResume reached");
    let crash_at = |at: u64| app_line(at, "liblog/AndroidRuntime", "FATAL EXCEPTION: main");
    let case =
        |name: &'static str, log: String, atld_exited: bool, marked: bool, fault: bool| SelfCase {
            name,
            log,
            atld_exited,
            marked,
            fault,
            frontier: false,
        };
    vec![
        case("marked, nothing after", mark.clone(), false, true, false),
        case(
            "crash inside the window",
            mark.clone() + &crash_at(T + 500),
            false,
            true,
            true,
        ),
        case(
            "process killed inside the window",
            mark.clone() + &app_line(T + 100, "liblog/Process", "Sending signal. PID: 2 SIG: 9"),
            false,
            true,
            true,
        ),
        case(
            "unserved-service wait inside the window",
            mark.clone()
                + &app_line(
                    T + 1_000,
                    "libbinder",
                    "Waited one second for SurfaceFlingerAIDL-10123-x (is service started? are binder threads started and available?)",
                ),
            false,
            true,
            true,
        ),
        case(
            "crash just past the window",
            mark.clone() + &crash_at(T + WINDOW_MS + 1),
            false,
            true,
            false,
        ),
        case("crash with no marker", crash_at(T), false, false, false),
        case(
            "atld exits inside the window",
            mark.clone(),
            true,
            true,
            true,
        ),
        // the frontier: the crash of an unimplemented addToDisplayAsUser, its kill and the atld exit that follows are no fault
        SelfCase {
            frontier: true,
            ..case(
                "crash at the frontier",
                mark.clone()
                    + &crash_at(T + 60)
                    + &app_line(
                        T + 60,
                        "liblog/AndroidRuntime",
                        &format!("\tat android.view.{FRONTIER_FRAME}(IWindowSession.java:1274)"),
                    )
                    + &app_line(T + 61, "liblog/Process", "Sending signal. PID: 2 SIG: 9"),
                true,
                true,
                false,
            )
        },
        // the same crash with a different stack is an ordinary crash
        case(
            "crash elsewhere",
            mark + &crash_at(T + 60)
                + &app_line(
                    T + 60,
                    "liblog/AndroidRuntime",
                    "\tat android.view.ViewRootImpl.setView(ViewRootImpl.java:1655)",
                ),
            true,
            true,
            true,
        ),
    ]
}

/// Whether the verdict on a case is the one it expects, and the verdict itself.
fn case_holds(c: &SelfCase) -> (bool, Verdict) {
    let v = verdict(&c.log, marker(Stage::Resume), c.atld_exited);
    let holds = v.marker_ms.is_some() == c.marked
        && v.fault.is_some() == c.fault
        && v.frontier.is_some() == c.frontier;
    (holds, v)
}

/// `--self-test`: every case's verdict is checked; the exit status is nonzero when one differs.
fn self_test() -> ExitCode {
    let mut all = true;
    let mut failures = Vec::new();
    for c in self_cases() {
        let (holds, v) = case_holds(&c);
        all &= holds;
        let said = v.fault.unwrap_or_else(|| {
            if v.marker_ms.is_some() {
                "pass".into()
            } else {
                "no marker".into()
            }
        });
        println!(
            "window: self-test {}: {} ({said})",
            c.name,
            if holds { "pass" } else { "FAIL" }
        );
        if !holds {
            failures.push(crate::verdict::Failure::step(
                c.name,
                format!("the verdict was {said}, which the case does not expect"),
            ));
        }
    }
    if all {
        ExitCode::SUCCESS
    } else {
        let verdict = crate::verdict::Verdict {
            cmd: "window",
            total: None,
            skipped: Vec::new(),
            failures,
        };
        println!("{}", verdict.line());
        ExitCode::FAILURE
    }
}

/// The counters' totals as they stood before `at_ms`.
fn before(all: &[Event], at_ms: u64) -> Vec<Counter> {
    totals(all.iter().filter(|e| e.at_ms < at_ms))
}

fn show(c: &Counter) -> String {
    format!("{} {} x{}", c.kind, c.name, c.count)
}

/// The counters before the marker (their totals), then each counted call from the marker on, with its offset from it.
fn print_calls(all: &[Event], marked: u64) {
    for c in before(all, marked) {
        println!("window: before onResume: {}", show(&c));
    }
    for e in all.iter().filter(|e| e.at_ms >= marked) {
        println!(
            "window: +{} ms after onResume: {} {} (total {})",
            e.at_ms - marked,
            e.counter.kind,
            e.counter.name,
            e.counter.count
        );
    }
}

/// The app's waits on a service nothing serves (libbinder's IServiceManager.cpp:727, "Waited one second for <service>"), from the marker on: the calls those waits hold up.
fn print_waits(app_after: &[&str]) {
    for l in app_after
        .iter()
        .filter(|l| l.contains("Waited one second for"))
    {
        println!("window: app waits, service not served: {l}");
    }
}

/// The services' crash reports (ActivityManager's handleApplicationCrash, activity.rs): logged, not counted.
fn print_crash_reports(services: &str) {
    for l in services.lines().filter(|l| l.contains("crash report from")) {
        println!("window: {l}");
    }
}

/// The app's lines after the first one that contains `marker`.
fn after_marker<'a>(app_log: &'a str, marker: &str) -> Vec<&'a str> {
    app_log
        .lines()
        .skip_while(|l| !l.contains(marker))
        .skip(1)
        .collect()
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.self_test {
        return Ok(self_test());
    }
    let _ = Layout::of(root)?;
    // hwui's GraphicBufferMapper aborts the app without the mapper ("gralloc-mapper is missing"); the window path always has it, as the app and composer gates do
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "window-art",
        args.verbose,
    )?
    .with_gralloc(root)?;
    let work = rt.out.join("window");
    let apk = rt.build_apk(&root.join(FIXTURE), &work.join("apk"), &[])?;
    let m = manifest::read(&rt.aapt2, &apk)?;
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the fixture has no launcher activity")?;
    println!(
        "window: built {} ({} versionCode {}, sdk {}..{}, launcher activity {activity})",
        apk.display(),
        m.name,
        m.version_code,
        m.min_sdk,
        m.target_sdk
    );
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![(apk, m.clone())],
        first_uid: APP_UID,
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    let start_ms = now_ms();
    let started = Instant::now();
    let mut atld_child = rt.start_atld(&run)?;
    let app_log = run.log(&m.name);
    let services_log = run.log("services");
    let marker = marker(args.stage);
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let mut ended = None;
    loop {
        // the window runs from the marker's own stamp, not from when this loop saw the marker
        if marker_stamp(&read(&app_log), marker).is_some_and(|t| now_ms() >= t + WINDOW_MS) {
            break;
        }
        if let Some(s) = atld_child.try_wait()? {
            ended = Some(s);
            break;
        }
        if started.elapsed() > LIMIT {
            break;
        }
        std::thread::sleep(POLL);
    }
    if ended.is_none() {
        let _ = atld_child.kill();
        atld_child.wait()?;
    }
    let app = read(&app_log);
    let services = read(&services_log);
    let all = events(&services);
    let v = verdict(&app, marker, ended.is_some());
    if args.verbose {
        println!("---- app log\n{app}");
    }
    if let (Some(marked), None) = (v.marker_ms, &v.fault) {
        println!(
            "window: pass {} (`{marker}` logged {} ms after the start; no FATAL EXCEPTION, death or unserved-service wait in the {WINDOW_MS} ms after it)",
            args.stage.name(),
            marked.saturating_sub(start_ms)
        );
        if let Some(at) = v.frontier {
            println!(
                "window: frontier: the app crashed {} ms after onResume in {FRONTIER_FRAME}: IWindowSession is not implemented (M3 slice 5); this crash is the only one the stage passes",
                at - marked
            );
        }
        print_calls(&all, marked);
        print_crash_reports(&services);
        let rest = after_marker(&app, marker);
        print_waits(&rest);
        println!(
            "---- app log after the marker (last {TAIL} of {} lines)\n{}",
            rest.len(),
            rest[rest.len().saturating_sub(TAIL)..].join("\n")
        );
        return Ok(ExitCode::SUCCESS);
    }
    let why = match (&v.fault, ended) {
        (Some(fault), _) => fault.clone(),
        (None, Some(s)) => format!("atld exited {s} before onResume"),
        (None, None) => format!("`{marker}` never logged within {LIMIT:?}"),
    };
    println!("window: FAIL {}: {why}", args.stage.name());
    if let Some(marked) = v.marker_ms {
        print_calls(&all, marked);
        print_waits(&after_marker(&app, marker));
    }
    print_crash_reports(&services);
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    println!(
        "---- services log (last {TAIL} lines)\n{}",
        tail(&services, TAIL)
    );
    let verdict = crate::verdict::Verdict {
        cmd: "window",
        total: None,
        skipped: Vec::new(),
        failures: vec![crate::verdict::Failure::step(args.stage.name(), why)],
    };
    println!("{}", verdict.line());
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every self-test case holds: a crash or a death inside the window fails, one just past it passes, a missing marker and an atld exit are as stated.
    #[test]
    fn self_cases_hold() {
        for c in self_cases() {
            let (holds, v) = case_holds(&c);
            assert!(holds, "{}: {v:?}", c.name);
        }
    }

    /// The marker's time is the first stamped line that contains it; an unstamped line is no stamp.
    #[test]
    fn marker_stamp_reads_the_first_stamped_line() {
        let log = "boot line\n1000.001 x marker\n1000.500 x marker\n";
        assert_eq!(marker_stamp(log, "marker"), Some(1_000_001));
        assert_eq!(marker_stamp("no marker here\n", "marker"), None);
    }

    /// The counters before the marker are totals; the calls from it on keep their offsets.
    #[test]
    fn calls_split_at_the_marker() {
        let ev = |at_ms, count| Event {
            at_ms,
            counter: Counter {
                kind: "unimplemented".into(),
                name: "IA.m".into(),
                count,
            },
        };
        let all = [ev(900, 1), ev(1_000, 2), ev(1_004, 3)];
        assert_eq!(
            before(&all, 1_000),
            [Counter {
                kind: "unimplemented".into(),
                name: "IA.m".into(),
                count: 1,
            }]
        );
        let offsets: Vec<u64> = all
            .iter()
            .filter(|e| e.at_ms >= 1_000)
            .map(|e| e.at_ms - 1_000)
            .collect();
        assert_eq!(offsets, [0, 4]);
    }

    /// A stamp is printed in the form parse_stamp reads.
    #[test]
    fn stamp_text_round_trips() {
        assert_eq!(stamp_text(1_793_747_156_733), "1793747156.733");
        assert_eq!(
            parse_stamp(&stamp_text(1_793_747_156_733)),
            Some(1_793_747_156_733)
        );
    }
}
