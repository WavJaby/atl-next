// SPDX-License-Identifier: MIT
//! `xtask test compositor`: atl-compositor's wire codecs against libgui's own C++ on host ART (framework.jar from --java-out; the converter's ART and libgui in --out).
//! CompositorTruth.java (xtask/testdata/compositor) drives libandroid_runtime's SurfaceControl JNI over libgui in both directions, libatl_parcel_truth.so (crate atl-parcel-truth, compositor.rs) decodes and encodes with atl-compositor:
//! 1. writer: a `SurfaceControl.Transaction` built with real setters is marshalled by libgui's C++ writer (`TransactionState::writeToParcel`, the bytes `setTransactionState` sends); Rust decodes it, every value Java set must come back, and Rust's re-encoding must equal libgui's bytes and object table.
//! 2. reader: Rust encodes a fully populated apply-shaped transaction; libgui's C++ reader decodes it (Transaction.CREATOR) and its writer encodes it again: equal except fd numbers.
//! 3. callbacks: Rust's `onTransactionCompleted` runs libgui's `TransactionCompletedListener` (a committed callback registered from Java must fire once); `onReleaseBuffer` must parse.
//! 4. self-tests: a corrupted re-encoding, swapped expected values and a dropped field must each be caught.
//!
//! The run is a binder process of a binderd this stage starts (libbinder opens the driver as soon as a parcel carries a binder).
//! `--golden <dir>` writes the libgui-written dump (data and object table, `WireParcel::to_dump`) there: the file committed as crates/atl-libgui-parcel/testdata/transaction_libgui_writer.dump.
//! Needs the converter's ART and libgui in --out: `xtask native` with the M3 goals (libandroid_runtime libandroid libsigchain ...).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};

use crate::binder;
use crate::host_art::HostArt;
use crate::parcel::truth_verdict;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory whose graph holds libgui, libbinder and ART (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: the ART host boot jars, framework-minus-apex and its d8 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Write the libgui-written Transaction dump into this existing directory.
    #[arg(long)]
    golden: Option<PathBuf>,
    /// Print the ART run's command, stdout and stderr.
    #[arg(long, short)]
    verbose: bool,
}

/// The Java driver's dir and class (xtask/testdata/compositor).
const FIXTURE: &str = "xtask/testdata/compositor/java";
const DRIVER: &str = "CompositorTruth";
/// atl-parcel-truth's cdylib, in <out>/cargo/release.
const TRUTH_LIB: &str = "libatl_parcel_truth.so";
/// The framework's natives, registered by the driver through AndroidRuntime::startReg as app_process does.
const RUNTIME: &str = "device/libandroid_runtime/libandroid_runtime.so";
/// The driver's binder uid: AID_SYSTEM, which servicemanager lets add services (the driver stands in for SurfaceFlinger's registration).
const AID_SYSTEM: u32 = 1000;
/// The driver's last stderr lines, printed when the gate fails: the launcher's refusal (no property area) is among them, and the driver prints no case line then.
const STDERR_LINES: usize = 20;
/// Every line CompositorTruth.java prints, by case, in its order (`summary` last): a missing one fails the stage.
const CASES: [&str; 9] = [
    "writer:layer-fields",
    "writer:minimal",
    "reader:full-state",
    "callback:transaction-completed",
    "callback:release-buffer",
    "self-test:corrupt",
    "self-test:swapped-values",
    "self-test:dropped-field",
    "summary",
];

/// Native goals (gate_goals.rs): servicemanager, `service`, host ART with its boot jars, libgui.
pub fn goals() -> crate::gate_goals::Goals {
    crate::gate_goals::Goals::host(&["servicemanager", "service", "libgui"])
        .with(crate::native_load::goals())
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "compositor")?;
    let art = HostArt::new(
        root,
        &args.host,
        args.java_out.as_deref(),
        &out,
        "compositor-art",
        args.verbose,
    )?;
    let lib = crate::packages::target_dir(&out)
        .join("release")
        .join(TRUTH_LIB);
    ensure!(
        lib.is_file(),
        "{} missing: `xtask native` builds atl-parcel-truth once its graph holds libatl_parcelables and libbinder",
        lib.display()
    );
    let fixture = art.compile(&root.join(FIXTURE), &out.join("compositor"))?;
    let runtime = out.join(RUNTIME);
    ensure!(
        runtime.is_file(),
        "{} missing: build libandroid_runtime",
        runtime.display()
    );
    let mut extra = vec![lib.display().to_string(), runtime.display().to_string()];
    // written by the driver: writable inside the sandbox
    let mut writable = vec![fixture.work.join("data")];
    if let Some(dir) = &args.golden {
        ensure!(dir.is_dir(), "--golden {}: not a directory", dir.display());
        extra.push(std::path::absolute(dir)?.display().to_string());
        writable.push(std::path::absolute(dir)?);
    }
    // libgui blocks until a "SurfaceFlingerAIDL" service exists (SurfaceComposerClient::onFirstRef), which the driver registers: so a servicemanager, and a driver allowed to add services
    let device = out.join("device");
    let servicemanager = device.join("servicemanager/servicemanager");
    let service = device.join("service/service");
    for b in [&servicemanager, &service] {
        ensure!(
            b.is_file(),
            "{} missing: cargo xtask native --host {} --out {} servicemanager service",
            b.display(),
            args.host,
            out.display()
        );
    }
    let sm_root = out.join("compositor-sm");
    binder::fresh_layout(&sm_root)?;
    let env = binder::lib_env(&out)?;
    let (broker, mut binderd) = binder::start_binderd(root, &out, &sm_root.join("binderd.log"))?;
    let (mut sm, found) =
        binder::start_servicemanager(&broker, &sm_root, &servicemanager, &service, &env)?;
    ensure!(
        found.0,
        "servicemanager did not come up: {}",
        found.1.trim()
    );
    let cmd = art.command(&fixture, &fixture.area.1, &[], DRIVER, &extra);
    let what = format!("{cmd:?}");
    // the sandbox shows only what is mounted (binder::spawn_of): the slot read-only at its own path, so the launcher finds the property area its variable names; ANDROID_DATA and --golden writable (parcel.rs does the same)
    let run = (|| -> Result<_> {
        let mut s = binder::spawn_of(&cmd, AID_SYSTEM)?;
        s.stdout = std::process::Stdio::piped();
        s.stderr = std::process::Stdio::piped();
        s.mounts.push(atl_broker::Mount {
            host: out.clone(),
            at: out.clone(),
            writable: false,
        });
        // the staged ART root's links name files outside the slot (ICU data in src/aosp, tzdata): their targets are mounted too (parcel.rs does the same)
        s.mounts.extend(atl_broker::tree_mounts(&art.art)?);
        for dir in &writable {
            s.mounts.push(atl_broker::Mount {
                host: dir.clone(),
                at: dir.clone(),
                writable: true,
            });
        }
        let p = broker
            .spawn(s)
            .context("spawning the compositor truth driver")?;
        art.finish(&what, p.child)
    })();
    sm.child.kill().ok();
    sm.child.wait().ok();
    drop(broker);
    binderd.child.wait()?;
    let run = run.context("running the compositor truth driver")?;
    let (passes, fails) = truth_verdict("ct ", &CASES, DRIVER, run.ok, &run.stdout, &run.failure());
    for p in &passes {
        println!("compositor: pass {p}");
    }
    for f in &fails {
        println!("compositor: FAIL {f}");
    }
    if fails.is_empty() {
        println!("compositor: pass ({} java cases)", passes.len());
        return Ok(ExitCode::SUCCESS);
    }
    let stderr: Vec<&str> = run.stderr.lines().collect();
    for line in &stderr[stderr.len().saturating_sub(STDERR_LINES)..] {
        println!("compositor: driver stderr: {line}");
    }
    println!("{}", crate::verdict::gate("compositor", &fails));
    Ok(ExitCode::FAILURE)
}
