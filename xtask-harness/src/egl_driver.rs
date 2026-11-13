// SPDX-License-Identifier: MIT
//! `xtask test egl-driver`: the GLES driver over the host's system EGL (crate atl-egl-driver, `libGLES_atl.so`) rendering into an ANativeWindow, every pixel checked, orientation included (atl-egl-driver-probe's module doc has the scenario and the judge).
//! Runs on this host's own EGL: llvmpipe on WSL, the GPU of gralloc's node on a desktop. No native build and no slot: the driver and the probe are plain cargo outputs (in --out).
//! Two runs, one per orientation tier (driver.rs `Orientation`): the driver as built for this host (its probe picks the tier, logged), then built with `ATL_EGL_TIER=blit` (the baseline, forced through the probe's input). Both must pass.
//! Breaks, each must fail: `fbo0` on both (framebuffer entries from the host, not the driver: framebuffer 0 is not the window); on a FlipY run `flip-y` (the surface FBO's flip-y undone: the frame stored upside down); on a Blit run the driver break `no_blit_flip` (the swap's copy not inverted); once, the driver break `no_device_check` (a buffer whose handle names another DRM device is imported).
//! Destination tier (driver.rs `Destination`): those runs must log `buffer tier import` (this host's renderer imports gralloc's buffers). With `--render-env` (a renderer that cannot import, chosen environment-side) two more runs, host and blit builds, must log `buffer tier readback` and pass every pixel; breaks `no_readback` (each) and `force_import` (once) must fail.
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
    /// KEY=VALUE for the rendering side only (repeat; the probe's `--render-env`, set after its gralloc allocation, as atld `--app-env` reaches app processes and not the allocator): test-environment setup choosing a host renderer that cannot import gralloc's buffers, e.g. Mesa's `GALLIUM_DRIVER=d3d12` on WSL. Given -> two more runs on the driver's readback destination tier (host and blit builds), which must log `buffer tier readback`, pass every pixel, and turn red under `no_readback` and `force_import`.
    #[arg(long = "render-env", value_name = "KEY=VALUE")]
    render_env: Vec<String>,
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
    // (passed, the probe's stdout, the window tier the driver logged + its buffer tier and renderer lines, when logged); `render` = the readback runs' render-side environment
    let run_probe =
        |driver: &Path, brk: Option<&str>, render: bool| -> Result<(bool, String, String)> {
            let mut cmd = Command::new(&probe);
            cmd.arg("--driver").arg(driver);
            if let Some(a) = &args.alloc {
                cmd.arg("--alloc").arg(a);
            }
            if let Some(b) = brk {
                cmd.args(["--break", b]);
            }
            if render {
                for kv in &args.render_env {
                    cmd.arg("--render-env").arg(kv);
                }
                cmd.arg("--cross-device");
            }
            let o = cmd.output().context("running the probe")?;
            // the host EGL's own warnings and the driver's lines go to stderr; the probe's lines are on stdout
            let stderr = String::from_utf8_lossy(&o.stderr);
            let line = |prefix: &str| {
                stderr
                    .lines()
                    .find_map(|l| l.strip_prefix(prefix).map(str::to_owned))
            };
            let mut tier = line(TIER_LINE).unwrap_or_else(|| "none logged".to_owned());
            for prefix in [BUFFER_TIER_LINE, RENDERER_LINE] {
                if let Some(v) = line(prefix) {
                    tier.push_str(&format!("; {prefix}{v}"));
                }
            }
            Ok((
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).into_owned(),
                tier,
            ))
        };
    let go = |driver: &Path, brk: Option<&str>| run_probe(driver, brk, false);
    let control = |name: &str, (passed, text, _): (bool, String, String), mark: &str| {
        // the probe's own line that caught the break, verbatim: the evidence, not just the verdict
        let hit = (!passed)
            .then(|| text.lines().find(|l| l.contains(mark)))
            .flatten();
        match hit {
            Some(line) => println!("egl-driver: break {name}: red (expected): {}", line.trim()),
            None => println!("egl-driver: break {name}: NOT caught"),
        }
        let caught = hit.is_some();
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
        // the baseline destination on this host's own renderer: the readback tier must not run where import works
        ensure!(
            tier.contains(&format!("{BUFFER_TIER_LINE}import")),
            "egl-driver: FAILED ({sub} build): want buffer tier import, got {tier}"
        );
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
    // the wrong-device refusal is tier-independent: one break run on the host build
    let broken = stage("host-no-device-check", None, Some("no_device_check"))?;
    control("no_device_check", go(&broken, None)?, WRONG_DEVICE_RED)?;
    if !args.render_env.is_empty() {
        // the readback destination tier on both orientation tiers, chosen by the driver's own probe under the render-side environment
        for (sub, forced) in [("readback", None), ("readback-blit", Some("blit"))] {
            let driver = stage(sub, forced, None)?;
            let (ok, text, tier) = run_probe(&driver, None, true)?;
            print!("{text}");
            println!("egl-driver: [{sub}] tier {tier}");
            if !ok || !text.contains("egl-driver: PASS") {
                bail!("egl-driver: FAILED ({sub} build, tier {tier})");
            }
            ensure!(
                tier.contains(&format!("{BUFFER_TIER_LINE}readback")),
                "egl-driver: FAILED ({sub} build): want buffer tier readback under {:?}, got {tier}",
                args.render_env
            );
            let broken = stage(&format!("{sub}-no-readback"), forced, Some("no_readback"))?;
            control(
                "no_readback",
                run_probe(&broken, None, true)?,
                ORIENTATION_RED,
            )?;
        }
        let broken = stage("readback-force-import", None, Some("force_import"))?;
        control(
            "force_import",
            run_probe(&broken, None, true)?,
            FORCED_IMPORT_RED,
        )?;
    }
    let readback = if args.render_env.is_empty() {
        ""
    } else {
        "; readback destination on both tiers, controls no_readback, force_import red"
    };
    println!(
        "egl-driver: ok (both tiers; controls: flip-y or no_blit_flip, fbo0, no_device_check red{readback})"
    );
    Ok(ExitCode::SUCCESS)
}

/// atl-egl-driver build.rs selectors, set explicitly per stage so the caller's environment never picks the tier or a break.
const TIER_ENV: &str = "ATL_EGL_TIER";
const BREAK_ENV: &str = "ATL_EGL_BREAK";
/// The driver's one stderr line naming the tier that ran (driver.rs `Driver::load`).
const TIER_LINE: &str = "libGLES_atl: window tier ";
/// The driver's line naming the destination tier its first-buffer probe chose (surface.rs `probe_destination`).
const BUFFER_TIER_LINE: &str = "libGLES_atl: buffer tier ";
/// The driver's line naming the device and `GL_RENDERER` (driver.rs `log_renderer_once`).
const RENDERER_LINE: &str = "libGLES_atl: device ";
/// The probe's failure when the destination probe is skipped on a renderer that cannot import: the first make-current fails EGL_BAD_ALLOC (0x3003), named, not a hang.
const FORCED_IMPORT_RED: &str = "EGL error 0x3003";
const ORIENTATION_RED: &str = "orientation red";
/// The probe's failure when the driver imports a buffer naming another DRM device (atl-egl-driver-probe `wrong_device`).
const WRONG_DEVICE_RED: &str = "wrong-device import accepted";

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
