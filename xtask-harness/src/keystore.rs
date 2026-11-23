// SPDX-License-Identifier: MIT
//! `xtask test keystore` (plan keystore S1): atld starts AOSP's keystore2 and Rust software KeyMint HAL (`Runtime::with_keystore`); the fixture (xtask-harness/testdata/keystore) runs as a plain app
//! process under its package's app uid and calls them over binder as the framework's KeyStore2 would:
//! - `IKeystoreService.getSecurityLevel(TRUSTED_ENVIRONMENT)` returns a binder (keystore2 connected the VINTF-declared `IKeyMintDevice/default`);
//! - `getKeyEntry` of an absent alias in Domain::APP fails with ResponseCode KEY_NOT_FOUND (keystore2 answers it before any permission check, so it passes without the cfg too);
//! - an EC P-256 signing key generated through that security level (the app's keystore2_key permissions pass: patches/aosp/system/security 0001, cfg `atl_no_selinux`) has a SOFTWARE ORIGIN authorization, the level KeyInfo.getSecurityLevel reports (patches/aosp/hardware/interfaces 0001);
//!   KeyMetadata.keySecurityLevel stays the TEE slot keystore2 called, as on a device; the key deletes; the HAL is reached only through keystore2, as an app on a device (sepolicy keeps apps from finding hal_keymint_service);
//! - `/data/misc/keystore`, keystore2's database, is not in the app's sandbox;
//! - servicemanager decides service access from the compiled pinned policy (service-access plan S1): the app cannot find the KeyMint HAL (checkService / getService null, isDeclared
//!   SecurityException) but finds `activity` and the allocator HAL, listServices names everything, and each denial is one avc line in servicemanager's log that atl-diag counts as `denied`.
//!   Red control `--break-allow-all`: the image's service access table grants everything, so the HAL checks fail.
//! - the KeyMint HAL's sources carry no copy of the Cuttlefish IMEI its attestation IDs used to hold (plan keystore S3, D-K1; a scan of the sources, not the executable).
//! - a key generated with an attestation challenge gets AOSP's software attestation chain (plan keystore S3b, D-K1 (a)): keystore2 asks sec_key_att_app_id_provider for the application id, which the service answers from the installed packages' certificates (crates/atl-services/src/key_attestation.rs).
//! - an app without READ_PRIVILEGED_PHONE_STATE asking for ATTESTATION_ID_IMEI gets CANNOT_ATTEST_IDS from keystore2's own permission check (utils.rs:143-148), which asks the `permission` service (crates/atl-services/src/permission_controller.rs; AOSP ActivityManagerService.PermissionController).
//! - an app reads the boot state of an unlocked device with no verified boot: ro.boot.verifiedbootstate orange, ro.boot.vbmeta.device_state unlocked (manager 2026-11-23; CONSTANTS in atl-property-service).
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
use atl_layout::Layout;

use crate::atld_run::{Package, Run, Runtime, Shell};
use crate::tree;

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
    /// Red control: the image's service access table grants every permission to every domain on every type (servicemanager then allows everything); the HAL checks must fail.
    #[arg(long)]
    break_allow_all: bool,
}

/// [`Args::break_allow_all`]'s table: `table` with every permission of every class granted from each of its domains to each of its targets.
fn allow_all(table: &atl_sepolicy::Table) -> atl_sepolicy::Table {
    let mut open = atl_sepolicy::Table::new(table.domains().clone(), table.targets().clone());
    for d in table.domains() {
        for t in table.targets() {
            for (class, perms) in atl_sepolicy::CLASSES {
                for p in *perms {
                    open.grant(d, t, class, p);
                }
            }
        }
    }
    open
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
/// android.system.keystore2.ResponseCode.PERMISSION_DENIED (ResponseCode.aidl:38).
const PERMISSION_DENIED: i32 = 6;
/// android.hardware.security.keymint.SecurityLevel.SOFTWARE (SecurityLevel.aidl).
const SOFTWARE: i32 = 0;
/// android.hardware.security.keymint.ErrorCode.CANNOT_ATTEST_IDS (ErrorCode.aidl:91): keystore2's refusal of a device-identifier tag without the privileged permission (utils.rs:146) and the TA's refusal of an ID it holds no value for.
const CANNOT_ATTEST_IDS: i32 = -66;
/// android.permission.READ_PRIVILEGED_PHONE_STATE: the permission keystore2 checks for a device-identifier tag (utils.rs:145).
const READ_PRIVILEGED_PHONE_STATE: &str = "android.permission.READ_PRIVILEGED_PHONE_STATE";
/// The IMEI the HAL's `attestation_id_info` (hardware/interfaces/security/keymint/aidl/default/hal/lib.rs) attested: a device identity D-K1 forbids.
const CUTTLEFISH_IMEI: &str = "867400022047199";

/// The probe's lookups of the KeyMint HAL's name (checkService, getService, isDeclared): each asks servicemanager's canFind once, so each denial writes one avc line.
const HAL_LOOKUPS: u64 = 3;
/// The KeyMint HAL's service_contexts type (`private/service_contexts` `android.hardware.security.keymint.IKeyMintDevice/default`).
const HAL_TYPE: &str = "hal_keymint_service";

/// The lines the probe must print (Probe.java), each with what it shows.
fn checks() -> [(&'static str, String); 18] {
    [
        // servicemanager decides from the compiled pinned policy (Access over libselinux, atl-selinux's table): untrusted_app may not find hal_keymint_service (private/app_neverallows.te:254,
        // public/service.te:336: no allow), so checkService / getService answer null (ServiceManager.cpp tryGetService) and isDeclared fails EX_SECURITY (ServiceManager.cpp:716-733)
        (
            "an app's checkService of the KeyMint HAL: null",
            "AtlKeystore: hal-check null".to_owned(),
        ),
        (
            "an app's getService of the KeyMint HAL: null",
            "AtlKeystore: hal-get null".to_owned(),
        ),
        (
            "an app's isDeclared of the KeyMint HAL: SecurityException",
            "AtlKeystore: hal-declared threw java.lang.SecurityException".to_owned(),
        ),
        // untrusted_app_all.te:99 (find app_api_service: activity_service), technical_debt.cil hal_graphics_allocator_client (find hal_graphics_allocator_service)
        (
            "an app's checkService(activity): a binder",
            "AtlKeystore: activity binder".to_owned(),
        ),
        (
            "an app's checkService of the allocator HAL: a binder",
            "AtlKeystore: allocator binder".to_owned(),
        ),
        // untrusted_app_all.te:88 (list servicemanager): listServices is not filtered by find
        (
            "an app's listServices names the KeyMint HAL",
            "AtlKeystore: list-has-hal true".to_owned(),
        ),
        // service.rs get_key_descriptor_for_lookup: GetInfo on wifi_key denied (app.te:164 grants untrusted_app only keystore:keystore2_key), then KeystorePerm::List denied
        // (untrusted_app has no keystore2 grant) -> error.rs:240 PERMISSION_DENIED; the generate check after it shows keystore2 still serves
        (
            "listEntries(Domain.SELINUX, 102): PERMISSION_DENIED",
            format!("AtlKeystore: selinux-list error {PERMISSION_DENIED}"),
        ),
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
        // keystore2 asks sec_key_att_app_id_provider for the application id (security_level.rs add_required_parameters); the chain is AOSP's software chain (D-K1 (a)), no hardware claim
        (
            "an EC key generated with an attestation challenge: AOSP's software chain",
            "AtlKeystore: attest-chain software".to_owned(),
        ),
        (
            "the generated key deletes",
            "AtlKeystore: delete-key ok".to_owned(),
        ),
        (
            "keystore2's database is outside the app sandbox",
            "AtlKeystore: keystore-dir absent".to_owned(),
        ),
        // (b): ATTESTATION_ID_BRAND is not permission-gated by keystore2, so a challenged key reaches the TA, which refuses the ID it has no value for (cert.rs check_attestation_id!)
        (
            "a challenged key asking ATTESTATION_ID_BRAND: CANNOT_ATTEST_IDS",
            format!("AtlKeystore: attest-ids brand-challenge error {CANNOT_ATTEST_IDS}"),
        ),
        // (c): keystore2 checks READ_PRIVILEGED_PHONE_STATE of the probe app through the `permission` service before the HAL (security_level.rs:500-504); the app holds none, so -66
        (
            "a challenged key asking ATTESTATION_ID_IMEI: CANNOT_ATTEST_IDS",
            format!("AtlKeystore: attest-ids imei-challenge error {CANNOT_ATTEST_IDS}"),
        ),
        // the boot state of an unlocked device with no verified boot (manager 2026-11-23; CONSTANTS in atl-property-service): orange and unlocked, never green
        (
            "an app reads ro.boot.verifiedbootstate: orange",
            "AtlKeystore: verifiedbootstate orange".to_owned(),
        ),
        (
            "an app reads ro.boot.vbmeta.device_state: unlocked",
            "AtlKeystore: vbmeta-device-state unlocked".to_owned(),
        ),
    ]
}

/// Appends the regular files under `dir` (recursively, symlinks skipped) whose bytes contain `needle`, as paths relative to `tree`.
fn literal_hits(tree: &Path, dir: &Path, needle: &[u8], hits: &mut Vec<String>) -> Result<()> {
    for e in std::fs::read_dir(dir).with_context(|| dir.display().to_string())? {
        let e = e?;
        let p = e.path();
        let ty = e.file_type()?;
        if ty.is_dir() {
            literal_hits(tree, &p, needle, hits)?;
        } else if ty.is_file()
            && std::fs::read(&p)?
                .windows(needle.len())
                .any(|w| w == needle)
        {
            hits.push(p.strip_prefix(tree).unwrap_or(&p).display().to_string());
        }
    }
    Ok(())
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let rt = Runtime::new(
        root,
        "keystore",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "keystore",
        args.verbose,
    )?
    .with_keystore(root)?
    // an app process's hwui RenderThread initialises EGL at start even with no window (it aborts without a GL driver): the app stage needs both, as the app gate
    .with_gralloc(root)?
    .with_gl_driver(root)?;
    let work = rt.out.join("keystore");
    std::fs::create_dir_all(&work).with_context(|| work.display().to_string())?;
    let rt = if args.break_allow_all {
        let built = rt.out.join("gen/sepolicy/service_access");
        let text = std::fs::read_to_string(&built).with_context(|| built.display().to_string())?;
        let open = work.join("service_access.allow-all");
        std::fs::write(
            &open,
            allow_all(&atl_sepolicy::Table::parse(&text)?)
                .to_text("keystore gate --break-allow-all: every permission granted"),
        )?;
        println!("keystore: --break-allow-all: the image's service access table grants everything");
        rt.with_service_access(open)
    } else {
        rt
    };
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
    // -66 also comes from the TA for an ID it holds no value for, so the IMEI row alone cannot tell the two apart: keystore2's own refusal (utils.rs:194, the caller lacks the permission) is
    // what shows the `permission` service answered false (utils.rs:146, :194 format, permission name utils.rs:145)
    let permission_refusal =
        format!("caller does not have the '{READ_PRIVILEGED_PHONE_STATE}' permission");
    let ok = ks_log.contains(&permission_refusal);
    println!(
        "keystore: {} keystore2 refuses the IMEI request on its permission check: {permission_refusal}",
        if ok { "pass" } else { "FAIL" }
    );
    if !ok {
        failed.push("keystore2's permission refusal of the IMEI request".to_owned());
    }
    // the KeyMint HAL's sources, as the build compiles them (task copies included), carry no copy of the Cuttlefish IMEI its attestation IDs used to hold (no device identity, D-K1).
    // Not a scan of the built executable: LLVM folds the 15-byte literal into movabs immediates (the first 8 bytes are in the image, the whole string is not).
    let tree = tree::shared_tree(&Layout::of(root)?);
    let mut hits = Vec::new();
    for dir in [
        "hardware/interfaces/security/keymint/aidl/default",
        "system/keymint",
    ] {
        literal_hits(
            &tree,
            &tree.join(dir),
            CUTTLEFISH_IMEI.as_bytes(),
            &mut hits,
        )?;
    }
    if hits.is_empty() {
        println!("keystore: pass the KeyMint HAL sources carry no IMEI literal");
    } else {
        println!(
            "keystore: FAIL the KeyMint HAL sources carry the Cuttlefish IMEI: {}",
            hits.join(", ")
        );
        failed.push("the KeyMint HAL sources carry the Cuttlefish IMEI".to_owned());
    }
    // libselinux's avc line per denial (avc.c avc_audit), through servicemanager's SELINUX_CB_LOG into its kernel log (atld kernel_log_to_log); atl-diag counts them as `denied`
    let sm_log = read(&run.log("servicemanager"));
    let denials = atl_diag::summary::totals(&atl_diag::summary::events(&sm_log))
        .into_iter()
        .find(|c| c.outcome == atl_diag::DENIED && c.api == format!("find {HAL_TYPE}"))
        .map_or(0, |c| c.count);
    let ok = denials == HAL_LOOKUPS;
    println!(
        "keystore: {} servicemanager's log: one avc denial of find {HAL_TYPE} per probe lookup, counted by atl-diag ({denials} of {HAL_LOOKUPS})",
        if ok { "pass" } else { "FAIL" }
    );
    let avc_lines: Vec<String> = sm_log
        .lines()
        .filter(|l| l.contains("avc:"))
        .map(|l| format!("servicemanager: {l}"))
        .collect();
    if !ok {
        failed.push(format!("avc denials of find {HAL_TYPE}"));
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
    lines.extend(avc_lines);
    // the probe's checks, servicemanager's avc line, the HAL source check and keystore2's permission refusal
    let mut count = checks().len() + 3;
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
