// SPDX-License-Identifier: MIT
//! `xtask run-apk <APK>`: any APK, started by atld the way an app is (its launcher activity), in UI mode: the app's window is a GTK window of this desktop.
//! Staging is the window gate's: the runtime (Runtime::new, gralloc, the display), the APK copied into the run's work dir, the package file from the APK's real manifest (manifest::read).
//! No probe and no injected input. Pass: the app reports its resume (`services: activity resumed`, sent after onResume returns), no fault in its log within the watch after that
//! (window::fault_after), and at least one frame presented (`atl-composer: ui: present`). Without --hold the run ends once a frame is presented and the watch is over.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::atld_run::{MS_PER_S, Run, Runtime, resumed_ms, run_display};
use crate::{manifest, window};

/// The app process's log line for a presented frame (atl-composer ui_window.rs, debug level: UI mode enables it).
const PRESENT_LINE: &str = "atl-composer: ui: present";
/// The staged runtime's name, shared with the window gate: one ART install for both.
const STAGE: &str = "window-art";

#[derive(clap::Args)]
pub struct Args {
    /// The APK to run, built as an app is: its manifest is the package file.
    apk: PathBuf,
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
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        STAGE,
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_ui("gtk")
    .with_display(run_display().to_arg());
    let rt = if args.no_app_seccomp {
        rt.without_app_seccomp()
    } else {
        rt
    };
    let work = rt.out.join("run-apk");
    std::fs::create_dir_all(&work)?;
    let apk = work.join("base.apk");
    std::fs::copy(&args.apk, &apk).with_context(|| format!("copying {}", args.apk.display()))?;
    let m = manifest::read(&rt.aapt2, &apk)?;
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the APK has no launcher activity")?;
    println!(
        "run-apk: {} ({} versionCode {}, sdk {}..{}, launcher activity {activity})",
        args.apk.display(),
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
    let started = Instant::now();
    let mut atld = rt.start_atld(&run)?;
    let mut ended = None;
    loop {
        let done = resumed_ms(&read(&services_log)).is_some_and(|t| {
            window::now_ms() >= t + watch_ms
                && (hold_ms.is_some() || presented(&read(&app_log)) > 0)
        });
        if done {
            break;
        }
        if let Some(s) = atld.try_wait()? {
            ended = Some(s);
            break;
        }
        if started.elapsed() > window::LIMIT + Duration::from_secs(args.hold.unwrap_or(0)) {
            break;
        }
        std::thread::sleep(window::POLL);
    }
    if ended.is_none() {
        window::stop_apps(atld.id());
        let _ = atld.kill();
        atld.wait()?;
    }
    let app = read(&app_log);
    let services = read(&services_log);
    if args.verbose {
        println!("---- app log\n{app}");
    }
    let outcome = judge(&services, &app, ended.is_some(), watch_ms);
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

/// Frames the app presented: its present lines.
fn presented(app_log: &str) -> usize {
    app_log.lines().filter(|l| l.contains(PRESENT_LINE)).count()
}

/// The run's verdict over its two logs: onResume (the services' resume line), then no fault in the app log within `watch_ms` after it, then a presented frame.
/// Ok: the pass line; Err: the first failed part.
fn judge(services: &str, app: &str, atld_exited: bool, watch_ms: u64) -> Result<String, String> {
    let Some(resumed) = resumed_ms(services) else {
        return Err(
            "no activity resumed: the services log has no `services: activity resumed` line"
                .to_owned(),
        );
    };
    if let Some(fault) = window::fault_after(app, resumed, watch_ms, atld_exited) {
        return Err(fault);
    }
    let frames = presented(app);
    if frames == 0 {
        return Err(format!(
            "no frame presented: the app log has no `{PRESENT_LINE}` line"
        ));
    }
    Ok(format!(
        "onResume at {}, {frames} frame(s) presented, no fault in the {watch_ms} ms after it",
        window::stamp_text(resumed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESUME: &str = "services: activity resumed at=100.000\n";
    const PRESENT: &str = "100.900 D atl-composer: ui: present n=1 time_ns=1\n";

    #[test]
    fn judge_passes_a_resumed_presenting_app() {
        let app = format!("100.500 I tag: onCreate\n{PRESENT}");
        assert!(judge(RESUME, &app, false, 2_000).is_ok());
    }

    #[test]
    fn judge_fails_each_missing_or_broken_part() {
        assert!(
            judge("", PRESENT, false, 2_000)
                .unwrap_err()
                .contains("no activity resumed")
        );
        assert!(
            judge(RESUME, "100.500 I tag: x\n", false, 2_000)
                .unwrap_err()
                .contains("no frame presented")
        );
        let crashed = format!("{PRESENT}100.700 E AndroidRuntime: FATAL EXCEPTION: main\n");
        assert!(
            judge(RESUME, &crashed, false, 2_000)
                .unwrap_err()
                .contains("FATAL EXCEPTION")
        );
        assert!(
            judge(RESUME, PRESENT, true, 2_000)
                .unwrap_err()
                .contains("atld exited")
        );
    }

    #[test]
    fn judge_ignores_a_fault_after_the_watch() {
        let app = format!("{PRESENT}102.500 E AndroidRuntime: FATAL EXCEPTION: main\n");
        assert!(judge(RESUME, &app, false, 2_000).is_ok());
    }
}
