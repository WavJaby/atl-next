// SPDX-License-Identifier: MIT
//! `xtask test keystore` (plan keystore S1): atld starts AOSP's keystore2 and Rust software KeyMint HAL (`Runtime::with_keystore`); the fixture (xtask-harness/testdata/keystore) runs as a plain app
//! process under its package's app uid and calls them over binder as the framework's KeyStore2 would:
//! - `IKeystoreService.getSecurityLevel(TRUSTED_ENVIRONMENT)` returns a binder (keystore2 connected the VINTF-declared `IKeyMintDevice/default`);
//! - `getKeyEntry` of an absent alias in Domain::APP fails with ResponseCode KEY_NOT_FOUND (keystore2 answers it before any permission check, so it passes without the cfg too);
//! - an EC P-256 signing key generated through that security level (the app's keystore2_key permissions pass: patches/aosp/system/security 0001, cfg `atl_no_selinux`) has a SOFTWARE ORIGIN authorization, the level KeyInfo.getSecurityLevel reports (patches/aosp/hardware/interfaces 0001);
//!   KeyMetadata.keySecurityLevel stays the TEE slot keystore2 called, as on a device; the key deletes; the HAL is reached only through keystore2, as an app on a device (sepolicy keeps apps from finding hal_keymint_service);
//! - `/data/misc/keystore`, keystore2's database, is not in the app's sandbox.
//!
//! Red control (by hand): drop the providers.toml `atl_no_selinux` rows and rebuild: keystore2's log names "Cannot check permission without calling_sid" and the generate and delete checks fail.
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::atld_run::{Package, Run, Runtime, Shell};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager, the services and keystore2 (default: this checkout's build dir, see `cargo xtask out show`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, d8, aapt2 (default: this checkout's build dir).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Print the probe's and keystore2's logs.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/keystore";
const PROBE_CLASS: &str = "com.example.atlkeystore.Probe";
const PROBE_NAME: &str = "keystore-probe";
/// A cold start (ART without a boot image interprets framework.jar's class init), as `xtask test gl`'s limit.
const LIMIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(100);

/// android.system.keystore2.ResponseCode.KEY_NOT_FOUND (ResponseCode.aidl).
const KEY_NOT_FOUND: i32 = 7;
/// android.hardware.security.keymint.SecurityLevel.SOFTWARE (SecurityLevel.aidl).
const SOFTWARE: i32 = 0;

/// The lines the probe must print (Probe.java), each with what it shows.
fn checks() -> [(&'static str, String); 6] {
    [
        ("keystore found", "AtlKeystore: keystore found".to_owned()),
        (
            "getSecurityLevel(TRUSTED_ENVIRONMENT) answers a binder",
            "AtlKeystore: security-level binder".to_owned(),
        ),
        (
            "getKeyEntry of an absent alias: KEY_NOT_FOUND",
            format!("AtlKeystore: get-key-entry error {KEY_NOT_FOUND}"),
        ),
        (
            "an EC key generated through keystore2: ORIGIN authorization SOFTWARE",
            format!("AtlKeystore: origin-level {SOFTWARE}"),
        ),
        ("the generated key deletes", "AtlKeystore: delete-key ok".to_owned()),
        (
            "keystore2's database is outside the app sandbox",
            "AtlKeystore: keystore-dir absent".to_owned(),
        ),
    ]
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let rt = Runtime::new(
        root,
        "keystore",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "keystore-art",
        args.verbose,
    )?
    .with_keystore(root)?;
    let work = rt.out.join("keystore");
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("apk"), &[], None, None)?;
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: None,
        packages: vec![Package::base(apk.clone())],
        libraries: vec![],
        shell: Some(Shell {
            class: PROBE_CLASS.to_owned(),
            // an app: the run's only package's uid
            uid: rt.first_app_uid(),
            name: PROBE_NAME.to_owned(),
            args: vec![],
            boot_jars: vec![apk],
        }),
        log_tags: "*:i".to_owned(),
    };
    let start = Instant::now();
    let mut child = rt.start_atld(&run)?;
    while child.try_wait()?.is_none() {
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(POLL);
    }
    let end = child.wait()?;
    let ended = rt.check_end(end, &run);
    let read = atl_diag::logfiles::read;
    let log = read(&run.log(PROBE_NAME));
    let ks_log = read(&run.log("keystore2"));
    if args.verbose {
        println!("---- probe log\n{log}\n---- keystore2 log\n{ks_log}");
    }
    let mut failed = Vec::new();
    for (what, line) in checks() {
        let ok = log.contains(&line);
        println!("keystore: {} {what}", if ok { "pass" } else { "FAIL" });
        if !ok {
            failed.push(what);
        }
    }
    if let Err(e) = &ended {
        println!("keystore: FAIL atld's end: {e:#}");
        failed.push("atld's end");
    }
    if failed.is_empty() {
        println!(
            "keystore: pass ({} checks in {:.1?})",
            checks().len(),
            start.elapsed()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let probe: Vec<&str> = log.lines().filter(|l| l.contains("AtlKeystore")).collect();
    println!("keystore: probe lines:\n{}", probe.join("\n"));
    if let Some(l) = ks_log.lines().find(|l| l.contains("calling_sid")) {
        println!("keystore: keystore2: {l}");
    }
    println!("keystore: FAILED: {}", failed.join("; "));
    Ok(ExitCode::FAILURE)
}
