// SPDX-License-Identifier: MIT
//! `xtask test qemu-linker`: the QEMU crossing's placement on the real arm64 NB images (QEMU bridge plan Q4-3) against its fixture, then each break.
//!
//! Library: the spike's `-shared` QEMU library (`qemu_spike::link`). Fixture: `xtask/testdata/qemu-linker/fixture.c`, static bionic by the NDK clang.
//! Images: `--nb <dir>` is the arm64 NB out dir (`<out>/nb/arm64`, from `xtask native --native-bridge arm64 nb:libc nb:native_bridge_guest_linker nb:libnative_bridge_vdso`): `native_bridge_guest_linker/linker64` and `libnative_bridge_vdso/libnative_bridge_vdso.so`.
//! Harness: `cargo test -p atl-native-bridge --features qemu-guest --test qemu_linker`, harness-less; its header lists the env and the checks (p, r, t, a, s).
//! Pass: every check prints ok and the run exits 0. Breaks (`ATL_NB_BREAK`): `qe_site` (the vdso's placeholder search fails closed on call_guest), `qe_nomap` (no page registered: a fetch from a placed site faults). Each must turn its run red.
//! Output: `<out>/qemu/<target>-linux-user-pic/qemu-linker/`, one log per run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, Build, GUEST_API};
use crate::qemu_spike;
use crate::verdict::{Failure, Verdict};

const FIXTURE_SRC: &str = "xtask/testdata/qemu-linker/fixture.c";
const PACKAGE: &str = "atl-native-bridge";
/// The checks the harness prints (its header): placement, registration, trap ids, arguments, answers.
const CHECKS: [char; 5] = ['p', 'r', 't', 'a', 's'];
/// The breaks: `ATL_NB_BREAK` names, each its run must go red.
const BREAKS: [&str; 2] = ["qe_site", "qe_nomap"];
/// Per harness run. A run that outlives it hangs (a guest fault QEMU does not end) and counts as failed.
const RUN_TIMEOUT_SECS: u32 = 240;

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The arm64 NB out dir (`<out>/nb/arm64`): holds `native_bridge_guest_linker/linker64` and `libnative_bridge_vdso/libnative_bridge_vdso.so`.
    #[arg(long)]
    pub nb: PathBuf,
    /// Only the plain run, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("qemu-linker");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = qemu_spike::link(&b, &dir.join("libqemu-spike.so"), None)?;
    let fixture = fixture(root, &b, &dir)?;
    let linker = args.nb.join("native_bridge_guest_linker/linker64");
    let vdso = args
        .nb
        .join("libnative_bridge_vdso/libnative_bridge_vdso.so");
    ensure!(
        linker.is_file() && vdso.is_file(),
        "missing {} or {}: build the arm64 NB first",
        linker.display(),
        vdso.display()
    );
    let mut runs: Vec<Option<&str>> = vec![None];
    if !args.no_breaks {
        runs.extend(BREAKS.iter().map(|b| Some(*b)));
    }
    let mut fails = Vec::new();
    for brk in &runs {
        let name = brk.unwrap_or("plain");
        let images = Images {
            linker: &linker,
            vdso: &vdso,
        };
        if let Err(e) = harness(root, &b, &dir, &so, &fixture, &images, *brk) {
            fails.push(Failure::step(name, format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "qemu-linker",
        total: Some((runs.len(), "runs")),
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

/// The arm64 NB artifacts the harness maps and places.
struct Images<'a> {
    linker: &'a Path,
    vdso: &'a Path,
}

/// The guest static executable: fixture.c, built as the spike's fixture is.
fn fixture(root: &Path, b: &Build, dir: &Path) -> Result<PathBuf> {
    let ndk = jni_fixture::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    let bin = dir.join("fixture");
    let status = target
        .cmd("clang")
        .args(["-static", "-O1", "-Wall", "-Werror", "-o"])
        .arg(&bin)
        .arg(root.join(FIXTURE_SRC))
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {FIXTURE_SRC}");
    Ok(bin)
}

/// One harness run: plain (every check ok, exit 0) or a break (not every check ok, or a nonzero exit).
fn harness(
    root: &Path,
    b: &Build,
    dir: &Path,
    so: &Path,
    fixture: &Path,
    images: &Images,
    brk: Option<&str>,
) -> Result<()> {
    // `timeout` kills the run's whole process group: a hung run is a failed run (exit 124)
    let mut cmd = Command::new("timeout");
    cmd.args(["-s", "KILL"])
        .arg(RUN_TIMEOUT_SECS.to_string())
        .arg(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args([
        "test",
        "--locked",
        "-p",
        PACKAGE,
        "--features",
        "qemu-guest",
        "--test",
        "qemu_linker",
        "--target-dir",
    ])
    .arg(dir.join("cargo"))
    .args(["--"])
    .env("ATL_QEMU_BUILD", b.build_dir())
    .env("ATL_QEMU_SPIKE_SO", so)
    .env("ATL_QEMU_SPIKE_FIXTURE", fixture)
    .env("ATL_NB_LINKER", images.linker)
    .env("ATL_NB_VDSO", images.vdso)
    .current_dir(root);
    match brk {
        Some(name) => cmd.env("ATL_NB_BREAK", name),
        None => cmd.env_remove("ATL_NB_BREAK"),
    };
    let out = cmd.output().context("running cargo test")?;
    let name = brk.unwrap_or("plain");
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &log,
        [out.stdout.as_slice(), out.stderr.as_slice()].concat(),
    )?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ok = |c: char| text.lines().any(|l| l == format!("qemu-linker: ({c}) ok"));
    match brk {
        None => {
            let red: Vec<char> = CHECKS.into_iter().filter(|&c| !ok(c)).collect();
            ensure!(
                red.is_empty() && out.status.success(),
                "checks {red:?} not ok ({}), log {}",
                out.status,
                log.display()
            );
        }
        Some(_) => {
            ensure!(
                !CHECKS.into_iter().all(ok),
                "break left every check ok, log {}",
                log.display()
            );
            ensure!(
                !out.status.success(),
                "break run exited 0, log {}",
                log.display()
            );
        }
    }
    Ok(())
}
