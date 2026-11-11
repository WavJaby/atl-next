// SPDX-License-Identifier: MIT
//! `xtask test qemu-gl`: GLES2 proxy calls from an arm64 guest under QEMU (QEMU bridge plan Q7-4). fp arguments (glClearColor, glUniform4f, glDepthRangef, glTexParameterf, glVertexAttrib4f), 9-argument calls with the ninth on the guest stack (glTexImage2D, glTexSubImage2D), an 8-argument exact fit (glCompressedTexImage2D) and the fp result (glGetPathLengthNV) reach a host recorder bit for bit, each bound by its row in the QEMU binding table. Then each break must turn its run red.
//!
//! Library: the engine `.so` (`qemu_spike::engine_so_file`). Fixture: `xtask-harness/testdata/qemu-gl/fixture.c`, static (NDK clang, the NDK's GLES2 headers for the PFN types). No guest linker or NB images: the gate calls the fixture's entries directly, as `qemu-abi` does.
//! Harness: `tests/qemu_gl.rs` (harness-less; checks s, b, c, u, d, t, a, i, j, k, n) run through `qemu_linker::harness`. Breaks (`ATL_NB_BREAK`, dispatch.rs `call_host`): `v_drop` (fp lanes zeroed: checks c, u, d, t, a), `drop_stack` (stack words one early: checks i, j), `ret_int` (the fp result read from x0: check n).
//! Output: `<out>/qemu/<target>-linux-user-pic/qemu-gl/`, one log per run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};

use crate::ndk::NdkTarget;
use crate::qemu::{self, GUEST_API};
use crate::qemu_linker::{self, Harness};
use crate::qemu_spike;
use crate::verdict::{Failure, Verdict};

const TESTDATA: &str = "xtask-harness/testdata/qemu-gl";
const HARNESS: Harness = Harness {
    test: "qemu_gl",
    prefix: "qemu-gl",
    checks: &['s', 'b', 'c', 'u', 'd', 't', 'a', 'i', 'j', 'k', 'n'],
    breaks: &["v_drop", "drop_stack", "ret_int"],
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
    let dir = b.dir.join("qemu-gl");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = qemu_spike::engine_so_file(root, &b, &dir.join("engine"))?;
    let ndk = crate::ndk::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    let fixture = fixture_build(root, &target, &dir.join("fixture"))?;
    let env: [(&str, &Path); 2] = [("ATL_ENGINE_SO", &so), ("ATL_QEMU_GL_FIXTURE", &fixture)];
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
        cmd: "qemu-gl",
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

/// `fixture.c` for the guest: a static executable, as `qemu-abi`'s.
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
