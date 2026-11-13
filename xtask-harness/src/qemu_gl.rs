// SPDX-License-Identifier: MIT
//! `xtask test qemu-gl`: GLES2 proxy calls from an arm64 guest under QEMU (QEMU bridge plan Q7-4). fp arguments (glClearColor, glUniform4f, glDepthRangef, glTexParameterf, glVertexAttrib4f), 9-argument calls with the ninth on the guest stack (glTexImage2D, glTexSubImage2D), an 8-argument exact fit (glCompressedTexImage2D) and the fp result (glGetPathLengthNV) reach a host recorder bit for bit, each bound by its row in the QEMU binding table. Then each break must turn its run red.
//!
//! Library: the engine `.so` (`qemu_spike::engine_so_file`). Fixture: `xtask-harness/testdata/qemu-gl/fixture.c`, static (NDK clang, the NDK's GLES2 headers for the PFN types). No guest linker or NB images: the gate calls the fixture's entries directly, as `qemu-abi` does.
//! GS2.2 G1 (eglGetProcAddress, checks e, f, g, h, p and the trap run o): the fixture asks the binding of eglGetProcAddress for a name and calls the answer. Rows the gate registers answer the row's guest address; a name with no row goes to the host libEGL at SYSTEM_LIB, which is the recorder of `egl_bridge.rs` (`stage_recorder`) under the same bwrap root (`system_wrap`). The fallback names (p) and the named trap (o, its own run) need that host.
//! Harness: `tests/qemu_gl.rs` (harness-less; checks s, b, c, u, d, t, a, i, j, k, n, e, f, g, h, p; the trap run o) run through `qemu_linker::harness_in`. Breaks (`ATL_NB_BREAK`, dispatch.rs `call_host`, engine/proc.rs): `v_drop` (fp lanes zeroed: checks c, u, d, t, a), `drop_stack` (stack words one early: checks i, j), `ret_int` (the fp result read from x0: check n), `egl_raw` (eglGetProcAddress answers the host address of a row, not its guest address: checks e, f, g), `egl_nodedup` (a host answer is bound once per call: check p).
//! Output: `<out>/qemu/<target>-linux-user-pic/qemu-gl/`, one log per run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::egl_bridge;
use crate::ndk::NdkTarget;
use crate::qemu::{self, GUEST_API};
use crate::qemu_linker::{self, Harness};
use crate::qemu_spike;
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{Failure, Verdict};

const TESTDATA: &str = "xtask-harness/testdata/qemu-gl";
const PACKAGE: &str = "atl-native-bridge";
/// Per run; a run that outlives it (a hung run) counts as failed.
const RUN_TIMEOUT_SECS: u32 = 600;
const HARNESS: Harness = Harness {
    test: "qemu_gl",
    prefix: "qemu-gl",
    checks: &[
        's', 'b', 'c', 'u', 'd', 't', 'a', 'i', 'j', 'k', 'n', 'e', 'f', 'g', 'h', 'p',
    ],
    breaks: &["v_drop", "drop_stack", "ret_int", "egl_raw", "egl_nodedup"],
};

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Only the plain run and the trap run, no breaks.
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
    let fixture = target.static_exe(
        &[&root.join(TESTDATA).join("fixture.c")],
        &dir.join("fixture"),
    )?;
    let env: [(&str, &Path); 2] = [("ATL_ENGINE_SO", &so), ("ATL_QEMU_GL_FIXTURE", &fixture)];
    // the host libEGL / libGLESv2 of the fallback checks (p, o): the recorder at SYSTEM_LIB, in the bwrap root of every run
    let stage = egl_bridge::stage_recorder(root, &dir)?;
    let wrap = qemu_linker::system_wrap(&stage)?;
    let mut runs: Vec<Option<&str>> = vec![None];
    if !args.no_breaks {
        runs.extend(HARNESS.breaks.iter().map(|b| Some(*b)));
    }
    let total = runs.len() + 1;
    let mut fails = Vec::new();
    for brk in runs {
        let name = format!("{}-{}", HARNESS.prefix, brk.unwrap_or("plain"));
        if let Err(e) = qemu_linker::harness_in(root, &dir, &HARNESS, &env, &dir, brk, &name, &wrap)
        {
            fails.push(Failure::step(&name, format!("{e:#}")));
        }
    }
    if let Err(e) = trap_run(root, &dir, &env, &wrap) {
        fails.push(Failure::step(
            &format!("{}-trap", HARNESS.prefix),
            format!("{e:#}"),
        ));
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

/// The named trap (check o, tests/qemu_gl.rs `egl_trap_run`): `ATL_QEMU_GL_CASE=trap` runs that check alone. The process must abort with its stderr naming glAtlNoDescriptor; a clean exit or another stderr fails.
fn trap_run(root: &Path, dir: &Path, env: &[(&str, &Path)], wrap: &[OsString]) -> Result<()> {
    let mut cmd = Command::new("timeout");
    cmd.args(["-s", "KILL"])
        .arg(RUN_TIMEOUT_SECS.to_string())
        .args(wrap)
        .arg(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args([
        "test",
        "--locked",
        "-p",
        PACKAGE,
        "--test",
        HARNESS.test,
        "--target-dir",
    ])
    .arg(dir.join("cargo"))
    .args(["--"])
    .envs(env.iter().copied())
    .env("ATL_NB_DLLIBS", dir)
    .env("ATL_QEMU_GL_CASE", "trap")
    .env_remove("ATL_NB_BREAK")
    .current_dir(root);
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("running cargo test")?;
    let log = dir.join(format!("{}-trap.log", HARNESS.prefix));
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
        run.stderr.contains(egl_bridge::TRAP_LINE),
        "the abort does not name glAtlNoDescriptor, log {}",
        log.display()
    );
    Ok(())
}
