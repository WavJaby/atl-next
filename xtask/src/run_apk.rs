// SPDX-License-Identifier: MIT
//! `xtask run-apk <ROW|PATH>`: a test APK row of test-apks.toml, or an APK file given by path (unpinned), started by atld the way an app is (its launcher activity), in UI mode: the app's window is a GTK window of this desktop with --hold, else of the gate's headless compositor (nested_wayland.rs).
//! Staging is the window gate's: the runtime (Runtime::new, gralloc, the display), the APK copied into the run's work dir, the package file from the APK's real manifest (manifest::read).
//! No probe; injected input only with `--pointer` (a scripted tap sequence the app runs from its first present). The verdict is about the last activity the app resumed (`services: activity resumed <class>`, sent after onResume returns). The run waits until the services
//! are settled (`services: activity settled`: no launch, start or lifecycle step unacknowledged), so a Splash that starts Main is judged on Main. Pass: that resume is the last one, a frame is
//! presented after it (`atl-composer: ui: present`), and no fault in the app log within the watch after it (window::fault_after). The run ends once all of that holds and the watch is over
//! (--hold: the hold); a run that ends before its watch is over fails.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use crate::atld_run::{MS_PER_S, Run, Runtime, last_resumed, parse_stamp, run_display, settled};
use crate::nested_wayland::NestedCompositor;
use crate::{manifest, test_apks, window};

/// The app process's log line for a presented frame (atl-composer ui_window.rs, debug level: UI mode enables it).
const PRESENT_LINE: &str = "atl-composer: ui: present";
/// The staged runtime's name, shared with the window gate: one ART install for both.
const STAGE: &str = "window-art";

#[derive(clap::Args)]
pub struct Args {
    /// A row of test-apks.toml (lowercase letters, digits, `-`): its pinned APK is downloaded and verified into `<workspace>/cache/test-apks` on first use.
    /// Any other argument is a file path (it holds `.`, `/` or `\`), relative to the current directory: that APK runs unpinned, no row checks it.
    #[arg(value_name = "ROW|PATH")]
    target: String,
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Keeps the app up this many seconds after its resume, the window open for drags: no probe, no injected input. Without it the run ends once a frame is presented and the watch is over.
    #[arg(long, value_name = "SECONDS")]
    hold: Option<u64>,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
    /// Deliberate-failure check: apps run without the app seccomp filter (atl-broker seccomp.rs). Without the filter the app's queued-work looper dies with SecurityException.
    #[arg(long)]
    no_app_seccomp: bool,
    /// A pointer script the app injects into its window from its first present (atl-composer `input::parse_script`): `down:X,Y up:X,Y` in the content's physical pixels, `wait:MS` between steps,
    /// e.g. `wait:6000 down:792,28 up:792,28` taps (792, 28) 6 s after the first frame. The verdict is still about the last resumed activity, so wait for each tap's answer to settle.
    #[arg(long, value_name = "SCRIPT")]
    pointer: Option<String>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let (pin, apk_file) = source(root, &args.target)?;
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        STAGE,
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver()?
    .with_ui("gtk");
    let rt = if args.no_app_seccomp {
        rt.without_app_seccomp()
    } else {
        rt
    };
    let rt = match &args.pointer {
        Some(script) => rt.with_headless_pointer(script),
        None => rt,
    };
    let work = rt.out.join("run-apk");
    std::fs::create_dir_all(&work)?;
    // without --hold nobody watches the window: it maps in the gate's own compositor, which outlives atld (declared before it)
    let nested = match args.hold {
        Some(_) => None,
        None => Some(NestedCompositor::start(&work.join("nested"))?),
    };
    // the display at the rate of the compositor the window maps on
    let rt = match &nested {
        Some(n) => rt
            .with_wayland(&n.socket)
            .with_display(n.display().to_arg()),
        None => rt.with_display(run_display().to_arg()),
    };
    let apk = work.join("base.apk");
    std::fs::copy(&apk_file, &apk).with_context(|| format!("copying {}", apk_file.display()))?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    if let Some(row) = &pin {
        test_apks::check_identity(
            &args.target,
            row,
            &m.name,
            m.version_code,
            m.version_name.as_deref(),
        )?;
    }
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the APK has no launcher activity")?;
    println!(
        "run-apk: {} ({} versionCode {}, sdk {}..{}, launcher activity {activity})",
        apk_file.display(),
        m.name,
        m.version_code,
        m.min_sdk,
        m.target_sdk
    );
    if pin.is_none() {
        println!(
            "run-apk: UNPINNED: {} is not a row of {}; no sha256, size or signer check ran",
            apk_file.display(),
            test_apks::FILE
        );
    }
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![(apk, m.clone())],
        first_uid: window::APP_UID,
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    let app_log = run.log(&m.name);
    let services_log = run.log("services");
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let hold_ms = args.hold.map(|s| s * MS_PER_S);
    let watch_ms = hold_ms.unwrap_or(window::WINDOW_MS);
    // a pointer script runs from the first present: the run lasts at least its waits plus the watch, and faults count from the last resume to the run's end
    let script_ms = args.pointer.as_deref().map_or(0, script_wait_ms);
    let started = Instant::now();
    let mut atld = rt.start_atld(&run)?;
    let mut ended = None;
    let mut complete = false;
    loop {
        let services = read(&services_log);
        let scripted = started.elapsed() >= Duration::from_millis(script_ms + watch_ms);
        let done = scripted
            && last_resumed(&services).is_some_and(|r| {
                window::now_ms() >= r.ms + watch_ms
                    && settled(&services)
                    && (hold_ms.is_some() || presented_since(&read(&app_log), r.ms) > 0)
            });
        if done {
            complete = true;
            break;
        }
        if let Some(s) = atld.try_wait()? {
            ended = Some(s);
            break;
        }
        // the app's end is the run's end here, as when atld ended with it
        if run.app_ended() {
            ended = Some(run.stop(&mut atld)?);
            break;
        }
        if started.elapsed() > window::LIMIT + Duration::from_secs(args.hold.unwrap_or(0)) {
            break;
        }
        std::thread::sleep(window::POLL);
    }
    if ended.is_none() {
        window::stop_apps(atld.id());
        run.stop(&mut atld)?;
    }
    let app = read(&app_log);
    let services = read(&services_log);
    if args.verbose {
        println!("---- app log\n{app}");
    }
    let fault_ms = match (args.pointer.is_some(), last_resumed(&services)) {
        (true, Some(r)) => watch_ms.max(window::now_ms().saturating_sub(r.ms)),
        _ => watch_ms,
    };
    let outcome = judge(&services, &app, complete, ended.is_some(), fault_ms);
    match &outcome {
        Ok(s) => println!("run-apk: ok {s}"),
        Err(s) => {
            println!("run-apk: FAILED {s}");
            println!("---- app log tail\n{}", window::tail(&app, window::TAIL));
            println!(
                "---- services log tail\n{}",
                window::tail(&services, window::TAIL)
            );
        }
    }
    println!("run-apk: logs {}", run.root.join("log").display());
    Ok(if outcome.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The APK to run: a row's pinned file (verified by `test_apks::ensure`), or the file a path names, which no row pins.
fn source(root: &Path, target: &str) -> Result<(Option<test_apks::Row>, PathBuf)> {
    if test_apks::names_row(target) {
        let (row, file) = test_apks::ensure(root, target)?;
        return Ok((Some(row), file));
    }
    let file = PathBuf::from(target);
    ensure!(
        file.is_file(),
        "{target}: not a file (an argument holding `.`, `/` or `\\` is a path)"
    );
    Ok((None, file))
}

/// The script's `wait:MS` items summed: how long it runs after the first present (a bad item is the app's to refuse, counted 0 here).
fn script_wait_ms(script: &str) -> u64 {
    script
        .split_whitespace()
        .filter_map(|item| item.strip_prefix("wait:")?.parse::<u64>().ok())
        .sum()
}

/// Frames the app presented at or after `from_ms`: its present lines stamped no earlier (a line's stamp is its first field).
fn presented_since(app_log: &str, from_ms: u64) -> usize {
    app_log
        .lines()
        .filter(|l| l.contains(PRESENT_LINE))
        .filter(|l| {
            l.split_whitespace()
                .next()
                .and_then(parse_stamp)
                .is_some_and(|t| t >= from_ms)
        })
        .count()
}

/// The run's verdict over its two logs, about the last activity the app resumed: the services are settled (nothing pending after that resume), no fault in the app log within
/// `watch_ms` after it (`atld_exited` is a fault at the window's end), the watch was run to its end (`complete`), and a frame was presented after it.
/// Ok: the pass line; Err: the first failed part.
fn judge(
    services: &str,
    app: &str,
    complete: bool,
    atld_exited: bool,
    watch_ms: u64,
) -> Result<String, String> {
    let Some(last) = last_resumed(services) else {
        return Err(
            "no activity resumed: the services log has no `services: activity resumed` line"
                .to_owned(),
        );
    };
    if !settled(services) {
        return Err(format!(
            "not settled after {}: a start or lifecycle step is still unacknowledged",
            last.class
        ));
    }
    if let Some(fault) = window::fault_after(app, last.ms, watch_ms, last.ms, atld_exited) {
        return Err(format!("{fault} (last resumed: {})", last.class));
    }
    if !complete {
        return Err(format!(
            "the run ended before the {watch_ms} ms watch after {} was over",
            last.class
        ));
    }
    let frames = presented_since(app, last.ms);
    if frames == 0 {
        return Err(format!(
            "no frame presented after {} resumed: the app log has no `{PRESENT_LINE}` line after it",
            last.class
        ));
    }
    Ok(format!(
        "onResume of {} at {}, {frames} frame(s) presented after it, settled, no fault in the {watch_ms} ms after it",
        last.class,
        window::stamp_text(last.ms)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESUME: &str =
        "services: activity resumed org.x.Main at=100.000\nservices: activity settled at=100.001\n";
    const PRESENT: &str = "100.900 D atl-composer: ui: present n=1 time_ns=1\n";

    #[test]
    fn judge_passes_a_resumed_presenting_app() {
        let app = format!("100.500 I tag: onCreate\n{PRESENT}");
        assert!(judge(RESUME, &app, true, false, 2_000).is_ok());
    }

    #[test]
    fn judge_fails_each_missing_or_broken_part() {
        let err = |services: &str, app: &str, complete: bool, atld_exited: bool| {
            judge(services, app, complete, atld_exited, 2_000).unwrap_err()
        };
        assert!(err("", PRESENT, true, false).contains("no activity resumed"));
        assert!(err(RESUME, "100.500 I tag: x\n", true, false).contains("no frame presented"));
        let crashed = format!("{PRESENT}100.700 E AndroidRuntime: FATAL EXCEPTION: main\n");
        assert!(err(RESUME, &crashed, true, false).contains("FATAL EXCEPTION"));
        assert!(err(RESUME, PRESENT, true, true).contains("atld exited"));
        assert!(err(RESUME, PRESENT, false, false).contains("ended before"));
        let pending = "services: activity resumed org.x.Main at=100.000\nservices: activity unsettled at=100.500\n";
        assert!(err(pending, PRESENT, true, false).contains("not settled"));
    }

    /// The shape of the r5 hold run: Splash resumes and presents, Main resumes 39 s later and dies 67 ms after that. Splash's watch is long over by then; the verdict is about Main.
    #[test]
    fn judge_checks_the_last_resumed_activity_of_the_hold_run() {
        let services = "services: activity resumed org.fossify.math.activities.SplashActivity at=1793887559.475\n\
                        services: activity unsettled at=1793887559.600\n\
                        services: activity resumed org.fossify.math.activities.MainActivity at=1793887598.643\n\
                        services: activity settled at=1793887598.644\n";
        let app = "1793887559.900 D atl-composer: ui: present n=1 time_ns=1\n\
                   1793887598.710 E AndroidRuntime: FATAL EXCEPTION: main\n";
        let err = judge(services, app, true, false, 2_000).unwrap_err();
        assert!(
            err.contains("FATAL EXCEPTION") && err.contains("MainActivity"),
            "{err}"
        );
    }

    /// A second activity that starts after the first watch and presents nothing is not a pass: the verdict is about it, and it has no frame.
    #[test]
    fn judge_refuses_a_second_activity_without_a_frame() {
        let services = "services: activity resumed org.x.Splash at=100.000\n\
                        services: activity unsettled at=103.000\n\
                        services: activity resumed org.x.Main at=103.500\n\
                        services: activity settled at=103.501\n";
        let err = judge(services, PRESENT, true, false, 2_000).unwrap_err();
        assert!(err.contains("no frame presented after org.x.Main"), "{err}");
    }

    #[test]
    fn script_waits_are_summed() {
        assert_eq!(script_wait_ms("wait:6000 down:1,2 up:1,2 wait:2500"), 8500);
        assert_eq!(script_wait_ms("down:1,2 wait:x"), 0);
    }

    #[test]
    fn judge_ignores_a_fault_after_the_watch() {
        let app = format!("{PRESENT}102.500 E AndroidRuntime: FATAL EXCEPTION: main\n");
        assert!(judge(RESUME, &app, true, false, 2_000).is_ok());
    }
}
