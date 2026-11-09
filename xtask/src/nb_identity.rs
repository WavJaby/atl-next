// SPDX-License-Identifier: MIT
//! `xtask test nb-identity`: the identity native-bridge gates g1-g5 (the x86_64 engine, no QEMU), with the env they read taken from this checkout's out dir instead of set by hand.
//!
//! Needs `<out>/nb/x86_64` (the x86_64 NB build) and what `xtask test app` stages: `<out>/app/root/system/lib64/x86_64` (the guest's libc++.so) and `<out>/app/jni-fixture/android-21` (the test libraries). A missing dir fails the run by name.
//! Env per gate: `ATL_NB`, `ATL_NB_LIBCXX`, `ATL_NB_TESTLIB` (the fixture dir, then the NDK sysroot's lib dir with libc++_shared.so, as an APK's `lib/<abi>/` holds them). `ATL_NB_BREAK` in the caller's environment passes through to the gates (names: atl-native-bridge build.rs `BREAKS`).
//! Pass: every gate's cargo test exits 0. Logs: `<out>/nb-identity/<gate>.log`.
//! Default: mutating (cargo builds the gate targets, and each run writes its log).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::arches;
use crate::jni_fixture;
use crate::slot;
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{Failure, Verdict};

const PACKAGE: &str = "atl-native-bridge";
/// The identity gates: harness-less test targets of [`PACKAGE`], no feature.
const GATES: [&str; 5] = ["g1", "g2", "g3", "g4", "g5"];
/// The arches.toml row whose NB build and guest libraries this runs (the identity guest is the host arch).
const ARCH: &str = "x86_64";

#[derive(clap::Args)]
pub struct Args {
    /// Native out dir holding `nb/x86_64` and `app/` (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/x86_64).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = slot::out_dir(root, args.out.as_deref(), "--out", |ws| {
        ws.join("out/native").join(ARCH)
    })?;
    let env = identity_env(root, &out)?;
    let dir = out.join("nb-identity");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut fails = Vec::new();
    for gate in GATES {
        if let Err(e) = gate_run(root, &dir, &env, gate) {
            fails.push(Failure::step(gate, format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "nb-identity",
        total: Some((GATES.len(), "gates")),
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

/// The env the gates read, from `out`. Each named dir must exist: a missing one names the step that stages it.
fn identity_env(root: &Path, out: &Path) -> Result<Vec<(&'static str, OsString)>> {
    let nb = out.join(format!("nb/{ARCH}"));
    let libcxx = out.join(format!("app/root/system/lib64/{ARCH}"));
    let fixture = out.join("app/jni-fixture/android-21");
    for dir in [&nb, &libcxx, &fixture] {
        ensure!(
            dir.is_dir(),
            "missing {}: build the {ARCH} NB and run `xtask test app` (it stages app/)",
            dir.display()
        );
    }
    let ndk = jni_fixture::default_ndk(root)?;
    let clang = arches::load(root)?
        .get(ARCH)
        .and_then(|a| a.guest.as_ref())
        .with_context(|| format!("arches.toml has no [{ARCH}.guest] row"))?
        .clang
        .to_string();
    let sysroot_lib = jni_fixture::toolchain(&ndk)
        .join("sysroot/usr/lib")
        .join(clang);
    let testlib = std::env::join_paths([fixture, sysroot_lib])
        .context("ATL_NB_TESTLIB: a path holds the separator")?;
    Ok(vec![
        ("ATL_NB", nb.into_os_string()),
        ("ATL_NB_LIBCXX", libcxx.into_os_string()),
        ("ATL_NB_TESTLIB", testlib),
    ])
}

/// One gate: its cargo test under `env`, log to `dir/<gate>.log`; a nonzero exit fails it.
fn gate_run(root: &Path, dir: &Path, env: &[(&'static str, OsString)], gate: &str) -> Result<()> {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args(["test", "--locked", "-p", PACKAGE, "--test", gate])
        .envs(env.iter().map(|(k, v)| (*k, v)))
        .current_dir(root);
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("running cargo test")?;
    let log = dir.join(format!("{gate}.log"));
    std::fs::write(
        &log,
        [run.stdout.as_bytes(), run.stderr.as_bytes()].concat(),
    )?;
    ensure!(
        run.status.success(),
        "{gate} exited {}, log {}",
        run.status,
        log.display()
    );
    Ok(())
}
