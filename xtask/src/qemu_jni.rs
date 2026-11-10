// SPDX-License-Identifier: MIT
//! `xtask test qemu-jni`: guest JNI calls under the QEMU crossing, integer lanes only, on the real arm64 NB images (QEMU bridge plan Q6-2). The fixture's JNI calls reach a fake host JNIEnv; GetJavaVM, a special, routes to its own thunk. Then each break must turn its run red.
//!
//! Library: the engine `.so` (`qemu_spike::engine_so_file`), as `qemu-thread` uses it. Fixture: `xtask/testdata/qemu-jni/fixture.c` (PIE, NDK clang, DT_NEEDED libc.so). Boot: `xtask/testdata/qemu-linker/boot.c` (static).
//! Images: `--nb <dir>` is the arm64 NB out dir (`<out>/nb/arm64`), built as for `test qemu-linker`.
//! Harness: `tests/qemu_jni.rs` (harness-less; checks s, v, e, c, j, t, d, f, a, p, the down calls o, m, x (Q6-4), and the refusals r, l, g and the down-call refusal R, each refusal run in a child process) run through `qemu_linker::harness`. Breaks (`ATL_NB_BREAK`): `jni_guest_env` (the host function gets the guest env, check e), `jni_swap` (x3 and x4 exchanged, check c), `v_drop` (fp lanes zeroed, checks d), `drop_stack` (stack words one early, check d), `ret_int` (fp results read from x0, checks d and f), `al_zero` (host calls with %al = 0, check p), `down_drop_stack` (a down call's stack words start one word late, check m), `down_swap_v` (a down call's %xmm0 and %xmm1 exchanged, checks m and x), `shorty_swap` (the first shorty parameter's class flipped, checks m, x and R), `down_noenv` (a down call's first argument is the host's env or vm, checks o, m and x).
//! Output: `<out>/qemu/<target>-linux-user-pic/qemu-jni/`, one log per run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};

use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, GUEST_API};
use crate::qemu_linker::{self, Harness};
use crate::qemu_spike;
use crate::verdict::{Failure, Verdict};

const TESTDATA: &str = "xtask/testdata/qemu-jni";
const HARNESS: Harness = Harness {
    test: "qemu_jni",
    prefix: "qemu-jni",
    checks: &[
        's', 'v', 'e', 'c', 'j', 't', 'd', 'f', 'a', 'p', 'o', 'm', 'x', 'r', 'l', 'g', 'R',
    ],
    breaks: &[
        "jni_guest_env",
        "jni_swap",
        "v_drop",
        "drop_stack",
        "ret_int",
        "al_zero",
        "down_drop_stack",
        "down_swap_v",
        "shorty_swap",
        "down_noenv",
    ],
};

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The arm64 NB out dir (`<out>/nb/arm64`), as for `test qemu-linker`.
    #[arg(long)]
    pub nb: PathBuf,
    /// Only the plain run, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("qemu-jni");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = qemu_spike::engine_so_file(root, &b, &dir.join("engine"))?;
    let linker = args.nb.join("native_bridge_guest_linker/linker64");
    let vdso = args
        .nb
        .join("libnative_bridge_vdso/libnative_bridge_vdso.so");
    let stub_libc = args.nb.join("libnative_bridge_guest_libc/libc.so");
    ensure!(
        linker.is_file() && vdso.is_file() && stub_libc.is_file(),
        "missing {}, {} or {}: build the arm64 NB first (nb:libnative_bridge_guest_libc among its goals)",
        linker.display(),
        vdso.display(),
        stub_libc.display()
    );
    let ndk = jni_fixture::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    let fixture = fixture_build(root, &target, &dir.join("fixture"))?;
    let boot =
        qemu_linker::guest_build(root, &target, "boot", &dir.join("boot"), &["-static"], &[])?;
    let env: [(&str, &Path); 5] = [
        ("ATL_ENGINE_SO", &so),
        ("ATL_NB_BOOT", &boot),
        ("ATL_NB_JNIEXE", &fixture),
        ("ATL_NB_LINKER", &linker),
        ("ATL_NB_VDSO", &vdso),
    ];
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
        cmd: "qemu-jni",
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

/// `fixture.c` for the guest: a PIE executable on the NDK's libc.so (no `-nostdlib`, so DT_NEEDED names libc.so, which the guest linker resolves to the stub libc).
fn fixture_build(root: &Path, target: &NdkTarget, bin: &Path) -> Result<PathBuf> {
    let src = root.join(TESTDATA).join("fixture.c");
    let status = target
        .cmd("clang")
        .args(["-fPIE", "-pie", "-O1", "-Wall", "-Werror", "-o"])
        .arg(bin)
        .arg(&src)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    Ok(bin.to_path_buf())
}
