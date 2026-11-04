// SPDX-License-Identifier: MIT
//! `xtask test composer`: the in-app SurfaceFlinger (crate atl-composer) against AOSP's libgui in a real app process (M3 slice 4).
//! atld starts the services and the composer fixture's class (xtask/testdata/composer: `Client`) as an app process (`--run-class`, uid 10123): its launcher starts the process's compositor, which is registered through the system process as
//! `SurfaceFlinger-<uid>-<process>` / `SurfaceFlingerAIDL-<uid>-<process>`, and libgui's lookup finds it by the suffix in the process's property area. The client then does what a window does with the framework's public API:
//! two layers (`createConnection`, `createSurface`), 30 transactions (`setTransactionState`), a transaction-committed callback (the compositor's call into libgui's TransactionCompletedListener) and Choreographer's vsync (`createDisplayEventConnection`, the BitTube).
//! Pass (all checked from the logs, [`evaluate`]): both services registered; every client step logged; the compositor's own counters (its last `composer: counters` line) show every transaction applied, both layers created and gone, every vsync event sent and
//! none dropped, the callback delivered; the vsync cadence is the display model's refresh interval (Choreographer's frame times) and close to it on the wall clock; no FATAL EXCEPTION and no wait on an unserved service.
//! Not covered yet: buffers. BLASTBufferQueue needs a gralloc allocator (M3 slice 2), so the frames-in-the-Scene-sink and every-buffer-released checks come with it.
//! `--self-test` checks [`evaluate`] on synthetic logs: a good one passes, each defect is caught.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use atl_layout::Layout;
use atl_services::display_model::DisplayModel;

use crate::atld_run::{Run, Runtime, Shell, counter_lines};
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
    /// Instead of the run: the verdict on synthetic logs (what the gate must pass and must fail). Needs no device.
    #[arg(long)]
    self_test: bool,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole client log.
    #[arg(long, short)]
    verbose: bool,
}

/// The fixture's dir, its client class and process name.
const FIXTURE: &str = "xtask/testdata/composer";
const CLIENT_CLASS: &str = "com.example.atlcomposer.Client";
const CLIENT_NAME: &str = "composer-client";
/// The client's uid: AID_APP_START + 123, any app id (as `xtask test app`).
const APP_UID: u32 = 10_123;
/// How long the cold start and the client may take (as `xtask test app`).
const LIMIT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(50);
/// Lines shown of a log on failure.
const TAIL: usize = 40;
/// The vsync frames the client observes (Client.java FRAMES): the task asks for 20 or more.
const FRAMES: u64 = 30;
/// Choreographer's mean frame interval may differ from the display's interval by this fraction: its times are the compositor's grid, exact up to the rounding of the interval to a nanosecond and of the mean to a microsecond.
const FRAME_INTERVAL_TOLERANCE: f64 = 0.01;
/// The wall-clock mean may differ by this fraction: sleeping to a deadline on a loaded machine is not exact, and thirty frames average the jitter, not remove it (2026-11-04: set from reading, not measured; tighten once a run shows the band).
const WALL_INTERVAL_TOLERANCE: f64 = 0.25;

/// One checked fact.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        ok,
        detail: detail.into(),
    }
}

/// `key=value` of a `composer: counters` line.
fn field(line: &str, key: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

/// The last `composer: counters` line: the compositor prints one each time its clock goes idle, so the last is the state after everything.
fn last_counters(app: &str) -> Option<&str> {
    app.lines()
        .rev()
        .find(|l| l.contains("composer: counters "))
}

/// A number after `key=` in an `AtlComposer:` client line.
fn client_value(app: &str, key: &str) -> Option<u64> {
    app.lines()
        .filter(|l| l.contains("AtlComposer:"))
        .find_map(|l| field(l, key))
}

/// Whether a client line says `text`.
fn client_says(app: &str, text: &str) -> bool {
    app.lines()
        .any(|l| l.contains("AtlComposer:") && l.contains(text))
}

/// Every check of a run, from its client log (the process's stdout and stderr: liblog records and the compositor's stderr lines) and the services' log.
fn evaluate(app: &str, services: &str, display: &DisplayModel) -> Vec<Check> {
    let mut out = Vec::new();
    let suffix = atl_services::compositor_registry::suffix(APP_UID, CLIENT_NAME);
    let (name, name_aidl) = atl_services::compositor_registry::service_names(&suffix);
    out.push(check(
        "registered",
        services.contains(&format!("services: registered {name}, {name_aidl}")),
        format!("{name}, {name_aidl}"),
    ));
    let steps = [
        ("layers", "layers created valid=true"),
        ("bbq surface", "bbq created surface valid=true"),
        ("vsync complete", "complete=true"),
        ("callback fired", "committed callback fired=true"),
        ("client done", "AtlComposer: done"),
    ];
    for (label, text) in steps {
        out.push(check(label, client_says(app, text), format!("`{text}`")));
    }
    let Some(line) = last_counters(app) else {
        out.push(check(
            "compositor counters",
            false,
            "no `composer: counters` line",
        ));
        return finish(out, app);
    };
    let n = |key: &str| field(line, key);
    out.push(check(
        "transactions applied",
        n("transactions").is_some_and(|t| t >= FRAMES)
            && n("transactions") == n("transactions_applied"),
        format!(
            "transactions={:?} applied={:?}, at least {FRAMES} (one per vsync frame)",
            n("transactions"),
            n("transactions_applied")
        ),
    ));
    out.push(check(
        "layers",
        n("layers_created") == Some(2) && n("layers_alive") == Some(0) && n("unknown_layer") == Some(0),
        format!(
            "created={:?} alive={:?} states for unknown layers={:?}: two created, both released by the client",
            n("layers_created"),
            n("layers_alive"),
            n("unknown_layer")
        ),
    ));
    out.push(check(
        "vsync delivered",
        n("vsync_sent").is_some_and(|s| s >= FRAMES) && n("vsync_dropped") == Some(0),
        format!(
            "sent={:?} dropped={:?}",
            n("vsync_sent"),
            n("vsync_dropped")
        ),
    ));
    out.push(check(
        "callbacks delivered",
        n("callbacks_sent").is_some_and(|s| s >= 1) && n("callbacks_failed") == Some(0),
        format!(
            "sent={:?} failed={:?}",
            n("callbacks_sent"),
            n("callbacks_failed")
        ),
    ));
    // the interval of a refresh rate, in microseconds
    let interval_us = 1e6 / f64::from(display.refresh_hz);
    for (label, key, tolerance) in [
        (
            "frame cadence",
            "mean_frame_interval_us",
            FRAME_INTERVAL_TOLERANCE,
        ),
        (
            "wall cadence",
            "mean_wall_interval_us",
            WALL_INTERVAL_TOLERANCE,
        ),
    ] {
        let got = client_value(app, key);
        let ok = got.is_some_and(|g| ((g as f64) - interval_us).abs() <= interval_us * tolerance);
        out.push(check(
            label,
            ok,
            format!(
                "{key}={got:?}, display {interval_us:.0} us +-{:.0} %",
                tolerance * 100.0
            ),
        ));
    }
    finish(out, app)
}

/// The checks that read the whole log.
fn finish(mut out: Vec<Check>, app: &str) -> Vec<Check> {
    for (label, text) in [
        ("no crash", "FATAL EXCEPTION"),
        ("no unserved-service wait", "Waited one second for"),
    ] {
        let hit = app.lines().find(|l| l.contains(text));
        out.push(check(
            label,
            hit.is_none(),
            hit.unwrap_or("none").to_owned(),
        ));
    }
    out
}

/// A client log of a good run: every step, the counters of a 30-frame run, a cadence of exactly 60 Hz.
fn good_log() -> String {
    "1 1 1 I liblog/AtlComposer: start\n\
     2 1 1 I liblog/AtlComposer: layers created valid=true\n\
     2 1 1 I liblog/AtlComposer: bbq created surface valid=true\n\
     3 1 1 I liblog/AtlComposer: vsync frames=30 complete=true\n\
     4 1 1 I liblog/AtlComposer: vsync mean_frame_interval_us=16666 mean_wall_interval_us=16700\n\
     5 1 1 I liblog/AtlComposer: committed callback fired=true\n\
     composer: counters ticks=34 transactions=30 layers_created=2 layers_alive=0 transactions_applied=30 layer_states=95 unknown_layer=0 buffers_acquired=0 buffers_released=0 completions=1 scenes=3 vsync_sent=34 vsync_dropped=0 callbacks_sent=1 callbacks_failed=0 ignored=\n\
     6 1 1 I liblog/AtlComposer: done\n"
        .to_owned()
}

fn good_services() -> String {
    let suffix = atl_services::compositor_registry::suffix(APP_UID, CLIENT_NAME);
    let (a, b) = atl_services::compositor_registry::service_names(&suffix);
    format!("services: registered {a}, {b}\n")
}

/// One synthetic run and the checks that must fail on it (empty: the run must pass).
struct SelfCase {
    name: &'static str,
    app: String,
    services: String,
    failing: &'static [&'static str],
}

fn self_cases() -> Vec<SelfCase> {
    let good = good_log();
    let case = |name, app: String, services: String, failing| SelfCase {
        name,
        app,
        services,
        failing,
    };
    vec![
        case("good run", good.clone(), good_services(), &[]),
        case(
            "not registered",
            good.clone(),
            String::new(),
            &["registered"],
        ),
        case(
            "a transaction not applied",
            good.replace("transactions_applied=30", "transactions_applied=29"),
            good_services(),
            &["transactions applied"],
        ),
        case(
            "too few transactions",
            good.replace("transactions=30 ", "transactions=29 ")
                .replace("transactions_applied=30", "transactions_applied=29"),
            good_services(),
            &["transactions applied"],
        ),
        case(
            "a layer outlives its handle",
            good.replace("layers_alive=0", "layers_alive=1"),
            good_services(),
            &["layers"],
        ),
        case(
            "vsync dropped",
            good.replace("vsync_dropped=0", "vsync_dropped=2"),
            good_services(),
            &["vsync delivered"],
        ),
        case(
            "callback failed",
            good.replace("callbacks_failed=0", "callbacks_failed=1"),
            good_services(),
            &["callbacks delivered"],
        ),
        case(
            "wrong frame cadence",
            good.replace(
                "mean_frame_interval_us=16666",
                "mean_frame_interval_us=33333",
            ),
            good_services(),
            &["frame cadence"],
        ),
        case(
            "wrong wall cadence",
            good.replace("mean_wall_interval_us=16700", "mean_wall_interval_us=40000"),
            good_services(),
            &["wall cadence"],
        ),
        case(
            "a crash",
            good.clone() + "7 1 1 E AndroidRuntime: FATAL EXCEPTION: main\n",
            good_services(),
            &["no crash"],
        ),
        case(
            "an unserved-service wait",
            good + "7 1 1 W libbinder: Waited one second for SurfaceFlingerAIDL\n",
            good_services(),
            &["no unserved-service wait"],
        ),
        case(
            "no counters line",
            "AtlComposer: done\n".to_owned(),
            good_services(),
            &["compositor counters"],
        ),
        case(
            "no bbq surface",
            good_log().replace("bbq created surface valid=true", "bbq failed: x"),
            good_services(),
            &["bbq surface"],
        ),
    ]
}

/// `--self-test`: each case fails exactly the checks it names.
fn self_test() -> ExitCode {
    let display = DisplayModel::default();
    let mut all = true;
    for c in self_cases() {
        let failed: Vec<&str> = evaluate(&c.app, &c.services, &display)
            .into_iter()
            .filter(|k| !k.ok)
            .map(|k| k.name)
            .collect();
        // a run without counters stops early: only its own check is reported
        let holds = if c.failing == ["compositor counters"] {
            failed.contains(&"compositor counters")
        } else {
            failed == c.failing
        };
        all &= holds;
        println!(
            "composer: self-test {}: {} (failed: {failed:?})",
            c.name,
            if holds { "pass" } else { "FAIL" }
        );
    }
    if all {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
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
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "composer-art",
        args.verbose,
    )?;
    let work = rt.out.join("composer");
    let apk = rt.build_apk(&root.join(FIXTURE), &work.join("apk"), &[])?;
    let m = manifest::read(&rt.aapt2, &apk)?;
    println!(
        "composer: built {} ({} versionCode {})",
        apk.display(),
        m.name,
        m.version_code
    );
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: None,
        packages: vec![(apk.clone(), m)],
        first_uid: APP_UID,
        libraries: vec![],
        shell: Some(Shell {
            class: CLIENT_CLASS.to_owned(),
            uid: APP_UID,
            name: CLIENT_NAME.to_owned(),
            args: vec![],
            boot_jars: vec![apk],
        }),
        log_tags: args.log_tags.clone(),
    };
    let started = Instant::now();
    let mut atld_child = rt.start_atld(&run)?;
    let status = loop {
        if let Some(s) = atld_child.try_wait()? {
            break Some(s);
        }
        if started.elapsed() > LIMIT {
            let _ = atld_child.kill();
            atld_child.wait()?;
            break None;
        }
        std::thread::sleep(POLL);
    };
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let app = read(&run.log(CLIENT_NAME));
    let services = read(&run.log("services"));
    if args.verbose {
        println!("---- client log\n{app}");
    }
    let mut ok = true;
    let exited = status.is_some_and(|s| s.success());
    println!(
        "composer: {} atld {}",
        if exited { "pass" } else { "FAIL" },
        match status {
            Some(s) => format!("exited {s} (the client's status)"),
            None => format!("did not exit within {LIMIT:?}"),
        }
    );
    ok &= exited;
    for c in evaluate(&app, &services, &DisplayModel::default()) {
        println!(
            "composer: {} {}: {}",
            if c.ok { "pass" } else { "FAIL" },
            c.name,
            c.detail
        );
        ok &= c.ok;
    }
    if let Some(l) = last_counters(&app) {
        println!("{l}");
    }
    for l in app
        .lines()
        .filter(|l| l.contains("services: unimplemented"))
    {
        println!("composer: counted {l}");
    }
    for l in counter_lines(&services) {
        println!("composer: services counted {l}");
    }
    if !ok {
        println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
        println!("---- client log (last {TAIL} lines)\n{}", tail(&app, TAIL));
        println!(
            "---- services log (last {TAIL} lines)\n{}",
            tail(&services, TAIL)
        );
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every self-test case fails exactly the checks it names.
    #[test]
    fn self_cases_hold() {
        let display = DisplayModel::default();
        for c in self_cases() {
            let failed: Vec<&str> = evaluate(&c.app, &c.services, &display)
                .into_iter()
                .filter(|k| !k.ok)
                .map(|k| k.name)
                .collect();
            if c.failing == ["compositor counters"] {
                assert!(failed.contains(&"compositor counters"), "{}", c.name);
            } else {
                assert_eq!(failed, c.failing, "{}", c.name);
            }
        }
    }

    /// The counters line's fields are read by exact key: `transactions` is not `transactions_applied`.
    #[test]
    fn field_matches_whole_keys() {
        let line = "composer: counters transactions=30 transactions_applied=29";
        assert_eq!(field(line, "transactions"), Some(30));
        assert_eq!(field(line, "transactions_applied"), Some(29));
        assert_eq!(field(line, "missing"), None);
    }
}
