// SPDX-License-Identifier: MIT
//! `xtask test app`: the M2 gate. The test APK (xtask/testdata/app: a manifest and one Activity whose onCreate logs [`MARKER`]) is started the Android way by atld: atl-binderd,
//! AOSP servicemanager, the M2 services (atl-system-services), then the services start the app through atld's process control (the broker's sandbox); ActivityThread attaches, is bound, launches the activity.
//! The APK is built here as an app is built (atld_run.rs); the package record atld installs is read back from it (`aapt2 dump xmltree`, manifest.rs), not restated.
//! Pass: [`MARKER`] in the app's log within [`LIMIT`]. Always printed: the services' counters (unimplemented calls and refused parcelables, atld_run.rs `counter_lines`); on failure also the atld, app and services log tails.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use atl_layout::Layout;

use crate::atld_run::{Run, Runtime, counter_lines};
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
    /// Also start the graphics allocator HAL under atld (needs a DRM device that allocates, WSL: vgem; and `xtask native` with the gralloc crates built): the run must register it with servicemanager, the VINTF-declared way, and the app must still reach onCreate with `mapper.atl.so` on its library path.
    #[arg(long)]
    gralloc: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/app";
/// What Main.onCreate logs (tag `AtlTest`), as the launcher's logger writes it (`<tag>: <message>`).
const MARKER: &str = "AtlTest: onCreate reached";
/// Main.onCreate's CA line before [`MARKER`]: the default TrustManagerFactory trusts a distro root (atld's /system CA store, cacerts.rs); `cacerts: trusted ...` passes, `cacerts: FAIL ...` says why not.
const CACERTS: &str = "AtlTest: cacerts: ";
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
    )?;
    let rt = if args.gralloc {
        rt.with_gralloc(root)?
    } else {
        rt
    };
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
    if ended.is_none() {
        let _ = atld_child.kill();
        atld_child.wait()?;
    }
    let app = read(&app_log);
    let services_log = read(&run.log("services"));
    if args.verbose {
        println!("---- app log\n{app}");
    }
    for l in counter_lines(&services_log) {
        println!("app: counter {l}");
    }
    // atld waits for the allocator's ready line before it starts the app, so a started app means a registered allocator; the line is checked anyway, with the allocator's own log as the explanation
    let gralloc_log = read(&run.log("gralloc"));
    let gralloc_ok = !args.gralloc || gralloc_log.contains("gralloc: ready");
    let cacerts = app
        .lines()
        .find_map(|l| l.split_once(CACERTS).map(|(_, r)| r.to_owned()))
        .unwrap_or_else(|| "FAIL no cacerts line".to_owned());
    if passed && !cacerts.starts_with("trusted ") {
        let why = format!("CA store: {cacerts}");
        println!("app: FAIL {why}");
        println!(
            "{}",
            crate::verdict::gate("app", std::slice::from_ref(&why))
        );
        return Ok(ExitCode::FAILURE);
    }
    if passed && gralloc_ok {
        println!("app: cacerts {cacerts}");
        println!("app: pass ({activity} Activity.onCreate logged after {elapsed:.1?})");
        if args.gralloc {
            println!("app: gralloc registered: {}", gralloc_log.trim());
        }
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
