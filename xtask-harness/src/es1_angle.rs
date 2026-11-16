// SPDX-License-Identifier: MIT
//! `xtask test es1-angle`: ANGLE embedded over our driver draws an ES1 frame into the window surface our driver connected.
//! The fixture (xtask-harness/testdata/es1-angle) runs as a plain app process (`--run-class`) with the GL driver + ANGLE placed as for an app (`Runtime::with_gl_driver`); one layer + BLASTBufferQueue, its Surface handed to the JNI lib.
//! Runs ([`Mode`]): `run` (H1 spike: the external-context sequence done from outside the driver, jni/lib.rs `run`), `driver` (H2a: the driver's own ES1 context kind through libEGL and the libGLESv1_CM front, jni/lib.rs `run_driver`).
//! Pass (both): the compositor's ReportSink digest of a `atl-es1` buffer equals [`expected_digest`] (clear + top-left quad, upright: buffer row 0 is the top); `version OpenGL ES 1.1` with `ANGLE`; the lib's `ok`.
//! `driver` also: the chosen ES1 config reports the ES1 bit; eglQueryContext says version 1; an ES3 context in the same process reports the host's GLES 3 (not ANGLE); both cross-kind share contexts fail EGL_BAD_CONTEXT, each counted by a driver line; `glEGLImageTargetTexture2DOES` with no image (in both libEGL tables, exported by ANGLE) is validated under ES1 and ES3 with no driver missing line; `glMultiDrawArraysEXT` (in both tables, in no ANGLE library) called under ES1 then ES3 leaves exactly one driver missing line (the ES1 call; the ES3 one went to the host).
//! Breaks: `break` (the driver's current context is cleared before the ANGLE external context: an `error angle ...` line, no expected buffer); `driver-nolib` (no `libGLESv1_CM_atl.so` placed: the ES1 create fails EGL_BAD_ALLOC with the driver's named line, no expected buffer).

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
    for mode in [Mode::Spike, Mode::SpikeBreak, Mode::Driver] {
        let mut shell_args = base.clone();
        shell_args.push(mode.arg().to_owned());
        let log = run_client(
            &rt,
            &apk,
            &work.join(mode.tag()),
            shell_args,
            &args.log_tags,
        )?;
        ok &= judge(mode, &log, want, args.verbose);
    }
    // the driver without its ES1 library: placed last, the runtime keeps the change
    let rt = rt.without_es1_driver_lib();
    let mut shell_args = base.clone();
    shell_args.push(Mode::DriverNoLib.arg().to_owned());
    let log = run_client(
        &rt,
        &apk,
        &work.join(Mode::DriverNoLib.tag()),
        shell_args,
        &args.log_tags,
    )?;
    ok &= judge(Mode::DriverNoLib, &log, want, args.verbose);
    println!("es1-angle: {}", if ok { "PASS" } else { "FAIL" });
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// H1: ANGLE external context made from outside the driver.
    Spike,
    /// H1 break: the carrier cleared first; ANGLE must be refused, named.
    SpikeBreak,
    /// H2a: the driver's own ES1 context kind, through libEGL and the libGLESv1_CM front.
    Driver,
    /// H2a break: `Driver` with no `libGLESv1_CM_atl.so` placed; the ES1 create must fail named.
    DriverNoLib,
}

impl Mode {
    /// The fixture's mode argument (Client.java).
    fn arg(self) -> &'static str {
        match self {
            Mode::Spike => "spike",
            Mode::SpikeBreak => "break",
            Mode::Driver | Mode::DriverNoLib => "driver",
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Mode::Spike => "run",
            Mode::SpikeBreak => "break",
            Mode::Driver => "driver",
            Mode::DriverNoLib => "driver-nolib",
        }
    }
}

/// The driver's refusal lines (atl-egl-driver es1.rs `create` / `refuse_share`).
const NOLIB_LINE: &str = "libEGL_atl: es1 context refused: ";
const SHARE_LINE: &str = "libEGL_atl: es1 share refused (";
/// The driver's line for an ES1 extension name ANGLE has no entry for (atl-egl-driver es1_ext.rs `select`).
const MISSING_LINE: &str = "libEGL_atl: es1 entry missing ";
/// The name jni/lib.rs `ext_error` calls: in both libEGL tables; ANGLE's libGLESv2_angle exports it, so libEGL's ES1 table takes ANGLE's address (dlsym searches libGLESv1_CM_atl's DT_NEEDED tree) and the driver is never asked under ES1.
const EXT_NAME: &str = "glEGLImageTargetTexture2DOES";
/// The name jni/lib.rs `gap_call` calls: in both libEGL tables, exported by no ANGLE library (measured gap), so both tables hold the driver's shell.
const GAP_NAME: &str = "glMultiDrawArraysEXT";
/// EGL_BAD_CONTEXT (egl.h): the share refusal's error.
const BAD_CONTEXT: &str = "0x3006";
/// EGL_BAD_ALLOC (egl.h): the missing-ES1-library refusal's error.
const BAD_ALLOC: &str = "0x3003";

/// One run's verdict lines; true when it passed (a break passes by failing as it must).
fn judge(mode: Mode, log: &str, want: u64, verbose: bool) -> bool {
    let tag = mode.tag();
    if verbose {
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
    let field = |p: &str| lines.iter().find_map(|l| l.strip_prefix(p)).unwrap_or("");
    let result = lines
        .iter()
        .rev()
        .find(|l| **l == "ok" || l.starts_with("error"))
        .copied()
        .unwrap_or("no result line");
    let mut checks: Vec<(&str, bool, String)> = Vec::new();
    match mode {
        Mode::SpikeBreak => {
            let angle_err = lines.iter().find(|l| l.starts_with("error angle"));
            checks.push((
                "break",
                !shown && angle_err.is_some(),
                format!("expected buffer shown={shown}, angle error={angle_err:?}"),
            ));
        }
        Mode::DriverNoLib => {
            let named = log
                .lines()
                .find_map(|l| l.split_once(NOLIB_LINE).map(|(_, t)| t));
            let refused =
                result.starts_with("error es1 eglCreateContext") && result.contains(BAD_ALLOC);
            checks.push((
                "break no ES1 library",
                !shown && refused && named.is_some(),
                format!("shown={shown}, result `{result}`, driver line {named:?}"),
            ));
        }
        Mode::Spike | Mode::Driver => {
            let version = field("version ");
            checks.push((
                "pixels",
                shown,
                format!(
                    "{} {LAYER} buffers, expected digest {want:016x} {}",
                    digests.len(),
                    if shown { "among them" } else { "MISSING" }
                ),
            ));
            checks.push((
                "version",
                version.starts_with("OpenGL ES 1.1") && version.contains("ANGLE"),
                format!("`{version}`"),
            ));
            checks.push(("lib", result == "ok", result.to_owned()));
        }
    }
    if mode == Mode::Driver {
        // the ES3 context in this ES1 process is the host's: GLES 3, not ANGLE's
        let es3 = field("es3 version ");
        checks.push((
            "es3 in es1 process",
            es3.starts_with("OpenGL ES 3.") && !es3.contains("ANGLE"),
            format!("`{es3}`"),
        ));
        let config = field("config renderable ");
        let es1_bit =
            i64::from_str_radix(config.trim_start_matches("0x"), 16).is_ok_and(|v| v & 1 != 0);
        checks.push((
            "config ES1 bit",
            es1_bit,
            format!("RENDERABLE_TYPE {config}"),
        ));
        checks.push((
            "query version",
            field("query version ") == "1",
            field("query version ").to_owned(),
        ));
        let counted = log.lines().filter(|l| l.contains(SHARE_LINE)).count();
        for what in ["es1-on-es3", "es3-on-es1"] {
            let line = field(&format!("share {what} "));
            checks.push((
                "share refused",
                line == format!("refused {BAD_CONTEXT}"),
                format!("{what}: `{line}`, driver refusal lines {counted}"),
            ));
        }
        checks.push((
            "share counted",
            counted == 2,
            format!("{counted} driver refusal lines"),
        ));
        // atl-egl-driver es1_ext.rs: extension names in both libEGL tables. An error code cannot tell which implementation took a call (ANGLE's GL backend reports the native context's errors too: control run), so the driver's missing line is the observable.
        let missing: Vec<&str> = log
            .lines()
            .filter_map(|l| l.split_once(MISSING_LINE).map(|(_, t)| t))
            .collect();
        let missing_of = |name: &str| {
            missing
                .iter()
                .filter(|t| t.split_whitespace().next() == Some(name))
                .count()
        };
        // a name ANGLE exports: validated under both kinds, never a driver shell under ES1
        for kind in ["es1", "es3"] {
            let err = field(&format!("ext {kind} error "));
            checks.push((
                "ext dispatch",
                missing_of(EXT_NAME) == 0 && !err.is_empty() && err != "0x0",
                format!(
                    "{kind}: {EXT_NAME}(no image) error `{err}`, driver missing lines for it {}",
                    missing_of(EXT_NAME)
                ),
            ));
        }
        // a name no ANGLE library has: the ES1 call reaches the shell, which answers none (one counted line), the ES3 call the host (no line)
        let gap = field("gap ");
        checks.push((
            "ext gap",
            gap == "es1 es3" && missing_of(GAP_NAME) == 1,
            format!(
                "calls `{gap}`, driver missing lines for {GAP_NAME}: {} (all: {missing:?})",
                missing_of(GAP_NAME)
            ),
        ));
    }
    let mut ok = true;
    for (what, pass, detail) in checks {
        ok &= pass;
        println!("es1-angle: {} [{tag}] {what}: {detail}", verdict(pass));
    }
    ok
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
