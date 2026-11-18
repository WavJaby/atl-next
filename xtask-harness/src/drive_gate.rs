// SPDX-License-Identifier: MIT
//! `xtask test drive`: `run-apk --drive` on fixtures, the drive engine's own gate.
//! Stage 1, xtask-harness/testdata/window: the PNG shots' pixels are the fixture's quadrants at the opening size and after a `resize` (pixel-exact, judged here). Stage 2, xtask-harness/testdata/widgets: the script's own checkpoints
//! (`expect-same` before a tap on the Button: nothing else moves the picture; `expect-changed` after it).
//! Breaks must turn it red: `--break-blank` blanks every shot (the drive itself must fail), `--break-colour` swaps R and B of the expectation, `--break-input` drops the injected input (stage 2 fails at its `expect-changed`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use atl_services_types::native_libs::SupportedAbi;
use xtask::timing;

use crate::atld_run::{PackageFile, Runtime};
use crate::screencopy::Image;
use crate::{drive, run_apk, window};

/// `resize` target of the quadrant stage: even sides, so the halves are exact.
const RESIZED: (u32, u32) = (640, 400);
/// The Button's centre in content pixels (`AtlWindow: layout button left=16 top=49 width=88 height=48`, the fixture's layout).
const BUTTON: (u32, u32) = (60, 73);
/// The label the click retitles, as a drive region `X Y W H` (`AtlWindow: layout label left=16 top=16 width=1408 height=33`): the tap's lasting change (onButton), judged there, since in touch mode
/// it is 0.112 % of the content, under CHANGED_FRACTION of the whole.
const LABEL: &str = "16 16 1408 33";
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
    /// Deliberate failure: `run-apk --break-input`, the proxy drops every injected event, so the widgets stage must fail at its `expect-changed`.
    #[arg(long)]
    break_input: bool,
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
    break_input: bool,
}

impl<'a> Gate<'a> {
    fn new(
        root: &'a Path,
        host: &'a str,
        out: Option<PathBuf>,
        java_out: Option<PathBuf>,
        (break_blank, break_input): (bool, bool),
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
            break_input,
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
        Ok(Gate {
            rt,
            foreign,
            ..self
        })
    }

    fn args(&self, apk: &Path) -> run_apk::Args {
        run_apk::Args::driven(
            apk,
            self.host,
            self.out.clone(),
            self.java_out.clone(),
            self.break_blank,
            self.break_input,
        )
    }

    fn drive(&self, apk: &Path, name: &str, script: String) -> Result<ExitCode> {
        let file = self.work.join(name);
        std::fs::write(&file, &script)?;
        let script = drive::parse(&script).with_context(|| file.display().to_string())?;
        run_apk::run_on(
            &self.rt,
            &self.args(apk),
            self.foreign.as_ref(),
            None,
            vec![PackageFile {
                path: apk.to_path_buf(),
            }],
            Some(script),
        )
        .map(|ran| ran.code())
    }
}

/// `cargo xtask check`'s form (check step drive-tap): one widgets run, first shot non-uniform (drive's own blank check) + one tap that changes pixels.
/// Catches a covered / unmapped window (blank first shot) and lost input (no change); the quadrants and the still-picture shots stay in `xtask test drive`.
/// Prints the phase report on stderr (the check log shows it), also when the run fails: the phases say where the time went up to it.
pub fn quick(root: &Path, host: &str, out: &Path) -> Result<()> {
    let load_start = load1();
    let result = timing::phase("drive-tap", || quick_run(root, host, out));
    eprintln!("drive-tap load1: start {load_start}, end {}", load1());
    eprintln!("{}", timing::report("drive-tap").trim_end());
    result
}

/// The 1-minute load average from /proc/loadavg, as the kernel prints it; `?` when unreadable.
fn load1() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "?".to_owned())
}

fn quick_run(root: &Path, host: &str, out: &Path) -> Result<()> {
    let gate = timing::phase("runtime: new", || {
        Gate::new(root, host, Some(out.to_owned()), None, (false, false))
    })?;
    let widgets = timing::phase("apk: build and sign", || {
        gate.apk(window::WIDGETS_FIXTURE, "apk-widgets")
    })?;
    let gate = timing::phase("stage: configure", || gate.stage(&[&widgets]))?;
    // `expect-varied` is the blank check at first present; the tap and its comparison wait SETTLE_MS each side, as `run`'s tap stage:
    // 300 ms after the very first present failed 3/6 under load (dfs-loopL: tap delivered, a frame came, no pixel changed)
    let code = timing::phase("drive: run-apk", || {
        gate.drive(
            &widgets,
            "tap.drive",
            format!(
                "expect-varied\nwait {SETTLE_MS}\nmark before\n{}",
                tap_script()
            ),
        )
    })?;
    ensure!(code == ExitCode::SUCCESS, "the tap drive failed");
    Ok(())
}

/// The Button's tap and its checkpoint against the mark `before`. No `wait-frames` between: under `--break-input` nothing redraws, and the red must come at `expect-changed`, not at a frame wait.
fn tap_script() -> String {
    let (x, y) = BUTTON;
    format!("tap {x} {y}\nwait {SETTLE_MS}\nexpect-changed before {LABEL}\n")
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let gate = Gate::new(
        root,
        &args.host,
        args.out.clone(),
        args.java_out.clone(),
        (args.break_blank, args.break_input),
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
    let code = drive(
        &widgets,
        "tap.drive",
        format!(
            "wait {SETTLE_MS}\nmark before\nwait {SETTLE_MS}\nexpect-same before\n{}",
            tap_script()
        ),
    )?;
    ensure!(code == ExitCode::SUCCESS, "the tap drive failed");
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
