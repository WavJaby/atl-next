// SPDX-License-Identifier: MIT
//! `xtask test crossing`: the crossing counter under QEMU (QEMU bridge plan Q5-8; counters.rs, feature `crossing-count`). The harness `tests/qemu_crossing.rs` checks on the real arm64 NB images: the fixture's N calls per binding count N, its sizes fall in their literal bins, and 2^16 crossings dump exactly once. Then the `qe_id` break (every binding calls its neighbour) must turn it red. Last, the library built without the feature has no counter symbol (`nm -C`), and the same probe finds one with the feature on (positive control).
//!
//! Library: the engine `.so` (`qemu_spike::engine_so_file`), as `qemu-thread` uses it. Fixture: `xtask/testdata/crossing/fixture.c` (PIE, NDK clang, `-fno-builtin`). Boot: `xtask/testdata/qemu-linker/boot.c` (static).
//! Images: `--nb <dir>` is the arm64 NB out dir (`<out>/nb/arm64`), built as for `test qemu-thread`.
//! Cargo runs go through `unit_use` with the feature selected here. Output: `<out>/qemu/<target>-linux-user-pic/crossing/`, one log per run; `cargo-probe` holds the feature-off and feature-on cdylib builds of the probe.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, GUEST_API};
use crate::qemu_linker;
use crate::qemu_spike;
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{Failure, Verdict};

const TESTDATA: &str = "xtask/testdata/crossing";
const PACKAGE: &str = "atl-native-bridge";
const TEST: &str = "qemu_crossing";
const FEATURE: &str = "crossing-count";
/// The harness's checks, `crossing: (x) ok` each: s started, r fixture results, c calls per binding, h size bins, w 2^16 window.
const CHECKS: [char; 5] = ['s', 'r', 'c', 'h', 'w'];
/// build.rs `BREAKS`: each binding trap calls its neighbour's host function.
const BREAK: &str = "qe_id";
/// Bound on one cargo run, as qemu_linker's harness runs (its `RUN_TIMEOUT_SECS`).
const RUN_TIMEOUT_SECS: u32 = 240;
/// The symbol prefix of the counter module in `nm -C` output (a demangled Rust path).
const COUNTER_SYMBOL: &str = "atl_native_bridge::counters";

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The arm64 NB out dir (`<out>/nb/arm64`), as for `test qemu-thread`.
    #[arg(long)]
    pub nb: PathBuf,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("crossing");
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
        ("ATL_NB_EXE", &fixture),
        ("ATL_NB_LINKER", &linker),
        ("ATL_NB_VDSO", &vdso),
    ];

    let mut fails = Vec::new();
    for brk in [None, Some(BREAK)] {
        let name = format!("crossing-{}", brk.unwrap_or("plain"));
        if let Err(e) = harness(root, &dir, &env, brk, &name) {
            fails.push(Failure::step(&name, format!("{e:#}")));
        }
    }
    if let Err(e) = probe(root, &dir) {
        fails.push(Failure::step("probe", format!("{e:#}")));
    }
    let verdict = Verdict {
        cmd: "crossing",
        total: Some((3, "runs")),
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

/// One harness run: `cargo test --features crossing-count --test qemu_crossing` with `env`, log in `dir/<name>.log`. The plain run needs every check ok and exit 0; the break run must not pass.
fn harness(
    root: &Path,
    dir: &Path,
    env: &[(&str, &Path)],
    brk: Option<&str>,
    name: &str,
) -> Result<()> {
    // `timeout -s KILL` kills the run's whole process group: a hung run is a failed run
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
        FEATURE,
        "--test",
        TEST,
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
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("running cargo test")?;
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &log,
        [run.stdout.as_bytes(), run.stderr.as_bytes()].concat(),
    )?;
    let text = &run.stdout;
    let ok = |c: char| text.lines().any(|l| l == format!("crossing: ({c}) ok"));
    match brk {
        None => {
            let red: Vec<char> = CHECKS.into_iter().filter(|&c| !ok(c)).collect();
            ensure!(
                red.is_empty() && run.status.success(),
                "checks {red:?} not ok ({}), log {}",
                run.status,
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
                !run.status.success(),
                "break run exited 0, log {}",
                log.display()
            );
        }
    }
    Ok(())
}

/// The counter's presence in the library: a cdylib built without the feature must have no counter symbol, and the same build with it must have one (the probe responds). `nm -C` on `dir/cargo-probe`'s `libatl_native_bridge.so`.
fn probe(root: &Path, dir: &Path) -> Result<()> {
    let target = dir.join("cargo-probe");
    let lib = target.join("debug/libatl_native_bridge.so");
    build_lib(root, &target, false)?;
    let off = counter_symbols(&lib)?;
    ensure!(
        off == 0,
        "the library built without {FEATURE} has {off} counter symbols"
    );
    build_lib(root, &target, true)?;
    let on = counter_symbols(&lib)?;
    ensure!(
        on > 0,
        "the library built with {FEATURE} has no counter symbol: the probe does not respond"
    );
    Ok(())
}

fn build_lib(root: &Path, target: &Path, feature: bool) -> Result<()> {
    let mut cmd = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    cmd.args(["build", "--locked", "-p", PACKAGE, "--lib", "--target-dir"])
        .arg(target)
        .current_dir(root);
    if feature {
        cmd.args(["--features", FEATURE]);
    }
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("running cargo build")?;
    ensure!(
        run.status.success(),
        "cargo build --lib (feature {feature}) failed: {}",
        run.status
    );
    Ok(())
}

/// Lines of `nm -C lib` that name a symbol of the counter module.
fn counter_symbols(lib: &Path) -> Result<usize> {
    let out = Command::new("nm")
        .arg("-C")
        .arg(lib)
        .output()
        .with_context(|| format!("running nm on {}", lib.display()))?;
    ensure!(out.status.success(), "nm failed on {}", lib.display());
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().filter(|l| l.contains(COUNTER_SYMBOL)).count())
}

/// `fixture.c` for the guest: a PIE executable on the NDK's libc.so, `-fno-builtin` so each call reaches the binding.
fn fixture_build(root: &Path, target: &NdkTarget, bin: &Path) -> Result<PathBuf> {
    let src = root.join(TESTDATA).join("fixture.c");
    let status = target
        .cmd("clang")
        .args([
            "-fPIE",
            "-pie",
            "-O1",
            "-fno-builtin",
            "-Wall",
            "-Werror",
            "-o",
        ])
        .arg(bin)
        .arg(&src)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    Ok(bin.to_path_buf())
}
