// SPDX-License-Identifier: MIT
//! `xtask test drive`: `run-apk --drive` on fixtures, judged from the PNGs it writes.
//! Stage 1, xtask-harness/testdata/window: the shot's pixels are the fixture's quadrants at the opening size and after a `resize`. Stage 2, xtask-harness/testdata/widgets: two shots before a tap on the Button are equal (nothing else moves the picture), and the shot after it differs.
//! Breaks must turn it red: `--break-blank` blanks every shot (the drive itself must fail), `--break-colour` swaps R and B of the expectation.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use atl_services_types::native_libs::SupportedAbi;

use crate::atld_run::{PackageFile, Runtime};
use crate::screencopy::Image;
use crate::{drive, run_apk, window};

/// `resize` target of the quadrant stage: even sides, so the halves are exact.
const RESIZED: (u32, u32) = (640, 400);
/// The Button's centre in content pixels (`AtlWindow: layout button left=16 top=49 width=88 height=48`, the fixture's layout).
const BUTTON: (u32, u32) = (60, 73);
/// How long the picture gets to settle: the button's press ripple is over well within it.
const SETTLE_MS: u32 = 3000;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Deliberate failure: every shot is blanked, so the drive must fail.
    #[arg(long)]
    break_blank: bool,
    /// Deliberate failure: the quadrants' expectation has R and B swapped, so the pixel check must fail.
    #[arg(long)]
    break_colour: bool,
}

/// The gate's work dir (emptied) and its one runtime: `new` stages it for the APK tools, `stage` configures it once, and every drive of the gate call runs on it.
struct Gate<'a> {
    root: &'a Path,
    rt: Runtime,
    /// The foreign ABI the staged APKs share (`run_apk::shared_foreign`); None for host-ISA APKs.
    foreign: Option<SupportedAbi>,
    work: PathBuf,
    host: &'a str,
    out: Option<PathBuf>,
    java_out: Option<PathBuf>,
    break_blank: bool,
}

impl<'a> Gate<'a> {
    fn new(
        root: &'a Path,
        host: &'a str,
        out: Option<PathBuf>,
        java_out: Option<PathBuf>,
        break_blank: bool,
    ) -> Result<Self> {
        // staged as run-apk: the drives are run-apk runs, and their remote record (Runtime::record_inputs) is run-apk's
        let rt = Runtime::new(
            root,
            "run-apk",
            host,
            out.as_deref(),
            java_out.as_deref(),
            run_apk::STAGE,
            false,
        )?;
        let work = rt.out.join("drive-gate");
        if work.exists() {
            std::fs::remove_dir_all(&work)?;
        }
        std::fs::create_dir_all(&work)?;
        Ok(Gate {
            root,
            rt,
            foreign: None,
            work,
            host,
            out,
            java_out,
            break_blank,
        })
    }

    fn apk(&self, fixture: &str, dir: &str) -> Result<PathBuf> {
        self.rt.apk.build(
            &self.root.join(fixture),
            &self.work.join(dir),
            &[],
            None,
            None,
        )
    }

    /// Configures the runtime once, for the APKs the gate's drives run (`run_apk::configure`).
    fn stage(self, apks: &[&Path]) -> Result<Self> {
        let first = apks.first().context("the gate stages no APK")?;
        let foreign = run_apk::shared_foreign(&self.rt, self.host, apks)?;
        let args = self.args(first);
        let rt = run_apk::configure(self.root, &args, self.rt, foreign.as_ref())?;
        Ok(Gate { rt, foreign, ..self })
    }

    fn args(&self, apk: &Path) -> run_apk::Args {
        run_apk::Args::driven(
            apk,
            self.host,
            self.out.clone(),
            self.java_out.clone(),
            self.break_blank,
        )
    }

    fn drive(&self, apk: &Path, name: &str, script: String) -> Result<ExitCode> {
        let file = self.work.join(name);
        std::fs::write(&file, &script)?;
        let steps = drive::parse(&script).with_context(|| file.display().to_string())?;
        run_apk::run_on(
            &self.rt,
            &self.args(apk),
            self.foreign.as_ref(),
            None,
            vec![PackageFile { path: apk.to_path_buf() }],
            Some(steps),
        )
    }
}

/// `cargo xtask check`'s form (check step drive-tap): one widgets run, first shot non-uniform (drive's own blank check) + one tap that changes pixels.
/// Catches a covered / unmapped window (blank first shot) and lost input (no change); the quadrants and the still-picture shots stay in `xtask test drive`.
pub fn quick(root: &Path, host: &str, out: &Path) -> Result<()> {
    let gate = Gate::new(root, host, Some(out.to_owned()), None, false)?;
    let widgets = gate.apk(window::WIDGETS_FIXTURE, "apk-widgets")?;
    let gate = gate.stage(&[&widgets])?;
    let (first, before, after) = (
        gate.work.join("first.png"),
        gate.work.join("before.png"),
        gate.work.join("after.png"),
    );
    let (x, y) = BUTTON;
    // the first shot is the blank check at first present; the tap and its comparison wait SETTLE_MS each side, as `run`'s tap stage:
    // 300 ms after the very first present failed 3/6 under load (dfs-loopL: tap delivered, a frame came, no pixel changed)
    let code = gate.drive(
        &widgets,
        "tap.drive",
        format!(
            "shot {}\nwait {SETTLE_MS}\nshot {}\ntap {x} {y}\nwait-frames 1\nwait {SETTLE_MS}\nshot {}\n",
            first.display(),
            before.display(),
            after.display()
        ),
    )?;
    ensure!(code == ExitCode::SUCCESS, "the tap drive failed");
    let changed = changed_pixels(&Image::read_png(&before)?, &Image::read_png(&after)?);
    ensure!(changed > 0, "the tap changed no pixel");
    println!("drive-tap: the tap changed {changed} pixels");
    Ok(())
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let gate = Gate::new(
        root,
        &args.host,
        args.out.clone(),
        args.java_out.clone(),
        args.break_blank,
    )?;
    let work = gate.work.clone();
    let quadrants = gate.apk(window::FIXTURE, "apk-window")?;
    let widgets = gate.apk(window::WIDGETS_FIXTURE, "apk-widgets")?;
    let gate = gate.stage(&[&quadrants, &widgets])?;
    let drive = |apk: &Path, name: &str, script: String| gate.drive(apk, name, script);
    let (open, resized) = (work.join("open.png"), work.join("resized.png"));
    let (w, h) = RESIZED;
    let code = drive(
        &quadrants,
        "quadrants.drive",
        format!(
            "shot {}\nresize {w} {h}\nwait-frames 1\nwait {SETTLE_MS}\nshot {}\n",
            open.display(),
            resized.display()
        ),
    )?;
    ensure!(code == ExitCode::SUCCESS, "the quadrant drive failed");
    let opened = Image::read_png(&open)?;
    let got = Image::read_png(&resized)?;
    ensure!(
        (got.width, got.height) == RESIZED,
        "resized shot is {}x{}, wanted {w}x{h}",
        got.width,
        got.height
    );
    for (name, image) in [("open", &opened), ("resized", &got)] {
        let wrong = quadrant_mismatches(image, args.break_colour);
        ensure!(wrong == 0, "{name} shot: {wrong} pixels off the quadrants");
        println!(
            "drive gate: {name} {}x{} = the quadrants",
            image.width, image.height
        );
    }
    let (before, still, after) = (
        work.join("before.png"),
        work.join("still.png"),
        work.join("after.png"),
    );
    let (x, y) = BUTTON;
    let code = drive(
        &widgets,
        "tap.drive",
        format!(
            "wait {SETTLE_MS}\nshot {}\nwait {SETTLE_MS}\nshot {}\ntap {x} {y}\nwait-frames 1\nwait {SETTLE_MS}\nshot {}\n",
            before.display(),
            still.display(),
            after.display()
        ),
    )?;
    ensure!(code == ExitCode::SUCCESS, "the tap drive failed");
    let (a, b, c) = (
        Image::read_png(&before)?,
        Image::read_png(&still)?,
        Image::read_png(&after)?,
    );
    ensure!(a == b, "the picture moved with no input");
    let changed = changed_pixels(&a, &c);
    ensure!(changed > 0, "the tap changed no pixel");
    println!("drive gate: the tap changed {changed} pixels");
    println!("drive gate: ok, shots in {}", work.display());
    Ok(ExitCode::SUCCESS)
}

/// Pixels of `image` other than the quadrants at its size (window::quadrant_pixels, the fixture's spec). `swap_rb`: the break.
fn quadrant_mismatches(image: &Image, swap_rb: bool) -> usize {
    let mut want = window::quadrant_pixels(image.width, image.height);
    if swap_rb {
        want.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|p| p.swap(0, 2));
    }
    image
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(want.as_chunks::<4>().0)
        .filter(|(got, want)| got != want)
        .count()
}

fn changed_pixels(a: &Image, b: &Image) -> usize {
    a.rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.rgba.as_chunks::<4>().0)
        .filter(|(x, y)| x != y)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDE: u32 = 64;

    fn quadrants() -> Image {
        Image {
            width: SIDE,
            height: SIDE,
            rgba: window::quadrant_pixels(SIDE, SIDE),
        }
    }

    #[test]
    fn quadrants_pass_and_a_wrong_pixel_fails() {
        let mut image = quadrants();
        assert_eq!(quadrant_mismatches(&image, false), 0);
        image.rgba[0] = 0;
        assert_eq!(quadrant_mismatches(&image, false), 1);
    }

    /// The swap moves the red and blue quadrants only: green and white keep their bytes.
    #[test]
    fn the_colour_break_fails_red_and_blue() {
        let quadrant = (SIDE / 2) * (SIDE / 2);
        assert_eq!(
            quadrant_mismatches(&quadrants(), true),
            2 * quadrant as usize
        );
    }
}
