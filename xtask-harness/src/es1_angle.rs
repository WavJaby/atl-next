// SPDX-License-Identifier: MIT
//! `xtask test es1-angle` (gl-honesty H1 spike, test-only): ANGLE embedded over our driver draws an ES1 frame into the window surface our driver connected.
//! The fixture (xtask-harness/testdata/es1-angle) runs as a plain app process (`--run-class`) with the GL driver + ANGLE placed as for an app (`Runtime::with_gl_driver`); one layer + BLASTBufferQueue, its Surface handed to the JNI lib
//! (jni/lib.rs has the sequence: ANGLE display with the driver's handle, driver window surface + ES3 carrier through libEGL, ANGLE ES1 external context + external surface, acquire / draw / release / swap).
//! Pass: the compositor's ReportSink digest of a `atl-es1` buffer equals [`expected_digest`] (clear + top-left quad, upright: buffer row 0 is the top); `version OpenGL ES 1.1` with `ANGLE` in the fixture log; the lib's `ok`.
//! Break (`break` arg): the driver's current context is cleared before the ANGLE external context is made; the run must end in an `error angle ...` line and show no expected buffer.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};

use crate::atld_run::{Package, Run, Runtime, Shell, run_display};
use crate::composer::{fnv1a64, word};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`).
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's build dir).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory (default: this checkout's build dir).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole client log.
    #[arg(long, short)]
    verbose: bool,
}

const FIXTURE: &str = "xtask-harness/testdata/es1-angle";
const CLIENT_CLASS: &str = "com.example.atles1.Client";
const CLIENT_NAME: &str = "es1-client";
const LIB: &str = "libatl_es1_angle.so";
const LAYER: &str = "atl-es1";
const TAG: &str = "AtlEs1: ";
/// Picture (two copies: here and jni/lib.rs): [`EDGE`] square, [`CLEAR`] everywhere, [`QUAD`] on the top-left quadrant.
const EDGE: usize = 64;
const CLEAR: [u8; 4] = [32, 64, 96, 255];
const QUAD: [u8; 4] = [255, 128, 0, 255];
const LIMIT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(50);

/// Rows top-down (row 0 = the top, as the compositor reads a buffer): the quad on rows and columns 0..EDGE/2.
fn expected_digest() -> u64 {
    let mut px = Vec::with_capacity(EDGE * EDGE * 4);
    for y in 0..EDGE {
        for x in 0..EDGE {
            px.extend(if x < EDGE / 2 && y < EDGE / 2 {
                QUAD
            } else {
                CLEAR
            });
        }
    }
    fnv1a64(&px)
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let rt = Runtime::new(
        root,
        "es1-angle",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "es1-art",
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver(root)?;
    let rt = rt.with_display(run_display().to_arg());
    let work = rt.out.join("es1-angle");
    let lib = build_lib(root, &work.join("cargo"))?;
    let lib_at = format!("{}/{LIB}", atl_android_paths::SYSTEM_LIB);
    let rt = rt.with_fixture_file(lib, &lib_at);
    let apk = rt
        .apk
        .build(&root.join(FIXTURE), &work.join("apk"), &[], None, None)?;
    let sys = atl_android_paths::SYSTEM_LIB;
    let base = vec![
        lib_at.clone(),
        format!("{sys}/libEGL_angle.so"),
        format!("{sys}/libGLESv1_CM_angle.so"),
        format!(
            "{}/{}",
            atl_android_paths::VENDOR_EGL,
            crate::egl_driver::atl_egl_driver_name()
        ),
    ];
    let want = expected_digest();
    let mut ok = true;
    for broken in [false, true] {
        let tag = if broken { "break" } else { "run" };
        let mut shell_args = base.clone();
        if broken {
            shell_args.push("break".to_owned());
        }
        let log = run_client(&rt, &apk, &work.join(tag), shell_args, &args.log_tags)?;
        if args.verbose {
            println!("---- {tag} log\n{log}");
        }
        let lines: Vec<&str> = log
            .lines()
            .filter_map(|l| l.split_once(TAG).map(|(_, t)| t))
            .collect();
        for l in &lines {
            println!("es1-angle: [{tag}] {l}");
        }
        let digests: Vec<u64> = log
            .lines()
            .filter(|l| l.starts_with("composer: quad ") && word(l, "name") == Some(LAYER))
            .filter_map(|l| word(l, "digest").and_then(|d| u64::from_str_radix(d, 16).ok()))
            .collect();
        let shown = digests.contains(&want);
        let version = lines
            .iter()
            .find_map(|l| l.strip_prefix("version "))
            .unwrap_or("");
        let version_ok = version.starts_with("OpenGL ES 1.1") && version.contains("ANGLE");
        let lib_ok = lines.contains(&"ok");
        let angle_err = lines.iter().find(|l| l.starts_with("error angle"));
        if broken {
            let fired = !shown && angle_err.is_some();
            ok &= fired;
            println!(
                "es1-angle: {} break: {}",
                verdict(fired),
                match angle_err {
                    Some(e) if !shown => format!("fails as it must: {e}"),
                    _ => format!(
                        "NOT caught (expected buffer shown={shown}, angle error={angle_err:?})"
                    ),
                }
            );
            continue;
        }
        for (what, pass, detail) in [
            (
                "pixels",
                shown,
                format!(
                    "{} {LAYER} buffers, expected digest {want:016x} {}",
                    digests.len(),
                    if shown { "among them" } else { "MISSING" }
                ),
            ),
            ("version", version_ok, format!("`{version}`")),
            (
                "lib",
                lib_ok,
                lines
                    .iter()
                    .rev()
                    .find(|l| **l == "ok" || l.starts_with("error"))
                    .copied()
                    .unwrap_or("no result line")
                    .to_owned(),
            ),
        ] {
            ok &= pass;
            println!("es1-angle: {} {what}: {detail}", verdict(pass));
        }
    }
    println!("es1-angle: {}", if ok { "PASS" } else { "FAIL" });
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run_client(
    rt: &Runtime,
    apk: &Path,
    work: &Path,
    args: Vec<String>,
    log_tags: &str,
) -> Result<String> {
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: None,
        packages: vec![Package::base(apk.to_path_buf())],
        libraries: vec![],
        shell: Some(Shell {
            class: CLIENT_CLASS.to_owned(),
            uid: crate::atld_run::FIRST_APP_UID,
            name: CLIENT_NAME.to_owned(),
            args,
            boot_jars: vec![apk.to_path_buf()],
        }),
        log_tags: log_tags.to_owned(),
    };
    let started = Instant::now();
    let mut child = rt.start_atld(&run)?;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > LIMIT {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(POLL);
    }
    let _ = child.wait();
    Ok(std::fs::read_to_string(run.log(CLIENT_NAME)).unwrap_or_default())
}

fn verdict(ok: bool) -> &'static str {
    if ok { "pass" } else { "FAIL" }
}

fn build_lib(root: &Path, target: &Path) -> Result<PathBuf> {
    let status = crate::unit_use::status(
        Command::new(env!("CARGO"))
            .args(["build", "--release", "--quiet", "--manifest-path"])
            .arg(root.join(FIXTURE).join("jni/Cargo.toml"))
            .arg("--target-dir")
            .arg(target),
    )
    .context("running cargo for the es1-angle library")?;
    ensure!(status.success(), "building the es1-angle library failed");
    Ok(target.join("release").join(LIB))
}
