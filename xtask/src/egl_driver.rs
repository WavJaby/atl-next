// SPDX-License-Identifier: MIT
//! `xtask test egl-driver`: the GLES driver over the host's system EGL (crate atl-egl-driver, `libGLES_atl.so`) rendering into an ANativeWindow, every pixel checked, orientation included (atl-egl-driver-probe's module doc has the scenario and the judge).
//! Runs on this host's own EGL: llvmpipe on WSL, the GPU of gralloc's node on a desktop. No native build and no slot: the driver and the probe are plain cargo outputs (in --out).
//! Two runs, one per orientation tier (driver.rs `Orientation`): the driver as built for this host (its probe picks the tier, logged), then built with `ATL_EGL_TIER=blit` (the baseline, forced through the probe's input). Both must pass.
//! Breaks, each must fail: `fbo0` on both (framebuffer entries from the host, not the driver: framebuffer 0 is not the window); on a FlipY run `flip-y` (the surface FBO's flip-y undone: the frame stored upside down); on a Blit run the driver break `no_blit_flip` (the swap's copy not inverted).
//! On another host: copy `<out>/<host|blit>/egl/libGLES_atl.so` and the probe (`<out>/cargo/release/atl-egl-driver-probe`) and run `atl-egl-driver-probe --driver libGLES_atl.so [--break flip-y|fbo0]`; the probe links no workspace library dynamically.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};

const DRIVER_PACKAGE: &str = "atl-egl-driver";
const PROBE_PACKAGE: &str = "atl-egl-probe";
const PROBE_BIN: &str = "atl-egl-driver-probe";

#[derive(clap::Args)]
pub struct Args {
    /// Output directory (default: <workspace>/out/egl-driver/<checkout dir name>; never inside the checkout, whose files the static checks read).
    #[arg(long)]
    out: Option<PathBuf>,
    /// DRM node gralloc allocates on (default: gralloc's own trial, the node the driver renders on).
    #[arg(long)]
    alloc: Option<PathBuf>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = match &args.out {
        Some(o) => o.clone(),
        None => {
            let checkout = root.file_name().context("checkout dir name")?;
            atl_layout::Layout::of(root)?
                .workspace()
                .join("out/egl-driver")
                .join(checkout)
        }
    };
    let probe = build_probe(root, &out.join("cargo"))?;
    let stage = |sub: &str, tier: Option<&str>, brk: Option<&str>| -> Result<PathBuf> {
        let env = [(TIER_ENV, tier), (BREAK_ENV, brk)];
        let dir = crate::cdylib::stage_with(
            root,
            &out.join(sub),
            DRIVER_PACKAGE,
            "egl",
            atl_egl_driver_name(),
            &env,
        )?;
        Ok(dir.join(atl_egl_driver_name()))
    };
    // (passed, the probe's stdout, the tier the driver logged)
    let go = |driver: &Path, brk: Option<&str>| -> Result<(bool, String, String)> {
        let mut cmd = Command::new(&probe);
        cmd.arg("--driver").arg(driver);
        if let Some(a) = &args.alloc {
            cmd.arg("--alloc").arg(a);
        }
        if let Some(b) = brk {
            cmd.args(["--break", b]);
        }
        let o = cmd.output().context("running the probe")?;
        // the host EGL's own warnings and the driver's lines go to stderr; the probe's lines are on stdout
        let tier = String::from_utf8_lossy(&o.stderr)
            .lines()
            .find_map(|l| l.strip_prefix(TIER_LINE).map(str::to_owned))
            .unwrap_or_else(|| "none logged".to_owned());
        Ok((
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            tier,
        ))
    };
    let control = |name: &str, (passed, text, _): (bool, String, String), mark: &str| {
        let caught = !passed && text.contains(mark);
        println!(
            "egl-driver: break {name}: {}",
            if caught {
                "red (expected)"
            } else {
                "NOT caught"
            }
        );
        if !caught {
            print!("{text}");
            bail!("egl-driver: FAILED control: break {name} passed");
        }
        Ok(())
    };
    // the host's own tier, then the baseline forced through the probe's input (build.rs `TIERS`): every host runs the baseline
    for (sub, forced) in [("host", None), ("blit", Some("blit"))] {
        let driver = stage(sub, forced, None)?;
        let (ok, text, tier) = go(&driver, None)?;
        print!("{text}");
        println!("egl-driver: [{sub}] tier {tier}");
        if !ok || !text.contains("egl-driver: PASS") {
            bail!("egl-driver: FAILED ({sub} build, tier {tier})");
        }
        if tier.starts_with("flip_y") {
            control("flip-y", go(&driver, Some("flip-y"))?, ORIENTATION_RED)?;
        } else if tier.starts_with("blit") {
            let broken = stage(&format!("{sub}-no-blit-flip"), forced, Some("no_blit_flip"))?;
            control("no_blit_flip", go(&broken, None)?, ORIENTATION_RED)?;
        } else {
            bail!("egl-driver: FAILED ({sub} build): tier {tier}");
        }
        control("fbo0", go(&driver, Some("fbo0"))?, "FAIL")?;
    }
    println!("egl-driver: ok (both tiers; controls: flip-y or no_blit_flip, fbo0 red)");
    Ok(ExitCode::SUCCESS)
}

/// atl-egl-driver build.rs selectors, set explicitly per stage so the caller's environment never picks the tier or a break.
const TIER_ENV: &str = "ATL_EGL_TIER";
const BREAK_ENV: &str = "ATL_EGL_BREAK";
/// The driver's one stderr line naming the tier that ran (driver.rs `Driver::load`).
const TIER_LINE: &str = "libGLES_atl: window tier ";
const ORIENTATION_RED: &str = "orientation red";

/// The value of `ro.hardware.egl` that selects the driver: Loader.cpp opens `libGLES_<value>.so` in the driver dir.
const HARDWARE: &str = "atl";

/// The app process's GL driver, built and placed as the runtime root holds it: (host file, Android path) pairs -- `libGLES_atl.so` in /vendor's egl dir and a /vendor/build.prop with `ro.hardware.egl` (`<out>/egl`).
/// It links only system libraries (the host's libEGL is dlopened by atl-egl-host), so no library dir joins the path.
pub fn placement(root: &Path, out: &Path) -> Result<Vec<(PathBuf, String)>> {
    let dir = crate::cdylib::stage(root, out, DRIVER_PACKAGE, "egl", atl_egl_driver_name())?;
    let prop = dir.join("vendor-build.prop");
    std::fs::write(&prop, format!("ro.hardware.egl={HARDWARE}\n"))?;
    Ok(vec![
        (
            dir.join(atl_egl_driver_name()),
            format!(
                "{}/{}",
                atl_android_paths::VENDOR_EGL,
                atl_egl_driver_name()
            ),
        ),
        (prop, atl_android_paths::VENDOR_BUILD_PROP.to_owned()),
    ])
}

/// The file name AOSP's loader opens for `ro.hardware.egl=atl` (Loader.cpp `libGLES_<name>.so`).
pub fn atl_egl_driver_name() -> &'static str {
    "libGLES_atl.so"
}

fn build_probe(root: &Path, target: &Path) -> Result<PathBuf> {
    crate::unit_use::exec(
        crate::cargo_command()
            .current_dir(root)
            .args([
                "build",
                "--release",
                "-p",
                PROBE_PACKAGE,
                "--bin",
                PROBE_BIN,
                "--target-dir",
            ])
            .arg(target),
        "cargo build (egl-driver probe)",
    )?;
    let bin = target.join("release").join(PROBE_BIN);
    ensure!(bin.is_file(), "{} missing after cargo build", bin.display());
    Ok(bin)
}
