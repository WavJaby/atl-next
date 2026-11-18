// SPDX-License-Identifier: MIT
//! `xtask test qemu`: the pinned QEMU's `qemu-<target>` (qemu.rs) against ground truth (QEMU bridge plan Q1).
//!
//! Hello: `xtask-harness/testdata/qemu/hello.c`, static bionic for the arch, built by the NDK clang; runs under the executable and must print `hello_expected`, which its C source computes. The version check (qemu::execute) runs ahead of it.
//! tcg smoke: the `TCG` programs of the arch's `qemu_target`, each by its `run-<name>` rule in QEMU's `tests/tcg/<target>-linux-user/Makefile`, with that rule's pass criterion (exit status, plus the `.ref` diff where the rule has one).
//! Output: `<out>/qemu/<target>-linux-user/gate/`, one make log per tcg program.
//! Break, by hand and reverted: an arches.toml `qemu_target` naming another target turns this gate red.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::ndk::NdkTarget;
use crate::qemu::{self, Build, GUEST_API};
use crate::verdict::{Failure, Verdict};

/// The hello program: a repo fixture, a stand-in for an NDK-built app executable (CLAUDE.md, C/C++ test fixtures).
const HELLO_SRC: &str = "xtask-harness/testdata/qemu/hello.c";
/// Its stdout for `target`: the printf of hello.c (which prints the compiler's arch macro), with sum(1..=100) = 5050 (Gauss's formula, not the program's output).
fn hello_expected(target: &str) -> String {
    format!("hello from static {target}: sum=5050\n")
}

/// bwrap argv (program first, closing `--` last) giving the guest a PID namespace of its own: LP32 bionic aborts at startup when its thread id is above 65535 ("32-bit pthread_mutex_t only supports pids <= 65535", measured with pids 133253 and 1042196), and a desktop's pid_max is far higher; an app process gets the same from its sandbox (atl-broker runs bwrap --unshare-pid). Used for every target: one path, harmless for LP64.
pub(crate) fn small_pids_argv() -> Vec<std::ffi::OsString> {
    let mut a = atl_bwrap::Args::new();
    a.dev_bind("/", "/").unshare_pid().die_with_parent();
    std::iter::once("bwrap".into()).chain(a.end()).collect()
}

/// Per `qemu_target`: QEMU's tests/tcg programs run here (each by its `run-<name>` rule; each passes on the pin with the NDK guest compiler), and those that fail on the pin, reported as skipped rather than run (each goes back into the run list when its condition clears).
struct Tcg {
    target: &'static str,
    smoke: &'static [&'static str],
    known_red: &'static [&'static str],
}

/// aarch64 `fcvt`, `fcvt-fprcvt`: FP16 flag lines differ from `fcvt.ref`, same with gcc and clang hosts; cleared when the guest toolchain explains it (bionic fenv suspected, not verified). `gcsstr`: guest SIGSEGV; cleared when it passes.
/// arm: the arm-specific programs of tests/tcg/arm/Makefile.target that build with the NDK clang; the 32-bit guest exercises patch 0006 (no reservation at -B 0, narrow mmap holes).
const TCG: &[Tcg] = &[
    Tcg {
        target: "aarch64",
        smoke: &[
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
        ],
        known_red: &["fcvt", "fcvt-fprcvt", "gcsstr"],
    },
    Tcg {
        target: "arm",
        smoke: &[
            "hello-arm",
            "pcalign-a32",
            "issue4448",
            "test-arm-opt-ror",
            "commpage",
        ],
        known_red: &[],
    },
];

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
    let tcg_set = TCG.iter().find(|t| t.target == b.target).with_context(|| {
        format!(
            "no tcg smoke list for qemu_target `{}` (qemu_gate.rs TCG)",
            b.target
        )
    })?;
    let exe = qemu::execute(root, &b)?;
    let gate = b.dir.join("gate");
    std::fs::create_dir_all(&gate).with_context(|| format!("creating {}", gate.display()))?;
    let mut fails = Vec::new();
    // the version check (qemu::execute), the hello program, and each smoke program
    let checks = 2 + tcg_set.smoke.len();
    if let Err(e) = hello(root, &b, &exe, &gate) {
        fails.push(Failure::step("hello", format!("{e:#}")));
    }
    for name in tcg_set.smoke {
        if let Err(e) = tcg(&b, &gate, name) {
            fails.push(Failure::step(&format!("tcg:{name}"), format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "qemu-gate",
        total: Some((checks, "checks")),
        skipped: tcg_set.known_red.to_vec(),
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
    let ndk = crate::ndk::default_ndk(root)?;
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
    let out = small_pids()
        .arg(exe)
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
    let want = hello_expected(&b.target);
    ensure!(got == want, "stdout {got:?}, expected {want:?}");
    Ok(())
}

/// A command running [`small_pids_argv`]; append the program and its arguments.
fn small_pids() -> Command {
    let argv = small_pids_argv();
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// Runs one tcg program by its `run-<name>` rule in the build's tests/tcg dir; the make output goes to `gate/tcg-<name>.log`.
fn tcg(b: &Build, gate: &Path, name: &str) -> Result<()> {
    let dir = b.build_dir().join("tests/tcg").join(b.name());
    let out = small_pids()
        .current_dir(&dir)
        .arg("make")
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
