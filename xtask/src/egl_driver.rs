// SPDX-License-Identifier: MIT
//! `xtask test egl-driver`: the GLES driver over the host's system EGL (crate atl-egl-driver, `libGLES_atl.so`) rendering into an ANativeWindow, every pixel checked, orientation included (atl-egl-driver-probe's module doc has the scenario and the judge).
//! Runs on this host's own EGL: llvmpipe on WSL, the GPU of gralloc's node on a desktop. No native build and no slot: the driver and the probe are plain cargo outputs (in --out).
//! Breaks, every run, each must fail: `flip` (the window drops the buffer transform: a driver that does not tag FLIP_V shows upside down) and `fbo0` (framebuffer entries from the host, not the driver: framebuffer 0 is not the window).
//! On another host: copy `<out>/egl/libGLES_atl.so` and the probe (`<out>/cargo/release/atl-egl-driver-probe`) and run `atl-egl-driver-probe --driver libGLES_atl.so [--break flip|fbo0]`; the probe links no workspace library dynamically.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};

const DRIVER_PACKAGE: &str = "atl-egl-driver";
const PROBE_PACKAGE: &str = "atl-egl-probe";
const PROBE_BIN: &str = "atl-egl-driver-probe";

#[derive(clap::Args)]
pub struct Args {
    /// Output directory (default: <workspace>/out/egl-driver).
    #[arg(long)]
    out: Option<PathBuf>,
    /// DRM node gralloc allocates on (default: gralloc's own trial, the node the driver renders on).
    #[arg(long)]
    alloc: Option<PathBuf>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = args.out.clone().unwrap_or_else(|| root.join("out/egl-driver"));
    let dir = crate::cdylib::stage(root, &out, DRIVER_PACKAGE, "egl", atl_egl_driver_name())?;
    let driver = dir.join(atl_egl_driver_name());
    let probe = build_probe(root, &out.join("cargo"))?;
    let go = |brk: Option<&str>| -> Result<(bool, String)> {
        let mut cmd = Command::new(&probe);
        cmd.arg("--driver").arg(&driver);
        if let Some(a) = &args.alloc {
            cmd.arg("--alloc").arg(a);
        }
        if let Some(b) = brk {
            cmd.args(["--break", b]);
        }
        let o = cmd.output().context("running the probe")?;
        // the host EGL's own warnings go to stderr; the probe's lines are on stdout
        Ok((o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned()))
    };
    let (ok, text) = go(None)?;
    print!("{text}");
    if !ok || !text.contains("egl-driver: PASS") {
        bail!("egl-driver: FAILED");
    }
    for (brk, mark) in [("flip", "orientation red"), ("fbo0", "FAIL")] {
        let (passed, text) = go(Some(brk))?;
        let caught = !passed && text.contains(mark);
        println!("egl-driver: break {brk}: {}", if caught { "red (expected)" } else { "NOT caught" });
        if !caught {
            print!("{text}");
            bail!("egl-driver: FAILED control: break {brk} passed");
        }
    }
    println!("egl-driver: ok (controls: flip, fbo0 red)");
    Ok(ExitCode::SUCCESS)
}

/// The file name AOSP's loader opens for `ro.hardware.egl=atl` (Loader.cpp `libGLES_<name>.so`).
fn atl_egl_driver_name() -> &'static str {
    "libGLES_atl.so"
}

fn build_probe(root: &Path, target: &Path) -> Result<PathBuf> {
    crate::unit_use::exec(
        crate::cargo_command()
            .current_dir(root)
            .args(["build", "--release", "-p", PROBE_PACKAGE, "--bin", PROBE_BIN, "--target-dir"])
            .arg(target),
        "cargo build (egl-driver probe)",
    )?;
    let bin = target.join("release").join(PROBE_BIN);
    ensure!(bin.is_file(), "{} missing after cargo build", bin.display());
    Ok(bin)
}
