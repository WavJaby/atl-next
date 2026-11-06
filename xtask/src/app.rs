// SPDX-License-Identifier: MIT
//! `xtask test app`: the M2 gate. The test APK (xtask/testdata/app: a manifest and one Activity whose onCreate logs [`MARKER`]) is started the Android way by atld: atl-binderd,
//! AOSP servicemanager, the M2 services (atl-system-services), then the services start the app through atld's process control (the broker's sandbox); ActivityThread attaches, is bound, launches the activity.
//! The APK is built here as an app is built (atld_run.rs); the package record atld installs is read back from it (`aapt2 dump xmltree`, manifest.rs), not restated.
//! Every run has the graphics allocator HAL (it must register with servicemanager, the VINTF-declared way) and the GL driver (`Runtime::with_gl_driver`).
//! Pass: [`MARKER`] in the app's log within [`LIMIT`]. Always printed: the services' counters (unimplemented calls and refused parcelables, atld_run.rs `counter_lines`); on failure also the atld, app and services log tails.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::atld_run::{Run, Runtime, counter_lines};
use crate::window::{self, WINDOW_MS};
use crate::{jni_fixture, manifest};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, d8, aapt2 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/app";
/// What Main.onCreate logs (tag `AtlTest`), as the launcher's logger writes it (`<tag>: <message>`).
const MARKER: &str = "AtlTest: onCreate reached";
/// Main.onCreate's CA line before [`MARKER`]: the default TrustManagerFactory trusts a distro root (atld's /system CA store, cacerts.rs); `cacerts: trusted ...` passes, `cacerts: FAIL ...` says why not.
const CACERTS: &str = "AtlTest: cacerts: ";
/// What Main.onCreate logs before MARKER when the bridge-1 fixture did not load; the gate wants it to carry the NativeBridge core's refusal (`atl_native_bridge::LOAD_REFUSED` + the library's path): proof that libnativeloader routed the library to our `loadLibrary`.
const BRIDGE1: &str = "AtlTest: bridge1 not loaded: java.lang.UnsatisfiedLinkError";
/// The fixture's file name in the APK (jni_fixture.rs LIB), which the refusal names.
const BRIDGE1_LIB: &str = "libatl-jni-test.so";
/// The test app's uid: AID_APP_START + 123, any app id.
const APP_UID: u32 = 10_123;
/// How long a cold start may take: ART without a boot image interprets framework.jar's class initialisation (the parcel stage's runs take seconds); 180 s for a loaded machine.
const LIMIT: Duration = Duration::from_secs(180);
/// App log lines shown on failure.
const TAIL: usize = 40;
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
        "app-art",
        args.verbose,
    )?
    // the activity's window renders through hwui, which needs EGL: the allocator and the GL driver are part of every run (needs a DRM device that allocates, WSL: vgem)
    .with_gralloc(root)?
    .with_gl_driver()?;
    let work = rt.out.join("app");
    // the bridge-1 JNI fixture rides along in lib/<abi>/, unloaded until bridge 1 loads it (jni_fixture.rs)
    let native = jni_fixture::apk_libs(root, &args.host, &work.join("jni-fixture"))?;
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("apk"), &[], Some(&native))?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the fixture has no launcher activity")?;
    println!(
        "app: built {} ({} versionCode {}, sdk {}..{}, launcher activity {})",
        apk.display(),
        m.name,
        m.version_code,
        m.min_sdk,
        m.target_sdk,
        activity
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
    let start = Instant::now();
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let (passed, ended) = loop {
        if read(&app_log).lines().any(|l| l.contains(MARKER)) {
            break (true, None);
        }
        if let Some(s) = atld_child.try_wait()? {
            break (read(&app_log).lines().any(|l| l.contains(MARKER)), Some(s));
        }
        if start.elapsed() > LIMIT {
            break (false, None);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let elapsed = start.elapsed();
    // the watch after the marker: a crash shows within it (window.rs WINDOW_MS); atld exits when the app's main process dies
    let mut ended = ended;
    if passed && ended.is_none() {
        let watch = Instant::now();
        while ended.is_none() && watch.elapsed() < Duration::from_millis(WINDOW_MS) {
            ended = atld_child.try_wait()?;
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if ended.is_none() {
        let _ = atld_child.kill();
        atld_child.wait()?;
    }
    let app = read(&app_log);
    let fault = fault(&app, ended.is_some());
    let services_log = read(&run.log("services"));
    if args.verbose {
        println!("---- app log\n{app}");
    }
    for l in counter_lines(&services_log) {
        println!("app: counter {l}");
    }
    // atld waits for the allocator's ready line before it starts the app, so a started app means a registered allocator; the line is checked anyway, with the allocator's own log as the explanation
    let gralloc_log = read(&run.log("gralloc"));
    let gralloc_ok = gralloc_log.contains("gralloc: ready");
    let cacerts = app
        .lines()
        .find_map(|l| l.split_once(CACERTS).map(|(_, r)| r.to_owned()))
        .unwrap_or_else(|| "FAIL no cacerts line".to_owned());
    let bridge1 = app.lines().find(|l| l.contains(BRIDGE1));
    let bridge1_ok = bridge1
        .is_some_and(|l| l.contains(atl_native_bridge::LOAD_REFUSED) && l.contains(BRIDGE1_LIB));
    let mut whys = Vec::new();
    if let Some(f) = &fault {
        whys.push(format!("the app faulted: {f}"));
        println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    }
    if passed && !cacerts.starts_with("trusted ") {
        whys.push(format!("CA store: {cacerts}"));
    }
    if passed && !bridge1_ok {
        whys.push(format!(
            "bridge1: want UnsatisfiedLinkError with the NativeBridge refusal for {BRIDGE1_LIB}, got {:?}",
            bridge1.or_else(|| app.lines().find(|l| l.contains("AtlTest: bridge1")))
        ));
    }
    if !whys.is_empty() {
        for why in &whys {
            println!("app: FAIL {why}");
        }
        println!("{}", crate::verdict::gate("app", &whys));
        return Ok(ExitCode::FAILURE);
    }
    if passed {
        println!(
            "app: pass bridge1 routed: {}",
            bridge1.unwrap_or_default().trim()
        );
    }
    if passed && gralloc_ok {
        println!("app: cacerts {cacerts}");
        println!("app: pass ({activity} Activity.onCreate logged after {elapsed:.1?})");
        println!("app: gralloc registered: {}", gralloc_log.trim());
        return Ok(ExitCode::SUCCESS);
    }
    if passed {
        println!(
            "app: FAIL the allocator did not register:\n{}",
            gralloc_log.trim()
        );
        let why = format!("the allocator did not register: {}", gralloc_log.trim());
        println!(
            "{}",
            crate::verdict::gate("app", std::slice::from_ref(&why))
        );
        return Ok(ExitCode::FAILURE);
    }
    let why = match ended {
        Some(s) => format!("atld exited {s} before the marker"),
        None if elapsed > LIMIT => format!("no marker after {LIMIT:?}"),
        None => "no marker".to_owned(),
    };
    println!("app: FAIL {why}");
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    println!(
        "---- services log (last {TAIL} lines)\n{}",
        tail(&services_log, TAIL)
    );
    println!(
        "{}",
        crate::verdict::gate("app", std::slice::from_ref(&why))
    );
    Ok(ExitCode::FAILURE)
}

/// The app's first fault up to the watch's end (marker + WINDOW_MS), from the log's start: the app's other threads run before onCreate (hwui's RenderThread made its EGL context 50 ms before the marker when it aborted, round-4 run). None without a marker.
fn fault(app_log: &str, atld_exited: bool) -> Option<String> {
    let marker_ms = window::verdict(app_log, MARKER, atld_exited, WINDOW_MS).marker_ms?;
    window::fault_after(app_log, 0, marker_ms + WINDOW_MS, marker_ms, atld_exited)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window::stamp_text;

    const T: u64 = 1_000_000;

    fn line(at: u64, prio: &str, text: &str) -> String {
        format!(
            "{} 34636.4 2 14 {prio} liblog/Tag: {text}\n",
            stamp_text(at)
        )
    }

    #[test]
    fn clean_run_has_no_fault() {
        assert_eq!(fault(&line(T, "I", MARKER), false), None);
    }

    #[test]
    fn abort_before_the_marker_is_a_fault() {
        // the round-4 shape: RenderThread's F line, then the main thread's marker
        let log = line(
            T - 50,
            "F",
            "HWUI: Failed to create context, error = EGL_BAD_ATTRIBUTE",
        ) + &line(T, "I", MARKER);
        assert!(fault(&log, false).is_some_and(|f| f.contains("fatal log line")));
    }

    #[test]
    fn death_inside_the_watch_is_a_fault() {
        assert!(fault(&line(T, "I", MARKER), true).is_some());
        let crash = line(T, "I", MARKER) + &line(T + 100, "E", "FATAL EXCEPTION: main");
        assert!(fault(&crash, false).is_some());
    }
}
