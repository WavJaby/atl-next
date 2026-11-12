// SPDX-License-Identifier: MIT
//! `xtask test sles-bridge`: OpenSL ES through the identity engine (opensl-es plan slice O1). An NDK x86_64 fixture (`xtask-harness/testdata/sles-bridge/fixture.c`, DT_NEEDED libOpenSLES.so -> the guest proxy) drives a host recorder SL library (atl-native-bridge example `sles_recorder`), which the bridge opens as `/system/lib64/libOpenSLES.so` by absolute path: the run sees a bwrap root whose `/system` is `<out>/sles-bridge/root/system` (qemu_linker.rs `system_wrap`), no product change.
//! Harness: `cargo test -p atl-native-bridge --test sles_bridge` (harness-less; checks a-g in its header), env as `nb-identity` (nb_identity.rs `identity_env`) with the fixture dir as the test library dir.
//! Runs: plain (every check ok, exit 0), then each break (`ATL_NB_BREAK`, atl-native-bridge build.rs `BREAKS`) must turn its named check red and exit nonzero: `sles_raw_sink` -> c, `sles_no_dedup` -> e, `sles_var_skip` -> f.
//! Output: `<out>/sles-bridge/`, one log per run. Default: mutating (builds the fixture, the recorder and the test; a break run rebuilds the package with its cfg).

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
const TEST: &str = "sles_bridge";
const RECORDER: &str = "sles_recorder";
const TESTDATA: &str = "xtask-harness/testdata/sles-bridge";
/// The prefix of the test's check lines (`sles-bridge: (<c>) ok`).
const PREFIX: &str = "sles-bridge";
const CHECKS: [char; 7] = ['a', 'b', 'c', 'd', 'e', 'f', 'g'];
/// (break, the check it must turn red): opensl-es plan O1 break column.
const BREAKS: [(&str, char); 3] = [
    ("sles_raw_sink", 'c'),
    ("sles_no_dedup", 'e'),
    ("sles_var_skip", 'f'),
];
/// NDK API of the fixture: the lowest x86_64 one (libOpenSLES is in every NDK API from 9).
const API: u32 = 21;
/// Per run; a run that outlives it (a hung callback thread) counts as failed.
const RUN_TIMEOUT_SECS: u32 = 600;

#[derive(clap::Args)]
pub struct Args {
    /// Native out dir holding `nb/x86_64` and `app/` (default: this checkout's slot, see `xtask slot`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Only the plain run, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |ws| {
        ws.join("out/native").join(ARCH)
    })?;
    let dir = out.join("sles-bridge");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let fixture_dir = fixture(root, &dir)?;
    let env = nb_identity::identity_env(root, &out, fixture_dir)?;
    let stage = recorder(root, &dir)?;
    let wrap = qemu_linker::system_wrap(&stage)?;
    let mut runs: Vec<Option<(&str, char)>> = vec![None];
    if !args.no_breaks {
        runs.extend(BREAKS.iter().map(|b| Some(*b)));
    }
    let total = runs.len();
    let mut fails = Vec::new();
    for brk in runs {
        let name = format!("{PREFIX}-{}", brk.map_or("plain", |b| b.0));
        if let Err(e) = one(root, &dir, &env, &wrap, brk, &name) {
            fails.push(Failure::step(&name, format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "sles-bridge",
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

/// `fixture.c` as `<dir>/fixture/libsles-fixture.so` (NDK clang, -lOpenSLES); returns its dir.
fn fixture(root: &Path, dir: &Path) -> Result<PathBuf> {
    let ndk = crate::ndk::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, ARCH, API)?;
    let fdir = dir.join("fixture");
    std::fs::create_dir_all(&fdir).with_context(|| format!("creating {}", fdir.display()))?;
    let status = target
        .cmd("clang")
        .args(["-shared", "-fPIC", "-O1", "-Wall", "-Werror", "-o"])
        .arg(fdir.join("libsles-fixture.so"))
        .arg(root.join(TESTDATA).join("fixture.c"))
        .arg("-lOpenSLES")
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {TESTDATA}/fixture.c");
    Ok(fdir)
}

/// Builds the recorder and stages it as `<dir>/root/system/lib64/libOpenSLES.so`; returns the stage root.
fn recorder(root: &Path, dir: &Path) -> Result<PathBuf> {
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
    std::fs::copy(&so, lib.join("libOpenSLES.so"))
        .with_context(|| format!("copying {}", so.display()))?;
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
