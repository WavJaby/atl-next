// SPDX-License-Identifier: MIT
//! `xtask test vk-driver`: the Vulkan HAL module (crate atl-vulkan-driver, `vulkan.atl.so`) loaded as AOSP's libvulkan loads it, driving this host's ICD: instance -> physical devices -> device -> queue, every handle's dispatch slot judged as libvulkan judges it (atl-vulkan-driver-probe's module doc).
//! Runs on this host's own ICDs: lavapipe on WSL, the GPU's ICD on a desktop. No native build and no slot: the driver and the probe are plain cargo outputs (in --out).
//! Runs, each judged: plain (PASS with exactly 1 physical device exposed; the driver's `vulkan.atl: icd ...` line names the ICD, device, tier and gralloc node); control `magic` (the probe expects another slot value: every slot red); environment break `node` (gralloc's node is /dev/null, chosen by the display: no device may be exposed, the refusal names both sides); environment break `no-icd` (HOME and the XDG dirs point at an empty dir, so discovery finds no manifest: the named refusal, open -ENODEV). The last is not caught on a host with manifests in /etc/vulkan/icd.d (a fixed search dir): the gate says so and fails.
//! On another host: copy `<out>/vk/vulkan.atl.so` and the probe (`<out>/cargo/release/atl-vulkan-driver-probe`) and run `atl-vulkan-driver-probe --driver vulkan.atl.so --node /dev/dri/renderD<n> --node-source display|trial [--break magic]`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};

const PACKAGE: &str = "atl-vulkan-driver";
const PROBE_BIN: &str = "atl-vulkan-driver-probe";
/// The file libvulkan opens for `ro.hardware.vulkan=atl` (driver.cpp `LoadDriver`: `vulkan.<name>.so`).
const DRIVER_FILE: &str = "vulkan.atl.so";
/// The driver's stderr line naming what it chose (hal.rs `device`).
const ICD_LINE: &str = "vulkan.atl: icd ";
/// The driver's refusal when discovery finds no manifest (icd.rs `choose`).
const NO_ICD: &str = "vulkan.atl: no device: no ICD manifest";
/// The probe's failure when a slot differs from the expected value.
const SLOT_RED: &str = "dispatch slot not";

/// The driver's refusal when no ICD has a device on gralloc's node (device.rs `pick`).
const REFUSED: &str = "no device on gralloc's node";
/// The node break's stand-in for gralloc's node: a character device no GPU renders on (mem 1:3), named as chosen by the display, so neither tier may expose a device on any host.
const NO_GPU_NODE: &str = "/dev/null";
/// Where render nodes live (drm_drv.c `renderD<minor>`).
const DRI_DIR: &str = "/dev/dri";

#[derive(clap::Args)]
pub struct Args {
    /// gralloc's node for the plain run (default: the first `/dev/dri/renderD*` by name, source `trial`: what gralloc's trial picks on a single-node host such as WSL's vgem; name the node on a multi-GPU host).
    #[arg(long)]
    node: Option<PathBuf>,
    /// How `--node` was chosen: display or trial.
    #[arg(long, default_value = "trial")]
    node_source: String,
    /// Output directory (default: <workspace>/out/vk-driver/<checkout dir name>; never inside the checkout, whose files the static checks read).
    #[arg(long)]
    out: Option<PathBuf>,
}

struct Run {
    passed: bool,
    stdout: String,
    stderr: String,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = match &args.out {
        Some(o) => o.clone(),
        None => {
            let checkout = root.file_name().context("checkout dir name")?;
            atl_layout::Layout::of(root)?
                .workspace()
                .join("out/vk-driver")
                .join(checkout)
        }
    };
    let driver = crate::cdylib::stage(root, &out, PACKAGE, "vk", DRIVER_FILE)?.join(DRIVER_FILE);
    let probe = build_probe(root, &out.join("cargo"))?;
    let node = match &args.node {
        Some(n) => n.clone(),
        None => first_render_node()?,
    };
    let plain_node = (node.as_path(), args.node_source.as_str());
    let go = |brk: Option<&str>, env_dir: Option<&Path>, gralloc: (&Path, &str)| -> Result<Run> {
        let mut cmd = Command::new(&probe);
        cmd.arg("--driver").arg(&driver);
        cmd.arg("--node")
            .arg(gralloc.0)
            .args(["--node-source", gralloc.1]);
        if let Some(b) = brk {
            cmd.args(["--break", b]);
        }
        if let Some(d) = env_dir {
            for name in [
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_CONFIG_DIRS",
                "XDG_DATA_HOME",
                "XDG_DATA_DIRS",
            ] {
                cmd.env(name, d);
            }
        }
        let o = cmd.output().context("running the probe")?;
        Ok(Run {
            passed: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        })
    };

    let plain = go(None, None, plain_node)?;
    print!("{}", plain.stdout);
    let icd = plain
        .stderr
        .lines()
        .find_map(|l| l.strip_prefix(ICD_LINE))
        .unwrap_or("none logged")
        .to_owned();
    println!("vk-driver: icd {icd}");
    if !plain.passed || !plain.stdout.contains("vk-driver: PASS") {
        eprint_stderr(&plain);
        bail!("vk-driver: FAILED (plain run)");
    }

    // control: the judge must discriminate
    let magic = go(Some("magic"), None, plain_node)?;
    control("magic", &magic, &magic.stdout, SLOT_RED)?;

    // environment break: gralloc's node has no Vulkan device -> 0 devices, both sides named
    let no_gpu = go(None, None, (Path::new(NO_GPU_NODE), "display"))?;
    control("node", &no_gpu, &no_gpu.stderr, REFUSED)?;

    // environment break: no manifest anywhere discovery looks
    let empty = out.join("no-icd");
    std::fs::create_dir_all(&empty)?;
    ensure!(
        std::fs::read_dir(&empty)?.next().is_none(),
        "{} is not empty",
        empty.display()
    );
    let no_icd = go(None, Some(&empty), plain_node)?;
    control("no-icd", &no_icd, &no_icd.stderr, NO_ICD)?;

    println!("vk-driver: ok (icd {icd}; controls: magic, node, no-icd red)");
    Ok(ExitCode::SUCCESS)
}

/// The first `renderD*` in [`DRI_DIR`] by name.
fn first_render_node() -> Result<PathBuf> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir(DRI_DIR)
        .with_context(|| format!("reading {DRI_DIR}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
        .into_iter()
        .next()
        .with_context(|| format!("no render node in {DRI_DIR}: name one with --node"))
}

#[allow(
    clippy::print_stderr,
    reason = "the failed run's own log, for the reader"
)]
fn eprint_stderr(r: &Run) {
    eprint!("{}", r.stderr);
}

/// A break run must fail, with `mark` in `text`: its line is printed as the evidence.
fn control(name: &str, r: &Run, text: &str, mark: &str) -> Result<()> {
    let hit = (!r.passed)
        .then(|| text.lines().find(|l| l.contains(mark)))
        .flatten();
    match hit {
        Some(line) => {
            println!("vk-driver: break {name}: red (expected): {}", line.trim());
            Ok(())
        }
        None => {
            println!("vk-driver: break {name}: NOT caught");
            print!("{}", r.stdout);
            eprint_stderr(r);
            bail!("vk-driver: FAILED control: break {name} passed")
        }
    }
}

fn build_probe(root: &Path, target: &Path) -> Result<PathBuf> {
    crate::unit_use::exec(
        crate::cargo_command()
            .current_dir(root)
            .args([
                "build",
                "--release",
                "-p",
                PACKAGE,
                "--bin",
                PROBE_BIN,
                "--target-dir",
            ])
            .arg(target),
        "cargo build (vk-driver probe)",
    )?;
    let bin = target.join("release").join(PROBE_BIN);
    ensure!(bin.is_file(), "{} missing after cargo build", bin.display());
    Ok(bin)
}
