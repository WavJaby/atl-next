// SPDX-License-Identifier: MIT
//! `xtask test keystore` (plan keystore S1): atld starts AOSP's keystore2 and Rust software KeyMint HAL (`Runtime::with_keystore`); the fixture (xtask-harness/testdata/keystore) runs as a plain app
//! process under its package's app uid and calls them over binder as the framework's KeyStore2 would:
//! - `IKeystoreService.getSecurityLevel(TRUSTED_ENVIRONMENT)` returns a binder (keystore2 connected the VINTF-declared `IKeyMintDevice/default`);
//! - `getKeyEntry` of an absent alias in Domain::APP fails with ResponseCode KEY_NOT_FOUND (keystore2 answers it before any permission check, so it passes without the cfg too);
//! - an EC P-256 signing key generated through that security level (the app's keystore2_key permissions pass: patches/aosp/system/security 0001, cfg `atl_no_selinux`) has a SOFTWARE ORIGIN authorization, the level KeyInfo.getSecurityLevel reports (patches/aosp/hardware/interfaces 0001);
//!   KeyMetadata.keySecurityLevel stays the TEE slot keystore2 called, as on a device; the key deletes; the HAL is reached only through keystore2, as an app on a device (sepolicy keeps apps from finding hal_keymint_service);
//! - `/data/misc/keystore`, keystore2's database, is not in the app's sandbox.
//!
//!
//! App stage (plan keystore S2): AesActivity launched as an app (the launcher installs AndroidKeyStoreProvider as zygote's preload did) generates an AES-256-GCM key, round-trips, reports KeyInfo
//! SOFTWARE / not in secure hardware and StrongBox refused; a second atld run on the same data root decrypts the first's ciphertext. Red controls: `--break-wipe` (keys lost -> "restart key absent");
//! by hand, drop the launcher's install call -> "KeyStoreException: AndroidKeyStore not found".
//!
//! Red control (by hand): drop the providers.toml `atl_no_selinux` rows and rebuild: keystore2's log names "Cannot check permission without calling_sid" and the generate and delete checks fail.
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

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
    /// Red control: empty keystore2's database dir between the two app runs (a device that lost its keys); the restart check must fail with "restart key absent".
    #[arg(long)]
    break_wipe: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/keystore";
const PACKAGE: &str = "com.example.atlkeystore";
/// AesActivity's lines (`AtlKeystoreApp: <check> <value>`), the last one of each launch.
const APP_DONE: &str = "AtlKeystoreApp: done";
/// android.security.keystore.KeyProperties.SECURITY_LEVEL_SOFTWARE.
const KEYINFO_SOFTWARE: i32 = 0;

/// The lines app launch `n` (AesActivity) must print, with what each shows: launch 1 makes the key, launch 2 is a new atld run on the same data root.
fn app_checks(n: usize) -> Vec<(&'static str, String)> {
    if n == 1 {
        vec![
            (
                "AES-256-GCM key generated, encrypt + decrypt round trip",
                "AtlKeystoreApp: roundtrip true".to_owned(),
            ),
            (
                "KeyInfo.getSecurityLevel SOFTWARE",
                format!("AtlKeystoreApp: security-level {KEYINFO_SOFTWARE}"),
            ),
            (
                "KeyInfo.isInsideSecureHardware false",
                "AtlKeystoreApp: inside-secure-hardware false".to_owned(),
            ),
            (
                "setIsStrongBoxBacked -> StrongBoxUnavailableException",
                "AtlKeystoreApp: strongbox unavailable".to_owned(),
            ),
        ]
    } else {
        vec![(
            "after an atld restart the stored key decrypts launch 1's ciphertext",
            "AtlKeystoreApp: restart decrypt true".to_owned(),
        )]
    }
}
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
        (
            "the generated key deletes",
            "AtlKeystore: delete-key ok".to_owned(),
        ),
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
    .with_keystore(root)?
    // an app process's hwui RenderThread initialises EGL at start even with no window (it aborts without a GL driver): the app stage needs both, as the app gate
    .with_gralloc(root)?
    .with_gl_driver(root)?;
    let work = rt.out.join("keystore");
    // the gate's own data root, kept across its three runs (keystore2's database: the restart check) and made fresh at each gate start
    let data_root = work.join("data-root");
    if data_root.exists() {
        std::fs::remove_dir_all(&data_root).with_context(|| data_root.display().to_string())?;
    }
    let rt = rt.with_data_root(&data_root);
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
            boot_jars: vec![apk.clone()],
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
            failed.push(what.to_owned());
        }
    }
    if let Err(e) = &ended {
        println!("keystore: FAIL atld's end: {e:#}");
        failed.push("atld's end".to_owned());
    }
    let mut lines: Vec<String> = log
        .lines()
        .filter(|l| l.contains("AtlKeystore"))
        .map(str::to_owned)
        .collect();
    if let Some(l) = ks_log.lines().find(|l| l.contains("calling_sid")) {
        lines.push(format!("keystore2: {l}"));
    }
    let mut count = checks().len();
    // the app stage: AesActivity in an app process (the launcher's zygote-child start installs the provider), two atld runs on one data root
    for n in [1, 2] {
        if n == 2 && args.break_wipe {
            let db = atl_android_paths::keystore_db_host(&data_root);
            for e in std::fs::read_dir(&db).with_context(|| db.display().to_string())? {
                let p = e?.path();
                std::fs::remove_file(&p).with_context(|| p.display().to_string())?;
            }
            println!("keystore: --break-wipe: emptied {}", db.display());
        }
        let run = Run {
            root: work.join(format!("app{n}")).join("root"),
            scratch: work.join(format!("app{n}")).join("scratch"),
            launch: Some(PACKAGE.to_owned()),
            packages: vec![Package::base(apk.clone())],
            libraries: vec![],
            shell: None,
            log_tags: "*:i".to_owned(),
        };
        let started = Instant::now();
        let mut child = rt.start_atld(&run)?;
        while !read(&run.log(PACKAGE)).contains(APP_DONE)
            && started.elapsed() < LIMIT
            && child.try_wait()?.is_none()
        {
            std::thread::sleep(POLL);
        }
        let end = run.stop(&mut child)?;
        let ended = rt.check_end(end, &run);
        let text = read(&run.log(PACKAGE));
        if args.verbose {
            println!("---- app launch {n} log\n{text}");
        }
        let mut checks = app_checks(n);
        checks.push(("the launch ended its stage", APP_DONE.to_owned()));
        for (what, line) in &checks {
            let ok = text.contains(line.as_str());
            println!(
                "keystore: {} launch {n}: {what}",
                if ok { "pass" } else { "FAIL" }
            );
            if !ok {
                failed.push(format!("launch {n}: {what}"));
            }
        }
        count += checks.len();
        if let Err(e) = &ended {
            println!("keystore: FAIL launch {n} atld's end: {e:#}");
            failed.push(format!("launch {n}: atld's end"));
        }
        lines.extend(
            text.lines()
                .filter(|l| l.contains("AtlKeystoreApp") || l.contains("KeyStoreException"))
                .map(|l| format!("launch {n}: {l}")),
        );
    }
    if failed.is_empty() {
        println!("keystore: pass ({count} checks in {:.1?})", start.elapsed());
        return Ok(ExitCode::SUCCESS);
    }
    println!("keystore: probe lines:\n{}", lines.join("\n"));
    println!("keystore: FAILED: {}", failed.join("; "));
    Ok(ExitCode::FAILURE)
}
