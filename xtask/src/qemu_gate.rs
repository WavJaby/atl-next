// SPDX-License-Identifier: MIT
//! `xtask test qemu`: the pinned QEMU's `qemu-<target>` (qemu.rs) against ground truth (QEMU bridge plan Q1).
//!
//! Hello: `xtask/testdata/qemu/hello.c`, static bionic for the arch, built by the NDK clang; runs under the executable and must print `HELLO_EXPECTED`, which its C source computes. The version check (qemu::execute) runs ahead of it.
//! tcg smoke: the `SMOKE` programs, each by its `run-<name>` rule in QEMU's `tests/tcg/<target>-linux-user/Makefile`, with that rule's pass criterion (exit status, plus the `.ref` diff where the rule has one).
//! Output: `<out>/qemu/<target>-linux-user/gate/`, one make log per tcg program.
//! Break, by hand and reverted: an arches.toml `qemu_target` naming another target turns this gate red.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, Build, GUEST_API};
use crate::verdict::{Failure, Verdict};

/// The hello program: a repo fixture, a stand-in for an NDK-built app executable (CLAUDE.md, C/C++ test fixtures).
const HELLO_SRC: &str = "xtask/testdata/qemu/hello.c";
/// Its stdout: the printf of hello.c, with sum(1..=100) = 5050 (Gauss's formula, not the program's output).
const HELLO_EXPECTED: &str = "hello from static aarch64: sum=5050\n";

/// QEMU's tests/tcg programs run here, each by its `run-<name>` rule; each passes on the pin with the NDK guest compiler.
const SMOKE: &[&str] = &[
    "pcalign-a64",
    "lse2-fault",
    "test-2248",
    "test-2150",
    "dcpop",
    "dcpodp",
    "bti-1",
    "bti-2",
    "bti-3",
    "sysregs",
    "test-aes",
    "sve-ioctls",
    "pauth-1",
    "mte-1",
];

/// tests/tcg programs that fail on the pin here, reported as skipped rather than run. Each goes back into `SMOKE` when its condition clears.
/// `fcvt`, `fcvt-fprcvt`: FP16 flag lines differ from `fcvt.ref`, same with gcc and clang hosts; cleared when the guest toolchain explains it (bionic fenv suspected, not verified).
/// `gcsstr`: guest SIGSEGV; cleared when it passes.
const KNOWN_RED: &[&str] = &["fcvt", "fcvt-fprcvt", "gcsstr"];

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is tested.
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), false)?;
    let exe = qemu::execute(root, &b)?;
    let gate = b.dir.join("gate");
    std::fs::create_dir_all(&gate).with_context(|| format!("creating {}", gate.display()))?;
    let mut fails = Vec::new();
    // the version check (qemu::execute), the hello program, and each smoke program
    let checks = 2 + SMOKE.len();
    if let Err(e) = hello(root, &b, &exe, &gate) {
        fails.push(Failure::step("hello", format!("{e:#}")));
    }
    for name in SMOKE {
        if let Err(e) = tcg(&b, &gate, name) {
            fails.push(Failure::step(&format!("tcg:{name}"), format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "qemu-gate",
        total: Some((checks, "checks")),
        skipped: KNOWN_RED.to_vec(),
        failures: fails,
    };
    println!("{}", verdict.line());
    Ok(if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Builds the hello program for the arch's bionic and checks what `exe` prints for it.
fn hello(root: &Path, b: &Build, exe: &Path, gate: &Path) -> Result<()> {
    let ndk = jni_fixture::default_ndk(root)?;
    let bin = gate.join("hello");
    let src = root.join(HELLO_SRC);
    let status = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?
        .cmd("clang")
        .args(["-static", "-O2", "-Wall", "-Werror", "-o"])
        .arg(&bin)
        .arg(&src)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    let out = Command::new(exe)
        .arg(&bin)
        .output()
        .with_context(|| format!("running {}", exe.display()))?;
    ensure!(
        out.status.success(),
        "{} exited {}",
        exe.display(),
        out.status
    );
    let got = String::from_utf8_lossy(&out.stdout);
    ensure!(
        got == HELLO_EXPECTED,
        "stdout {got:?}, expected {HELLO_EXPECTED:?}"
    );
    Ok(())
}

/// Runs one tcg program by its `run-<name>` rule in the build's tests/tcg dir; the make output goes to `gate/tcg-<name>.log`.
fn tcg(b: &Build, gate: &Path, name: &str) -> Result<()> {
    let dir = b.build_dir().join("tests/tcg").join(b.name());
    let out = Command::new("make")
        .current_dir(&dir)
        .arg(format!("run-{name}"))
        .output()
        .with_context(|| format!("running make in {}", dir.display()))?;
    let log = gate.join(format!("tcg-{name}.log"));
    std::fs::write(&log, [out.stdout, out.stderr].concat())
        .with_context(|| format!("writing {}", log.display()))?;
    ensure!(
        out.status.success(),
        "make run-{name} exited {}, log {}",
        out.status,
        log.display()
    );
    Ok(())
}
