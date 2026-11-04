// SPDX-License-Identifier: MIT
//! `xtask test parcel`: Parcel encodings against their ground truth.
//! 1. probe: atl-aidl-probe, built as `xtask native` builds converter packages (packages.rs), round-trips an AIDL parcelable through AOSP's C++ Parcel (libbinder_rs -> libbinder_ndk -> libbinder).
//!    Two runs: as built (must exit 0), then told to expect one int32 more than Parcel's encoding gives (must fail naming the byte count), so the probe's check is shown to fire.
//! 2. java: atl-parcelables' codecs against Java's own Parcelables on host ART (host_art.rs; framework.jar from --java-out): ParcelTruth.java (xtask/testdata/parcel) marshals each M2 parcelable,
//!    libatl_parcel_truth.so (crate atl-parcel-truth) decodes it with the codec (every byte consumed) and re-encodes; the bytes must be equal, and Java must read the Rust bytes back to the
//!    same bytes. Bundles and Intent extras with objects go through Java's own Parcels instead (`roundTripParcel`: the codec reads and writes the Parcel's data and object table): a binder comes back the same object and answers,
//!    `hasFileDescriptors` agrees with Java's. Every case in [`CASES`] must report pass, including the self-tests that make a corrupted encoding, a wrong codec and an unmodelled value fail; the refusal of the last also has to be a stderr line.
//!    Needs the converter's ART in --out: `xtask native ... libart libnativehelper libjavacore libopenjdk libicu_jni libandroid_runtime libandroid libsigchain`.
//! 3. permission hook: `atl-aidl-probe --enforce` (the aidl patch's @EnforcePermission hook): a denying hook returns SECURITY and the method body never runs; an allowing one runs it.
//!
//! Stages 2 and 3 run as binder processes of a binderd this stage starts (binder.rs start_binderd): libbinder opens the driver as soon as a parcel carries a binder or an interface token.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

use crate::atld_run::without_stamp;
use crate::host_art::HostArt;
use crate::{binder, farm, packages, slot};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory whose graph holds libbinder_rs, libatl_parcelables and ART (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: the ART host boot jars, framework-minus-apex and its d8 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Print every ART run's command, stdout and stderr.
    #[arg(long, short)]
    verbose: bool,
}

const PACKAGE: &str = "atl-aidl";
const PROBE: &str = "atl-aidl-probe";
/// The probe's override of its expected parcel size (atl-aidl src/bin/probe.rs).
const WANT_BYTES: &str = "--want-bytes";
/// What the probe says when the size differs.
const SIZE_FAILURE: &str = "Parcel's encoding gives";
/// One more int32 than the probe's own expectation (48, probe.rs PARCEL_BYTES): any value but the true size makes the check fire; this one is the nearest the encoding could hold.
const WRONG_BYTES: &str = "52";
/// The probe's permission-hook mode (atl-aidl src/bin/probe.rs).
const ENFORCE: &str = "--enforce";

/// The Java driver's dir and class (xtask/testdata/parcel).
const FIXTURE: &str = "xtask/testdata/parcel/java";
const DRIVER: &str = "ParcelTruth";
/// atl-parcel-truth's cdylib, in <out>/cargo/release.
const TRUTH_LIB: &str = "libatl_parcel_truth.so";
/// The framework's natives (Parcel, SystemClock), registered by the driver through AndroidRuntime::startReg as app_process does.
const RUNTIME: &str = "device/libandroid_runtime/libandroid_runtime.so";
/// The driver's binder uid: AID_APP_START (system/core/libcutils/include/private/android_filesystem_config.h), the first app uid, as an app process has.
const AID_APP_START: u32 = 10000;
/// Every line ParcelTruth.java prints, by case, in its order: a missing one fails the stage (a driver that stops early passes nothing). `summary` is its last line.
const CASES: [&str; 47] = [
    "ApplicationInfo",
    "ActivityInfo",
    "ActivityManager.TaskDescription",
    "ApplicationErrorReport.ParcelableCrashInfo",
    "DisplayInfo",
    "Configuration",
    "CompatibilityInfo",
    "Intent",
    "Intent+selector",
    "Intent:extras",
    "IntentFilter",
    "ProviderInfo",
    "ProviderInfoList",
    "ContentProviderHolder",
    "PackageInfo",
    "ComponentName",
    "InstrumentationInfo",
    "ParceledListSlice",
    "ClientTransaction",
    "ClientTransaction:resume",
    "Map",
    "Map:null",
    "Bundle",
    "Bundle:entries",
    "Bundle:put-all",
    "Bundle:binder",
    "Bundle:fd",
    "Intent:binder-extras",
    "WindowManager.LayoutParams",
    "InsetsState",
    "ClientWindowFrames",
    "MergedConfiguration",
    "SurfaceControl",
    "SurfaceControl:native",
    "InsetsSourceControl.Array",
    "WindowRelayoutResult",
    "InputDevice",
    "InputDevice:pointer",
    "Rect",
    "InputChannel",
    "PointerIcon",
    "self-test:corrupt",
    "self-test:wrong-codec",
    "self-test:refusal",
    "self-test:bundle-flag",
    "self-test:fd-read",
    "summary",
];

/// One probe run's result: exit success and stderr.
fn run_probe(probe: &Path, args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new(probe)
        .args(args)
        .output()
        .with_context(|| format!("running {}", probe.display()))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// The verdict of the two probe runs: the round trip passes, and the size check fires on a wrong expectation.
fn verdict(round_trip: &(bool, String), wrong: &(bool, String)) -> Vec<String> {
    let mut fails = Vec::new();
    if !round_trip.0 {
        fails.push(format!("round trip failed: {}", round_trip.1.trim()));
    }
    if wrong.0 || !wrong.1.contains(SIZE_FAILURE) {
        fails.push(format!(
            "size check did not fire with {WANT_BYTES} {WRONG_BYTES}: exit ok = {}, stderr {:?}",
            wrong.0,
            wrong.1.trim()
        ));
    }
    fails
}

/// The verdict of the Java run (`pt pass|fail <case> <detail>` lines, then `pt summary`): passes and failures as printed; every case of [`CASES`] once, in order, and exit 0.
fn java_verdict(ok: bool, stdout: &str, failure: &str) -> (Vec<String>, Vec<String>) {
    truth_verdict("pt ", &CASES, DRIVER, ok, stdout, failure)
}

/// The verdict of a truth driver's run: its `<prefix>pass|fail <case> <detail>` lines (then `<prefix>summary`) as passes and failures; `cases` must each be reported once, in order, and a bad exit needs a failure line or is one itself.
pub fn truth_verdict(
    prefix: &str,
    cases: &[&str],
    driver: &str,
    ok: bool,
    stdout: &str,
    failure: &str,
) -> (Vec<String>, Vec<String>) {
    let mut passes = Vec::new();
    let mut fails = Vec::new();
    let mut seen = Vec::new();
    for line in stdout.lines().filter_map(|l| l.strip_prefix(prefix)) {
        let (verdict, rest) = line.split_once(' ').unwrap_or((line, ""));
        match verdict {
            "pass" => passes.push(rest.to_owned()),
            "fail" => fails.push(rest.to_owned()),
            _ => {}
        }
        seen.push(
            if verdict == "summary" {
                "summary"
            } else {
                rest.split(' ').next().unwrap_or("")
            }
            .to_owned(),
        );
    }
    if seen != cases {
        fails.push(format!("cases reported {seen:?}, want {cases:?}"));
    }
    if !ok && fails.is_empty() {
        fails.push(format!("{driver} ended badly: {failure}"));
    }
    (passes, fails)
}

/// The line `atl_parcelables::refuse` writes to stderr for the Intent ClipData refusal that ParcelTruth's `self-test:refusal` provokes (once): `parcelables: refused <what> <n>`.
const REFUSAL_LINE: &str = "parcelables: refused android.content.Intent.clipData 1";

/// A failure message when the refusal of the self-test is missing from the driver's stderr: refusals that show up only in a counter nobody prints are the failure this guards.
fn refusal_logged(stderr: &str) -> Option<String> {
    if stderr.lines().any(|l| without_stamp(l) == REFUSAL_LINE) {
        None
    } else {
        Some(format!("stderr lacks the refusal line {REFUSAL_LINE:?}"))
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |workspace| {
        workspace.join("out/native").join(&args.host)
    })?;
    let probe = packages::bin(root, &out, PACKAGE, PROBE)?;
    let mut fails = verdict(
        &run_probe(&probe, &[])?,
        &run_probe(&probe, &[WANT_BYTES, WRONG_BYTES])?,
    );
    if fails.is_empty() {
        println!("parcel: pass probe (round trip, and the size check fires on a wrong size)");
    }

    let art = HostArt::new(
        root,
        &args.host,
        args.java_out.as_deref(),
        &out,
        "parcel-art",
        args.verbose,
    )?;
    let lib = art.cargo.join("release").join(TRUTH_LIB);
    ensure!(
        lib.is_file(),
        "{} missing: `xtask native` builds atl-parcel-truth once its graph holds libatl_parcelables",
        lib.display()
    );
    let fixture = art.compile(&root.join(FIXTURE), &out.join("parcel"))?;
    let runtime = out.join(RUNTIME);
    ensure!(
        runtime.is_file(),
        "{} missing: build libandroid_runtime",
        runtime.display()
    );
    // writing a binder, even null, reaches libbinder's IPCThreadState and so opens the driver: the run is a binder process of binderd, as an app's is
    let (mut binderd, harness) =
        binder::start_binderd(root, &out, &out.join("parcel/binderd.log"))?;
    // the virtual keyboard's KeyCharacterMap in Parcel form, loaded by the same atl-libinput code the input service runs
    let keymap = out.join("parcel/Virtual.kcm.parcel");
    let to_parcel = packages::bin(root, &out, "atl-libinput", "atl-kcm-parcel")?;
    let kcm = farm::virtual_kcm(&Layout::of(root)?);
    let status = Command::new(&to_parcel)
        .arg(&kcm)
        .arg(&keymap)
        .status()
        .with_context(|| format!("running {}", to_parcel.display()))?;
    ensure!(
        status.success(),
        "{} refused {} ({status})",
        to_parcel.display(),
        kcm.display()
    );
    let cmd = art.command(
        &fixture,
        &fixture.area.1,
        &[],
        DRIVER,
        &[
            lib.display().to_string(),
            runtime.display().to_string(),
            keymap.display().to_string(),
        ],
    );
    let what = format!("{cmd:?}");
    let run = harness
        .spawn(cmd, AID_APP_START, false)
        .and_then(|child| art.finish(&what, child));
    // the permission hook (atl-aidl-probe --enforce), a binder process too
    let mut enforce_cmd = Command::new(&probe);
    enforce_cmd
        .arg(ENFORCE)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let enforce = harness
        .spawn(enforce_cmd, AID_APP_START, false)
        .and_then(|child| Ok(child.wait_with_output()?));
    drop(harness);
    binderd.wait()?;
    let run = run?;
    let enforce = enforce?;
    if enforce.status.success() {
        println!(
            "parcel: pass permission hook (deny -> SECURITY, body not run; allow -> body runs)"
        );
    } else {
        fails.push(format!(
            "permission hook ({}): {}",
            enforce.status,
            String::from_utf8_lossy(&enforce.stderr).trim()
        ));
    }
    let (passes, java_fails) = java_verdict(run.ok, &run.stdout, &run.failure());
    for p in &passes {
        println!("parcel: pass {p}");
    }
    fails.extend(java_fails);
    // a refusal must be visible in the process log, not only in the counter map (ParcelTruth's self-test:refusal makes one)
    if let Some(f) = refusal_logged(&run.stderr) {
        fails.push(f);
    }
    for f in &fails {
        println!("parcel: FAIL {f}");
    }
    if fails.is_empty() {
        println!("parcel: pass (probe + {} java cases)", passes.len());
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", crate::verdict::gate("parcel", &fails));
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each failure is reported: a failed round trip, a wrong expectation the probe accepts, and one it refuses for another reason.
    #[test]
    fn verdicts() {
        let ok = (true, String::new());
        let fired = (
            false,
            format!("Error: parcel holds 48 bytes, {SIZE_FAILURE} 52"),
        );
        assert!(verdict(&ok, &fired).is_empty());
        assert_eq!(verdict(&(false, "boom".into()), &fired).len(), 1);
        assert_eq!(verdict(&ok, &ok).len(), 1);
        assert_eq!(verdict(&ok, &(false, "other".into())).len(), 1);
    }

    /// The refusal line must be on its own line of stderr; any other text does not count.
    #[test]
    fn refusal_line_is_required() {
        assert_eq!(
            refusal_logged(&format!("noise\n{REFUSAL_LINE}\nmore\n")),
            None
        );
        assert!(refusal_logged("").is_some());
        assert_eq!(refusal_logged(&format!("{REFUSAL_LINE} at=1.000\n")), None);
        assert!(
            refusal_logged("parcelables: refused android.content.Intent.clipData 2\n").is_some()
        );
    }

    /// The Java verdict: all cases passed; a failed case, a missing case, and a crash after all passes each fail.
    #[test]
    fn java_verdicts() {
        let all = CASES[..CASES.len() - 1]
            .iter()
            .map(|c| format!("pt pass {c} ok\n"))
            .collect::<String>()
            + "pt summary 0 failed\n";
        assert!(java_verdict(true, &all, "").1.is_empty());
        let one_failed = all.replacen("pt pass Intent ok", "pt fail Intent bytes differ", 1);
        assert_eq!(
            java_verdict(false, &one_failed, "exit 1").1,
            ["Intent bytes differ"]
        );
        let missing = all.replacen("pt pass PackageInfo ok\n", "", 1);
        assert!(java_verdict(true, &missing, "").1[0].contains("cases reported"));
        assert!(java_verdict(false, &all, "SIGSEGV").1[0].contains("ended badly"));
    }
}
