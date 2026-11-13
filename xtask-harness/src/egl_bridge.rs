// SPDX-License-Identifier: MIT
//! `xtask test egl-bridge`: eglGetProcAddress through the identity engine (GS2.2 G1). An NDK x86_64 fixture (`xtask-harness/testdata/egl-bridge/fixture.c`, DT_NEEDED libEGL.so and libGLESv2.so -> the guest proxies) asks eglGetProcAddress for glVertexAttrib4f, glTexImage2D, glFinish, a host-described name with no row, a name nobody has, and a name with no prototype (the trap run). The host side is the recorder (atl-native-bridge example `egl_recorder`), staged as `/system/lib64/libEGL.so` and `libGLESv2.so` under the bwrap root (qemu_linker.rs `system_wrap`), no product change.
//! Harness: `cargo test -p atl-native-bridge --test egl_bridge` (harness-less; checks a, f, b, d, e and the trap run c in its header), env as `nb-identity` (nb_identity.rs `identity_env`) with the fixture dir as the test library dir.
//! Runs: plain (every check ok, exit 0); each break (`ATL_NB_BREAK`, atl-native-bridge build.rs `BREAKS`) turns its check red and exits nonzero: `egl_raw` -> a, `egl_nodedup` -> e; the trap run (`ATL_EGL_CASE=trap`) must abort naming glAtlNoDescriptor (check c).
//! Output: `<out>/egl-bridge/`, one log per run. Default: mutating (builds the fixture, the recorder and the test; a break run rebuilds the package with its cfg).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::nb_identity::{self, ARCH};
use crate::ndk::NdkTarget;
use crate::qemu_linker;
use crate::slot;
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{Failure, Verdict};

const PACKAGE: &str = "atl-native-bridge";
const TEST: &str = "egl_bridge";
const RECORDER: &str = "egl_recorder";
const TESTDATA: &str = "xtask-harness/testdata/egl-bridge";
/// The prefix of the test's check lines (`egl-bridge: (<c>) ok`).
const PREFIX: &str = "egl-bridge";
const CHECKS: [char; 5] = ['a', 'b', 'd', 'e', 'f'];
/// (break, the check it must turn red): GS2.2 G1 break column.
const BREAKS: [(&str, char); 2] = [("egl_raw", 'a'), ("egl_nodedup", 'e')];
/// The trap run's stderr line: the named trap of a name no prototype describes (engine/proc.rs `named_trap`).
pub(crate) const TRAP_LINE: &str =
    "eglGetProcAddress(glAtlNoDescriptor): no host binding (no prototype descriptor)";
/// NDK API of the fixture: the lowest x86_64 one.
const API: u32 = 21;
/// Per run; a run that outlives it (a hung run) counts as failed.
const RUN_TIMEOUT_SECS: u32 = 600;

#[derive(clap::Args)]
pub struct Args {
    /// Native out dir holding `nb/x86_64` and `app/` (default: this checkout's slot, see `xtask slot`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Only the plain run and the trap run, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |ws| {
        ws.join("out/native").join(ARCH)
    })?;
    let dir = out.join("egl-bridge");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let fixture_dir = fixture(root, &dir)?;
    let env = nb_identity::identity_env(root, &out, fixture_dir)?;
    let stage = stage_recorder(root, &dir)?;
    let wrap = qemu_linker::system_wrap(&stage)?;
    let mut runs: Vec<Option<(&str, char)>> = vec![None];
    if !args.no_breaks {
        runs.extend(BREAKS.iter().map(|b| Some(*b)));
    }
    let total = runs.len() + 1;
    let mut fails = Vec::new();
    for brk in runs {
        let name = format!("{PREFIX}-{}", brk.map_or("plain", |b| b.0));
        if let Err(e) = one(root, &dir, &env, &wrap, brk, &name) {
            fails.push(Failure::step(&name, format!("{e:#}")));
        }
    }
    let trap = format!("{PREFIX}-trap");
    if let Err(e) = trap_run(root, &dir, &env, &wrap) {
        fails.push(Failure::step(&trap, format!("{e:#}")));
    }
    let verdict = Verdict {
        cmd: "egl-bridge",
        total: Some((total, "runs")),
        skipped: Vec::new(),
        failures: fails,
    };
    println!("{}", verdict.line());
    Ok(if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `fixture.c` as `<dir>/fixture/libegl-fixture.so` (NDK clang, -lEGL -lGLESv2: DT_NEEDED the two proxies); returns its dir.
fn fixture(root: &Path, dir: &Path) -> Result<PathBuf> {
    let ndk = crate::ndk::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, ARCH, API)?;
    let fdir = dir.join("fixture");
    std::fs::create_dir_all(&fdir).with_context(|| format!("creating {}", fdir.display()))?;
    let status = target
        .cmd("clang")
        .args(["-shared", "-fPIC", "-O1", "-Wall", "-Werror", "-o"])
        .arg(fdir.join("libegl-fixture.so"))
        .arg(root.join(TESTDATA).join("fixture.c"))
        .args(["-lEGL", "-lGLESv2"])
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {TESTDATA}/fixture.c");
    Ok(fdir)
}

/// Builds the recorder and stages it as `<dir>/root/system/lib64/{libEGL,libGLESv2}.so` (one file, both names); returns the stage root. The QEMU gate (`qemu_gl.rs`) stages it the same way for its fallback checks.
pub(crate) fn stage_recorder(root: &Path, dir: &Path) -> Result<PathBuf> {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args(["build", "--locked", "-p", PACKAGE, "--example", RECORDER])
        .env_remove("ATL_NB_BREAK")
        .current_dir(root);
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("building the recorder")?;
    ensure!(
        run.status.success(),
        "cargo build --example {RECORDER}: {}\n{}",
        run.status,
        run.stderr
    );
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let so = target.join(format!("debug/examples/lib{RECORDER}.so"));
    let stage = dir.join("root");
    let lib = stage.join(atl_android_paths::SYSTEM_LIB.trim_start_matches('/'));
    std::fs::create_dir_all(&lib).with_context(|| format!("creating {}", lib.display()))?;
    for name in ["libEGL.so", "libGLESv2.so"] {
        std::fs::copy(&so, lib.join(name))
            .with_context(|| format!("copying {} to {name}", so.display()))?;
    }
    Ok(stage)
}

/// One run under `wrap`: plain = every check ok and exit 0; break `(name, c)` = check `c` not ok and a nonzero exit.
fn one(
    root: &Path,
    dir: &Path,
    env: &[(&'static str, OsString)],
    wrap: &[OsString],
    brk: Option<(&str, char)>,
    name: &str,
) -> Result<()> {
    let mut cmd = Command::new("timeout");
    cmd.args(["-s", "KILL"])
        .arg(RUN_TIMEOUT_SECS.to_string())
        .args(wrap)
        .arg(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["test", "--locked", "-p", PACKAGE, "--test", TEST])
        .envs(env.iter().map(|(k, v)| (*k, v)))
        .current_dir(root);
    match brk {
        Some((b, _)) => cmd.env("ATL_NB_BREAK", b),
        None => cmd.env_remove("ATL_NB_BREAK"),
    };
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("running cargo test")?;
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &log,
        [run.stdout.as_bytes(), run.stderr.as_bytes()].concat(),
    )?;
    let ok = |c: char| {
        run.stdout
            .lines()
            .any(|l| l == format!("{PREFIX}: ({c}) ok"))
    };
    match brk {
        None => {
            let red: Vec<char> = CHECKS.iter().copied().filter(|&c| !ok(c)).collect();
            ensure!(
                red.is_empty() && run.status.success(),
                "checks {red:?} not ok ({}), log {}",
                run.status,
                log.display()
            );
        }
        Some((b, c)) => {
            ensure!(!ok(c), "break {b} left check {c} ok, log {}", log.display());
            ensure!(
                !run.status.success(),
                "break {b} exited 0, log {}",
                log.display()
            );
        }
    }
    Ok(())
}

/// The trap run (check c): `ATL_EGL_CASE=trap` calls the named trap; the process must abort (nonzero exit) with its stderr naming glAtlNoDescriptor.
fn trap_run(
    root: &Path,
    dir: &Path,
    env: &[(&'static str, OsString)],
    wrap: &[OsString],
) -> Result<()> {
    let mut cmd = Command::new("timeout");
    cmd.args(["-s", "KILL"])
        .arg(RUN_TIMEOUT_SECS.to_string())
        .args(wrap)
        .arg(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["test", "--locked", "-p", PACKAGE, "--test", TEST])
        .envs(env.iter().map(|(k, v)| (*k, v)))
        .env("ATL_EGL_CASE", "trap")
        .env_remove("ATL_NB_BREAK")
        .current_dir(root);
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("running cargo test")?;
    let log = dir.join(format!("{PREFIX}-trap.log"));
    std::fs::write(
        &log,
        [run.stdout.as_bytes(), run.stderr.as_bytes()].concat(),
    )?;
    ensure!(
        !run.status.success(),
        "the named trap did not abort ({}), log {}",
        run.status,
        log.display()
    );
    ensure!(
        run.stderr.contains(TRAP_LINE),
        "the abort does not name glAtlNoDescriptor (`{TRAP_LINE}`), log {}",
        log.display()
    );
    Ok(())
}
