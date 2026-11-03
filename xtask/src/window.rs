// SPDX-License-Identifier: MIT
//! `xtask test window`: the window path, one stage at a time. Stage `resume` (M3 plan step 1, slice 1): the window fixture (xtask/testdata/window: a software activity painting four solid colours)
//! is started the Android way by atld, as `xtask test app` starts its fixture; the services send ResumeActivityItem after LaunchActivityItem (activity.rs), so ActivityThread runs onResume.
//! Pass: the stage's marker in the app's log within [`LIMIT`]. The app then goes on into WindowManager, which no service provides yet (no `window` service is registered), so what it logs after
//! the marker and which service counters rose after it are printed. The counters are sampled when the marker is first seen (every [`POLL`]): a call made within that interval is counted as before.
//! Always printed: the services' counters (atld_run.rs `counter_lines`); on failure also the atld, app and services log tails.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::atld_run::{Counter, Run, Runtime, counters};
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
    /// The stage to run. `resume`: the activity's onResume is logged.
    #[arg(long, value_enum, default_value = "resume")]
    stage: Stage,
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
/// How long a cold start may take (as `xtask test app`).
const LIMIT: Duration = Duration::from_secs(180);
/// How often the app log is read; how long the run goes on after the marker before the counters are read: the window path's next calls, or its failure, come within it.
const POLL: Duration = Duration::from_millis(50);
const AFTER: Duration = Duration::from_secs(10);
/// Lines shown of a log on failure, and of the app log after the marker.
const TAIL: usize = 40;

/// What the stage waits for: the fixture's Log.i line (tag `AtlWindow`, Main.java), as the launcher's logger writes it (`<tag>: <message>`).
fn marker(stage: Stage) -> &'static str {
    match stage {
        Stage::Resume => "AtlWindow: onResume reached",
    }
}

/// The lines of `text` after the first one that contains `marker` (none when no line does).
fn after_marker<'a>(text: &'a str, marker: &str) -> Vec<&'a str> {
    text.lines()
        .skip_while(|l| !l.contains(marker))
        .skip(1)
        .collect()
}

/// The counters that rose from `before` to `after`, per kind and name, each with its rise as the count.
fn rose(before: &[Counter], after: &[Counter]) -> Vec<Counter> {
    after
        .iter()
        .filter_map(|a| {
            let was = before
                .iter()
                .find(|b| b.kind == a.kind && b.name == a.name)
                .map_or(0, |b| b.count);
            (a.count > was).then(|| Counter {
                kind: a.kind.clone(),
                name: a.name.clone(),
                count: a.count - was,
            })
        })
        .collect()
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let _ = Layout::of(root)?;
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "window-art",
        args.verbose,
    )?;
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
    let mut atld_child = rt.start_atld(&run)?;
    let app_log = run.log(&m.name);
    let services_log = run.log("services");
    let marker = marker(args.stage);
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let start = Instant::now();
    // when the marker was first seen, and the services' counters then
    let mut seen: Option<(Instant, Vec<Counter>)> = None;
    let mut ended = None;
    loop {
        if seen.is_none() && read(&app_log).lines().any(|l| l.contains(marker)) {
            seen = Some((Instant::now(), counters(&read(&services_log))));
        }
        if seen.as_ref().is_some_and(|(at, _)| at.elapsed() >= AFTER) {
            break;
        }
        if let Some(s) = atld_child.try_wait()? {
            ended = Some(s);
            break;
        }
        if start.elapsed() > LIMIT {
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
    let reached = app.lines().any(|l| l.contains(marker));
    if args.verbose {
        println!("---- app log\n{app}");
    }
    let after = counters(&services);
    let before = seen.as_ref().map_or_else(Vec::new, |(_, c)| c.clone());
    let rose = rose(&before, &after);
    let show = |cs: &[Counter]| -> Vec<String> {
        cs.iter()
            .map(|c| format!("{} {} x{}", c.kind, c.name, c.count))
            .collect()
    };
    if reached {
        let logged_after = seen.as_ref().map(|(at, _)| at.duration_since(start));
        println!(
            "window: pass {} (`{marker}` logged after {:.1?}; the run went on {AFTER:?} after it)",
            args.stage.name(),
            logged_after.unwrap_or_default()
        );
        for l in show(&before) {
            println!("window: counted before the marker: {l}");
        }
        for l in show(&rose) {
            println!("window: counted after the marker: {l}");
        }
        let after_lines = after_marker(&app, marker);
        println!(
            "---- app log after the marker (last {TAIL} of {} lines)\n{}",
            after_lines.len(),
            after_lines[after_lines.len().saturating_sub(TAIL)..].join("\n")
        );
        return Ok(ExitCode::SUCCESS);
    }
    let why = match ended {
        Some(s) => format!("atld exited {s} before the marker"),
        None if start.elapsed() > LIMIT => format!("no marker after {LIMIT:?}"),
        None => "no marker".to_owned(),
    };
    println!("window: FAIL {}: {why}", args.stage.name());
    for l in show(&after) {
        println!("window: counter {l}");
    }
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    println!(
        "---- services log (last {TAIL} lines)\n{}",
        tail(&services, TAIL)
    );
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter(kind: &str, name: &str, count: u64) -> Counter {
        Counter {
            kind: kind.into(),
            name: name.into(),
            count,
        }
    }

    /// The lines after the marker, marker excluded; nothing when the marker is absent.
    #[test]
    fn after_marker_takes_the_lines_below_it() {
        let text = "a\nAtlWindow: onResume reached\nb\nc\n";
        assert_eq!(
            after_marker(text, "AtlWindow: onResume reached"),
            ["b", "c"]
        );
        assert!(after_marker("a\nb\n", "AtlWindow: onResume reached").is_empty());
    }

    /// A counter's rise is its count now less its count before; a counter not seen before rises by its whole count; one that did not move is not listed.
    #[test]
    fn rose_is_the_difference_per_kind_and_name() {
        let before = [counter("unimplemented", "IWindowManager.openSession", 1)];
        let after = [
            counter("unimplemented", "IWindowManager.openSession", 3),
            counter("unimplemented", "IActivityManager.getProcessLimit", 2),
            counter("refused", "android.os.Bundle", 1),
        ];
        let want = [
            counter("unimplemented", "IWindowManager.openSession", 2),
            counter("unimplemented", "IActivityManager.getProcessLimit", 2),
            counter("refused", "android.os.Bundle", 1),
        ];
        assert_eq!(rose(&before, &after), want);
        assert!(rose(&after, &after).is_empty());
    }
}
