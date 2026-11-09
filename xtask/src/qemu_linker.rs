// SPDX-License-Identifier: MIT
//! `xtask test qemu-linker`: the QEMU crossing on the real arm64 NB images, two harnesses, each plain then with each of its breaks (QEMU bridge plan Q4-3, Q4-4).
//!
//! Library: the spike's `-shared` QEMU library (`qemu_spike::link`). Fixtures in `xtask/testdata/qemu-linker/`, built by the NDK clang: `fixture.c` (static, Q4-3), `boot.c` (static, the Q4-4 boot executable), `exe.c` (PIE without libc, the main executable the guest linker loads).
//! Images: `--nb <dir>` is the arm64 NB out dir (`<out>/nb/arm64`, from `xtask native --native-bridge arm64 nb:libc nb:native_bridge_guest_linker nb:libnative_bridge_vdso`): `native_bridge_guest_linker/linker64` and `libnative_bridge_vdso/libnative_bridge_vdso.so`.
//! Harnesses: `cargo test -p atl-native-bridge --features qemu-guest --test <name>`, harness-less; each header lists its env and checks.
//! - `qemu_linker` (Q4-3 placement): checks p, r, t, a, s. Breaks (`ATL_NB_BREAK`): `qe_site` (the vdso's placeholder search fails closed on call_guest), `qe_nomap` (no page registered: a fetch from a placed site faults).
//! - `qemu_start` (Q4-4 linker init): checks s (started), e (entry), t (static TLS), h (get_host_pthread), g (GS base). Breaks: `qe_guest_base` (QEMU at a nonzero guest base: BOOT refuses), `qe_nodata` (the argument block unregistered with QEMU: the linker cannot open argv[1]).
//!
//! Pass: every check prints ok and the run exits 0; each break must turn its run red. Output: `<out>/qemu/<target>-linux-user-pic/qemu-linker/`, one log per run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, GUEST_API};
use crate::qemu_spike;
use crate::verdict::{Failure, Verdict};

const TESTDATA: &str = "xtask/testdata/qemu-linker";
const PACKAGE: &str = "atl-native-bridge";
/// Per harness run. A run that outlives it hangs (a guest fault QEMU does not end) and counts as failed.
const RUN_TIMEOUT_SECS: u32 = 240;

/// One harness: its cargo test, the prefix and letters of its check lines, its breaks.
struct Harness {
    test: &'static str,
    prefix: &'static str,
    checks: &'static [char],
    breaks: &'static [&'static str],
}

const HARNESSES: [Harness; 2] = [
    Harness {
        test: "qemu_linker",
        prefix: "qemu-linker",
        checks: &['p', 'r', 't', 'a', 's'],
        breaks: &["qe_site", "qe_nomap"],
    },
    Harness {
        test: "qemu_start",
        prefix: "qemu-start",
        checks: &['s', 'e', 't', 'h', 'g'],
        breaks: &["qe_guest_base", "qe_nodata"],
    },
];

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
    /// Only the plain runs, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("qemu-linker");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = qemu_spike::link(&b, &dir.join("libqemu-spike.so"), None)?;
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
    let ndk = jni_fixture::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    let fixture = guest_build(root, &target, &dir, "fixture", &["-static"])?;
    let boot = guest_build(root, &target, &dir, "boot", &["-static"])?;
    let exe = guest_build(root, &target, &dir, "exe", &["-nostdlib", "-fPIE", "-pie"])?;
    let build_dir = b.build_dir();
    let env: [(&str, &Path); 7] = [
        ("ATL_QEMU_BUILD", &build_dir),
        ("ATL_QEMU_SPIKE_SO", &so),
        ("ATL_QEMU_SPIKE_FIXTURE", &fixture),
        ("ATL_NB_BOOT", &boot),
        ("ATL_NB_EXE", &exe),
        ("ATL_NB_LINKER", &linker),
        ("ATL_NB_VDSO", &vdso),
    ];
    let mut fails = Vec::new();
    let mut total = 0;
    for h in &HARNESSES {
        let mut runs: Vec<Option<&str>> = vec![None];
        if !args.no_breaks {
            runs.extend(h.breaks.iter().map(|b| Some(*b)));
        }
        for brk in runs {
            total += 1;
            let name = format!("{}-{}", h.test, brk.unwrap_or("plain"));
            if let Err(e) = harness(root, &dir, h, &env, brk, &name) {
                fails.push(Failure::step(&name, format!("{e:#}")));
            }
        }
    }
    let verdict = Verdict {
        cmd: "qemu-linker",
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

/// `<TESTDATA>/<stem>.c` built for the guest by the NDK clang with `flags`, as `<dir>/<stem>`.
fn guest_build(
    root: &Path,
    target: &NdkTarget,
    dir: &Path,
    stem: &str,
    flags: &[&str],
) -> Result<PathBuf> {
    let src = root.join(TESTDATA).join(format!("{stem}.c"));
    let bin = dir.join(stem);
    let status = target
        .cmd("clang")
        .args(flags)
        .args(["-O1", "-Wall", "-Werror", "-o"])
        .arg(&bin)
        .arg(&src)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    Ok(bin)
}

/// One harness run: plain (every check ok, exit 0) or a break (not every check ok, and a nonzero exit).
fn harness(
    root: &Path,
    dir: &Path,
    h: &Harness,
    env: &[(&str, &Path)],
    brk: Option<&str>,
    name: &str,
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
        h.test,
        "--target-dir",
    ])
    .arg(dir.join("cargo"))
    .args(["--"])
    .envs(env.iter().copied())
    .current_dir(root);
    match brk {
        Some(b) => cmd.env("ATL_NB_BREAK", b),
        None => cmd.env_remove("ATL_NB_BREAK"),
    };
    let out = cmd.output().context("running cargo test")?;
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &log,
        [out.stdout.as_slice(), out.stderr.as_slice()].concat(),
    )?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ok = |c: char| text.lines().any(|l| l == format!("{}: ({c}) ok", h.prefix));
    match brk {
        None => {
            let red: Vec<char> = h.checks.iter().copied().filter(|&c| !ok(c)).collect();
            ensure!(
                red.is_empty() && out.status.success(),
                "checks {red:?} not ok ({}), log {}",
                out.status,
                log.display()
            );
        }
        Some(_) => {
            ensure!(
                !h.checks.iter().all(|&c| ok(c)),
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
