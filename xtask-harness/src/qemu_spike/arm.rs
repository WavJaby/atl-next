// SPDX-License-Identifier: MIT
//! `xtask test qemu-spike --arch arm`: the arm32 engine spike (phase 4 P1) on the arm-linux-user PIC build: tests/spike_arm.rs against fixture_arm.c, then its breaks.
//! Library and harness as for aarch64 (the parent module): the same link, the same libsigchain object, its own cargo target dir (`cargo-arm`: the layouts differ by target).
//! Breaks, each a text edit of one patched file compiled by the build's own command (one variable each): `no-p6` restores the 4 GiB reservation under `-B 0` (patch 0006 removed): the boot ends "address space that is in use" against the ART stand-in, before gate (e); `arm-only-decode` reads every svc immediate as an Arm imm24 (patch 0005's Thumb branch removed): the Thumb caller of gate (a) misses the hook.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use super::{
    FIXTURE_DIR, PACKAGE, RUN_TIMEOUT_SECS, basename, compile_as, killed_at_limit, link, sigchain,
};
use crate::ndk::NdkTarget;
use crate::qemu::{Build, GUEST_API};
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{Failure, Verdict};

/// The arm fixture (fixture_arm.c), beside the aarch64 one.
const FIXTURE: &str = "fixture_arm.c";
/// Gates the plain run must print ok (spike_arm.rs).
const GATES: [char; 7] = ['B', 'a', 'b', 'c', 'd', 'e', 'f'];

/// A break: one file of the build, `from` replaced by `to` (exactly once) before it is compiled.
struct Edit {
    file: &'static str,
    from: &'static str,
    to: &'static str,
}

struct RunSpec {
    name: &'static str,
    edit: Option<Edit>,
    /// What makes the run red: `Gate(g)` its gate failed; `Boot(text)` the boot ended with `text` in the output before BOOT.
    red: Option<Red>,
}

enum Red {
    Gate(char),
    Boot(&'static str),
}

const RUNS: &[RunSpec] = &[
    RunSpec {
        name: "plain",
        edit: None,
        red: None,
    },
    // patch 0006 removed: `-B 0` reserves the low 4 GiB again, which the stand-in at 0x12c00000 occupies (common-user/probe-guest-base.c pgb_fixed's message)
    RunSpec {
        name: "no-p6",
        edit: Some(Edit {
            file: "linux-user/main.c",
            from: " &&\n               !(have_guest_base && guest_base == 0)) {",
            to: ") {",
        }),
        red: Some(Red::Boot("address space that is in use")),
    },
    // patch 0005's Thumb branch removed: a Thumb svc is decoded from the 4 bytes before pc as an Arm insn
    RunSpec {
        name: "arm-only-decode",
        edit: Some(Edit {
            file: "linux-user/arm/cpu_loop.c",
            from: "    if (env->thumb) {\n        if (get_user_u16(",
            to: "    if (0) {\n        if (get_user_u16(",
        }),
        red: Some(Red::Gate('a')),
    },
];

/// Runs the plain run and (unless `no_breaks`) every break; prints the verdict line. True when all ended as specified.
pub(super) fn run(root: &Path, b: &Build, no_breaks: bool) -> Result<bool> {
    let dir = b.dir.join("spike");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = link(b, &dir.join("libqemu-spike.so"), None)?;
    let fixture = fixture(root, b, &dir)?;
    let sigchain = sigchain(root, &dir)?;
    let mut fails = Vec::new();
    // the armv7 boot executables assemble and link (atl-qemu-boot's Arm-mode entries); the engine `.so` (C ABI 4.0) boots the boot executable
    if let Err(e) = boot_exes(root, b, &dir).and_then(|boot| engine_so(root, b, &dir, &boot)) {
        fails.push(Failure::step("engine-so", format!("{e:#}")));
    }
    for spec in RUNS.iter().filter(|r| !no_breaks || r.red.is_none()) {
        let r = match &spec.edit {
            Some(e) => edited(b, &dir, spec.name, e)
                .and_then(|lib| harness(root, b, &dir, spec, &lib, &fixture, &sigchain)),
            None => harness(root, b, &dir, spec, &so, &fixture, &sigchain),
        };
        if let Err(e) = r {
            fails.push(Failure::step(spec.name, format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "qemu-spike",
        total: Some((
            1 + RUNS
                .iter()
                .filter(|r| !no_breaks || r.red.is_none())
                .count(),
            "arm checks",
        )),
        skipped: Vec::new(),
        failures: fails,
    };
    println!("{}", verdict.line());
    Ok(verdict.failures.is_empty())
}

/// Builds the boot and cpuinfo executables for the arm guest and checks each has an ELF entry (llvm-readelf). Returns the boot executable.
fn boot_exes(root: &Path, b: &Build, dir: &Path) -> Result<PathBuf> {
    let out = dir.join("boot");
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let boot = crate::qemu::boot_exe(root, b, &out)?;
    for exe in [boot.clone(), crate::qemu::cpuinfo_exe(root, b, &out)?] {
        let entry = super::llvm_entry(&exe)?;
        ensure!(entry != 0, "{}: e_entry 0", exe.display());
    }
    Ok(boot)
}

/// The arm engine `.so` (`libatl_engine_arm.so`, linked as aarch64's) booted through its C ABI on `boot` (tests/spike.rs `--engine-so`): (q) create reaches BOOT, map and attach inside it, AT_ENTRY = e_entry; (r) attach + detach on a thread of its own.
fn engine_so(root: &Path, b: &Build, dir: &Path, boot: &Path) -> Result<()> {
    let out = dir.join("engine");
    let so = super::engine_so_file(root, b, &out)?;
    let entry = super::llvm_entry(boot)?;
    super::boot_gate_of(root, b, &out, "arm", &so, boot, entry, &['q', 'r'])
}

/// fixture_arm.c as a static bionic armv7 executable (NDK clang of the arm row).
fn fixture(root: &Path, b: &Build, dir: &Path) -> Result<PathBuf> {
    let ndk = crate::ndk::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    target.static_exe(
        &[&root.join(FIXTURE_DIR).join(FIXTURE)],
        &dir.join("fixture-arm"),
    )
}

/// The spike library with `e.file` compiled from the build's source after the edit.
fn edited(b: &Build, dir: &Path, name: &str, e: &Edit) -> Result<PathBuf> {
    let text = std::fs::read_to_string(b.src.join(e.file))
        .with_context(|| format!("reading {}", e.file))?;
    ensure!(
        text.matches(e.from).count() == 1,
        "{name}: {} holds the edited text {} times, expected once (patch changed?)",
        e.file,
        text.matches(e.from).count()
    );
    let flat = dir.join(name);
    std::fs::create_dir_all(&flat).with_context(|| format!("creating {}", flat.display()))?;
    let src = flat.join(basename(e.file)?);
    std::fs::write(&src, text.replacen(e.from, e.to, 1))?;
    let (output, obj) = compile_as(b, dir, name, e.file, &src)?;
    link(
        b,
        &dir.join(format!("libqemu-spike-{name}.so")),
        Some((&output, &obj)),
    )
}

fn harness(
    root: &Path,
    b: &Build,
    dir: &Path,
    spec: &RunSpec,
    so: &Path,
    fixture: &Path,
    sigchain: &Path,
) -> Result<()> {
    // a PID namespace of its own (qemu_gate::small_pids_argv), as cargo's runner for the test binary: unit_use appends cargo's own arguments, so cargo must stay the command
    let runner = format!(
        "target.'cfg(all())'.runner = [{}]",
        crate::qemu_gate::small_pids_argv()
            .iter()
            .map(|a| format!("{:?}", a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut cmd = Command::new("timeout");
    cmd.args(["-s", "KILL"])
        .arg(RUN_TIMEOUT_SECS.to_string())
        .arg(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "test",
            "--config",
            &runner,
            "--locked",
            "-p",
            PACKAGE,
            "--features",
            "qemu",
            "--test",
            "spike_arm",
            "--target-dir",
        ])
        .arg(dir.join("cargo-arm"))
        .env("ATL_QEMU_BUILD", b.build_dir())
        .env("ATL_QEMU_SPIKE_SO", so)
        .env("ATL_QEMU_SPIKE_FIXTURE", fixture)
        .env("ATL_QEMU_SIGCHAIN_OBJ", sigchain)
        .env_remove("ATL_QEMU_ENGINE_BREAK")
        .current_dir(root);
    let run =
        unit_use::run(&mut cmd, Units::Recorded, Echo::Nothing).context("running cargo test")?;
    let log = dir.join(format!("arm-{}.log", spec.name));
    std::fs::write(
        &log,
        [run.stdout.as_bytes(), run.stderr.as_bytes()].concat(),
    )?;
    let hung = if killed_at_limit(run.status.code()) {
        ", hung"
    } else {
        ""
    };
    let ok = |g: char| {
        run.stdout
            .lines()
            .any(|l| l == format!("spike-arm: ({g}) ok"))
    };
    match &spec.red {
        None => {
            let red: Vec<char> = GATES.into_iter().filter(|g| !ok(*g)).collect();
            ensure!(
                red.is_empty() && run.status.success(),
                "gates {red:?} not ok ({}{hung}), log {}",
                run.status,
                log.display()
            );
        }
        // red only when the gate itself failed: its FAIL line, or the process died inside it (its `run` marker is the last line), and no ok
        Some(Red::Gate(g)) => {
            let fail = format!("spike-arm: ({g}) FAIL");
            let marker = format!("spike-arm: ({g}) run");
            let failed = run.stdout.lines().any(|l| l.starts_with(&fail))
                || run.stdout.lines().last() == Some(marker.as_str());
            if ok(*g) || !failed || run.status.success() {
                bail!(
                    "break did not fail gate ({g}) ({}{hung}), log {}",
                    run.status,
                    log.display()
                );
            }
        }
        // red only when the boot itself ended: the pre-boot marker is the last line, nothing reached BOOT, and QEMU said why
        Some(Red::Boot(why)) => {
            let last = run.stdout.lines().last().unwrap_or_default();
            let said = run.stderr.contains(why) || run.stdout.contains(why);
            if ok('B') || last != "spike-arm: boot" || !said || run.status.success() {
                bail!(
                    "break did not end the boot with {why:?} ({}{hung}), log {}",
                    run.status,
                    log.display()
                );
            }
        }
    }
    Ok(())
}
