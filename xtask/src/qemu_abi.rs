// SPDX-License-Identifier: MIT
//! `xtask test qemu-abi`: the engine ABI minor 1, trap side (QEMU bridge plan Q6-1). A guest host call's integer lanes, FP lanes and stack words reach the host, and the host's FP answer returns in the guest's v0. Then each break must turn its run red.
//!
//! Library: the engine `.so` (`qemu_spike::engine_so_file`), as `qemu-jni` uses it. Fixture: `xtask/testdata/qemu-abi/fixture.c`, static, built as `qemu-linker`'s boot executable. No guest linker or NB images: the gate calls the fixture's entry directly.
//! Harness: `tests/qemu_abi.rs` (harness-less; checks s, x, v, k, r) run through `qemu_linker::harness`. Breaks (`ATL_NB_BREAK`): `qa_stack_off` (stack words arrive one word late, check k), `qa_v_lane` (FP lanes arrive one register late, check v).
//! Output: `<out>/qemu/<target>-linux-user-pic/qemu-abi/`, one log per run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};

use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, GUEST_API};
use crate::qemu_linker::{self, Harness};
use crate::qemu_spike;
use crate::verdict::{Failure, Verdict};

const TESTDATA: &str = "xtask/testdata/qemu-abi";
const HARNESS: Harness = Harness {
    test: "qemu_abi",
    prefix: "qemu-abi",
    checks: &['s', 'x', 'v', 'k', 'r'],
    breaks: &["qa_stack_off", "qa_v_lane"],
};

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Only the plain run, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("qemu-abi");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = qemu_spike::engine_so_file(root, &b, &dir.join("engine"))?;
    let ndk = jni_fixture::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    let fixture = fixture_build(root, &target, &dir.join("fixture"))?;
    let env: [(&str, &Path); 2] = [("ATL_ENGINE_SO", &so), ("ATL_QEMU_ABI_FIXTURE", &fixture)];
    let mut runs: Vec<Option<&str>> = vec![None];
    if !args.no_breaks {
        runs.extend(HARNESS.breaks.iter().map(|b| Some(*b)));
    }
    let total = runs.len();
    let mut fails = Vec::new();
    for brk in runs {
        let name = format!("{}-{}", HARNESS.prefix, brk.unwrap_or("plain"));
        if let Err(e) = qemu_linker::harness(root, &dir, &HARNESS, &env, &dir, brk, &name) {
            fails.push(Failure::step(&name, format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "qemu-abi",
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

/// `fixture.c` for the guest: a static executable, the form of `qemu-linker`'s boot executable.
fn fixture_build(root: &Path, target: &NdkTarget, bin: &Path) -> Result<PathBuf> {
    let src = root.join(TESTDATA).join("fixture.c");
    let status = target
        .cmd("clang")
        .args(["-static", "-O1", "-Wall", "-Werror", "-o"])
        .arg(bin)
        .arg(&src)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    Ok(bin.to_path_buf())
}
