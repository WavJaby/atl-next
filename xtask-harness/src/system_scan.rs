// SPDX-License-Identifier: MIT
//! `xtask test system-scan`: a preinstalled package (plan system-packages SP2). Fixture xtask-harness/testdata/system-scan, placed by atld `--file` at `/product/app/AtlSystemScan/AtlSystemScan.apk`
//! and launched; checks are lines of its log (tag AtlSystemScan) and of the services' log:
//! - the services' scan accepted one system package (`scan: 1 system packages, 0 skipped`);
//! - PackageManager's ApplicationInfo of it has FLAG_SYSTEM and its sourceDir is the /product path (the parse's own name, not `base.apk`);
//! - its sandbox reads that file (the product partition is bound into every app) and a write there fails EROFS (bound read-only).
//!
//! `--break-as-session` installs the same APK as an install session instead (not placed): the FLAG_SYSTEM, sourceDir and EROFS checks must fail.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime};

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
    /// Install the fixture as a session instead of placing it in /product (deliberate break): the system, sourceDir and EROFS checks must fail.
    #[arg(long)]
    break_as_session: bool,
    /// Print the app's whole log.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/system-scan";
const PACKAGE: &str = "com.example.atlsystemscan";
/// Soong's install dir name of the module (`<partition>/app/<Name>/<Name>.apk`, build/soong/java/app.go installPath).
const MODULE: &str = "AtlSystemScan";
/// A cold start (ART without a boot image interprets framework.jar's class init), as `xtask test app`'s limit.
const LIMIT: Duration = Duration::from_secs(180);
const TAIL: usize = 30;

const SCANNED: &str = "scan: 1 system packages, 0 skipped";
const READ: &str = "AtlSystemScan: read zip";
const EROFS: &str = "AtlSystemScan: write EROFS";
const DONE: &str = "AtlSystemScan: done";

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let _ = Layout::of(root)?;
    let rt = Runtime::new(
        root,
        "system-scan",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "system-scan-art",
        args.verbose,
    )?;
    let work = rt.out.join("system-scan");
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("app"), &[], None, None)?;
    let at = format!("{}/app/{MODULE}/{MODULE}.apk", atl_android_paths::PRODUCT);
    let (rt, packages) = if args.break_as_session {
        (rt, vec![Package::base(apk)])
    } else {
        (rt.with_fixture_file(apk, &at), vec![])
    };
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(PACKAGE.to_owned()),
        packages,
        libraries: vec![],
        shell: None,
        log_tags: "*:i".into(),
    };
    let mut atld = rt.start_atld(&run)?;
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let log = run.log(PACKAGE);
    let start = Instant::now();
    let ended = loop {
        if read(&log).contains(DONE) || start.elapsed() > LIMIT {
            break None;
        }
        if let Some(s) = atld.try_wait()? {
            break Some(s);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if ended.is_none() {
        let _ = atld.kill();
        atld.wait()?;
    }
    let text = read(&log);
    let services = read(&run.log("services"));
    if args.verbose {
        println!("---- app log\n{text}");
    }
    let identity = format!("AtlSystemScan: system true source {at}");
    let checks = [
        (
            "the services scanned one system package",
            args.break_as_session || services.contains(SCANNED),
        ),
        (
            "PackageManager: FLAG_SYSTEM, sourceDir in /product",
            text.contains(&identity),
        ),
        ("the app sandbox reads the APK", text.contains(READ)),
        ("a write there fails EROFS", text.contains(EROFS)),
    ];
    let mut whys = Vec::new();
    for (what, ok) in checks {
        println!("system-scan: {} {what}", if ok { "pass" } else { "FAIL" });
        if !ok {
            whys.push(what.to_owned());
        }
    }
    if whys.is_empty() {
        println!(
            "system-scan: pass ({} checks in {:.1?})",
            checks.len(),
            start.elapsed()
        );
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(e) = ended {
        whys.push(format!("atld exited {e}"));
    }
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL})\n{}", tail(&text, TAIL));
    println!("---- services log (last {TAIL})\n{}", tail(&services, TAIL));
    println!("{}", crate::verdict::gate("system-scan", &whys));
    Ok(ExitCode::FAILURE)
}
