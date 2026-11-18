// SPDX-License-Identifier: MIT
//! `xtask test rv-qemu --host riscv64`: the riscv64 host's CPU-side checks under qemu-user in this build machine, once per CPU model: the host row's baseline and V models standing in for RVA23 cores (no V hardware is available; sifive's P550 has no V).
//!
//! Models: `baseline` is derived from the host row's `isa` (arches.toml) as QEMU's bare `rv64i` plus exactly the row's extensions, so nothing past the row is on (QEMU's `rv64` default adds zbs, zbc, zicboz, zfa, ...). `--model` adds QEMU `-cpu` strings; default [`V_MODELS`].
//! Which tier a model is, is probed, never read from its name: the probe fixture prints AT_HWCAP and riscv_hwprobe, and every expectation below follows the probed V bit.
//!
//! Checks, each per model:
//! 1. probe (testdata/rv-qemu/isa_probe.c): baseline = AT_HWCAP letters equal the row's single-letter extensions, hwprobe names only extensions the row allows and all its Z extensions.
//!    Then one V instruction: SIGILL without V (the red control: the model matters), `vlenb` = VLEN/8 with it (`vlen=<bits>` from the model string). At least one model must probe V=1 and the baseline V=0.
//! 2. zlib (testdata/rv-qemu/zlib_tier.c, `<out>/device/libz`): adler32 against RFC 1950's definition and a compress/uncompress round trip; the tier zlib took (its own AT_HWCAP probe) equals the probe's, and QEMU's translation log holds V instructions exactly when V=1 (adler32_simd.c is the only V code in the process: isa-tiers.toml).
//! 3. unwind (testdata/rv-qemu/unwind_vlenb.cpp, `<out>/device/libunwindstack`): an unwind from a signal's ucontext, as ART's fatal-signal dump starts one, stores the probed vlenb (0 without V: libunwindstack patch 0002) and reaches `main`.
//! 4. units: the lib unit tests of [`UNIT_CRATES`] cross-built for the host's Rust target (`cross::cargo_target`) and run under the model.
//!
//! `--break baseline`: QEMU's default `rv64` replaces the derived baseline; check 1 must turn red (extensions past the row).
//! Needs: `cargo xtask native --host riscv64 --for rv-qemu`; the `qemu-user` deps row; the host row's sysroot and compilers.
//! Default: mutating (builds the fixtures into `<out>/rv-qemu`, links the runtime libraries there, cross-builds the unit tests into the checkout's target dir).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output};

use anyhow::{Context, Result, bail, ensure};

use crate::{arches, cross, deps, verdict};

const CMD: &str = "rv-qemu";
const FIXTURES: &str = "xtask-harness/testdata/rv-qemu";
/// The deps.toml row of the user-mode emulator.
const QEMU_ROW: &str = "qemu-user";
/// QEMU `-cpu` strings of the V tier: RVA23U64, the profile of the RVA23 cores ATL may meet (SiFive P800 series "fully compliant with RVA23", 2x128b VLEN, sifive.com/cores/performance-p870 read 2026-11-19; AOSP riscv64 devices),
/// at VLEN 128 (RVA23's minimum, Zvl128b, and the P800 series' width) and 256 (a wider core, so code that reads vlenb sees a second value).
const V_MODELS: [&str; 2] = ["rva23u64,vlen=128", "rva23u64,vlen=256"];
/// Crates holding `target_arch = "riscv64"` code whose lib unit tests run in one user process. Not atl-broker: its seccomp and namespace tests need the kernel (qemu-user answers seccomp with ENOSYS) and its closure test reads the loader at the build machine's /lib.
const UNIT_CRATES: [&str; 2] = ["atl-property-service", "atl-native-bridge"];
/// SIGILL: how a core without V answers the probe's vsetvli.
const SIGILL: i32 = 4;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch: an arches.toml row with `[host]` and an rv64 `isa`.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's build dir for --host).
    #[arg(long)]
    out: Option<PathBuf>,
    /// QEMU `-cpu` strings run besides the derived baseline (default: RVA23U64 at VLEN 128 and 256).
    #[arg(long = "model")]
    models: Vec<String>,
    /// Deliberate break that must turn the gate red: `baseline` runs QEMU's default `rv64` as the baseline.
    #[arg(long, value_parser = ["baseline"])]
    r#break: Option<String>,
    /// Print each command and its full output.
    #[arg(long)]
    verbose: bool,
}

/// A CPU model: the label lines carry, and the `-cpu` string.
struct Model {
    label: String,
    cpu: String,
    baseline: bool,
}

/// The host row's cross toolchain and the run environment of its outputs.
struct Toolchain {
    cc: String,
    cxx: String,
    /// `--target --sysroot -march -fuse-ld=lld`: the converter's host modules' own.
    flags: Vec<String>,
    libcxx: PathBuf,
    /// QEMU's `-L`: the prefix the interpreter path (/lib/ld-linux-riscv64-lp64d.so.1) and glibc's absolute loads resolve under.
    prefix: PathBuf,
    /// The sysroot's multiarch dir (libc++, libunwind): on every run's library path.
    multiarch: PathBuf,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let arches_table = arches::load(root)?;
    let host = arches_table
        .get(&args.host)
        .and_then(|a| a.host.as_ref())
        .with_context(|| format!("{}: no [{}.host] row", arches::FILE, args.host))?;
    let (Some(isa), Some(clang), Some(sysroot), Some(cc), Some(cxx)) =
        (&host.isa, &host.clang, &host.sysroot, &host.cc, &host.cxx)
    else {
        bail!(
            "[{}.host] has no isa, clang, sysroot, cc and cxx: not a cross host",
            args.host
        );
    };
    let closure = arches::isa_closure(isa)?;
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, CMD)?;
    let qemu = deps::binary(root, QEMU_ROW)?;
    let sysroot = deps::sysroot_root(root, sysroot)?;
    let [lib, multiarch] = cross::sysroot_lib_dirs(&sysroot, clang);
    let tc = Toolchain {
        cc: deps::binary(root, cc)?,
        cxx: deps::binary(root, cxx)?,
        flags: vec![
            format!("--target={clang}"),
            format!("--sysroot={}", sysroot.display()),
            format!("-march={isa}"),
            "-fuse-ld=lld".to_owned(),
        ],
        libcxx: atl_layout::libcxx_dir(&sysroot)?,
        prefix: lib.parent().context("sysroot lib dir")?.to_owned(),
        multiarch,
    };

    let work = out.join(CMD);
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    let libs = work.join("lib");
    std::fs::create_dir_all(&libs)?;
    for f in crate::host_art::runtime_libs(&out)? {
        let name = f.file_name().context("a file name")?;
        std::os::unix::fs::symlink(&f, libs.join(name))?;
    }
    let lib_path = format!("{}:{}", libs.display(), tc.multiarch.display());
    let tree = crate::tree::shared_tree(&atl_layout::Layout::of(root)?);
    let fx = root.join(FIXTURES);
    let probe = work.join("isa_probe");
    tc.compile_c(&fx.join("isa_probe.c"), &probe, &[])?;
    let zlib = work.join("zlib_tier");
    tc.compile_c(
        &fx.join("zlib_tier.c"),
        &zlib,
        &[
            format!("-I{}", tree.join("external/zlib").display()),
            format!("-L{}", out.join("device/libz").display()),
            "-lz".to_owned(),
        ],
    )?;
    let unwind = work.join("unwind_vlenb");
    tc.compile_cxx(
        &fx.join("unwind_vlenb.cpp"),
        &unwind,
        &[
            format!(
                "-I{}",
                tree.join("system/unwinding/libunwindstack/include")
                    .display()
            ),
            format!("-I{}", tree.join("system/libbase/include").display()),
            format!("-L{}", out.join("device/libunwindstack").display()),
            "-lunwindstack".to_owned(),
            // libunwindstack's own DT_NEEDED (libbase, liblog, ...) resolve at run time from the library path, not at this link
            "-Wl,--allow-shlib-undefined".to_owned(),
        ],
    )?;
    let units = unit_tests(root, &args.host)?;

    let baseline_cpu = match args.r#break.as_deref() {
        Some("baseline") => "rv64".to_owned(),
        _ => baseline_cpu(&closure),
    };
    let mut models = vec![Model {
        label: format!("baseline {isa}"),
        cpu: baseline_cpu,
        baseline: true,
    }];
    let extra: Vec<String> = if args.models.is_empty() {
        V_MODELS.iter().map(|m| m.to_string()).collect()
    } else {
        args.models.clone()
    };
    models.extend(extra.into_iter().map(|cpu| Model {
        label: cpu.clone(),
        cpu,
        baseline: false,
    }));

    let run = Runner {
        qemu,
        prefix: tc.prefix.clone(),
        lib_path,
        verbose: args.verbose,
    };
    let mut fails = Vec::new();
    let mut tiers = Vec::new();
    for model in &models {
        println!("model {}: -cpu {}", model.label, model.cpu);
        let mut model_fails = Vec::new();
        let mut check = |name: &str, r: Result<String, String>| match r {
            Ok(detail) => println!("  {name}: PASS {detail}"),
            Err(e) => {
                println!("  {name}: FAIL {e}");
                model_fails.push(format!("[{}] {name}: {e}", model.label));
            }
        };
        let probed = run.probe(model, &probe)?;
        let v = probed.as_ref().ok().map(|p| p.v);
        check(
            "probe",
            probed
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|p| probe_result(p, model, &closure, isa)),
        );
        if let Ok(p) = &probed {
            tiers.push((model.baseline, p.v));
            check("zlib", run.zlib(model, &zlib, &work, p.v)?);
            check("unwind", run.unwind(model, &unwind, p.vlenb)?);
        }
        for (name, exe) in &units {
            check(&format!("units {name}"), run.units(model, exe)?);
        }
        let tier = v.map_or("?".to_owned(), |v| format!("V={}", u8::from(v)));
        println!(
            "{}",
            verdict::gate(&format!("{CMD}[{} {tier}]", model.label), &model_fails)
        );
        fails.extend(model_fails);
    }
    // both tiers covered: else the run proved nothing about the other
    if !tiers.iter().any(|&(baseline, v)| baseline && !v) {
        fails.push("no baseline model probed V=0".to_owned());
    }
    if !tiers.iter().any(|&(_, v)| v) {
        fails.push("no model probed V=1: the V tier ran nowhere".to_owned());
    }
    println!("{}", verdict::gate(CMD, &fails));
    Ok(if fails.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// QEMU's bare `rv64i` with each extension of the row's closure switched on: `rv64gc_zba_zbb` -> `rv64i,a=true,c=true,d=true,...,zbb=true`. The implied ones (zmmul, zca, ...) are named too, so QEMU's own implication rules cannot differ from the row's.
fn baseline_cpu(closure: &BTreeSet<String>) -> String {
    std::iter::once("rv64i".to_owned())
        .chain(
            closure
                .iter()
                .filter(|e| e.as_str() != "i")
                .map(|e| format!("{e}=true")),
        )
        .collect::<Vec<_>>()
        .join(",")
}

/// What the probe fixture printed.
#[derive(Debug)]
struct Probe {
    hwcap: String,
    hwprobe: BTreeSet<String>,
    v: bool,
    /// None: the V instruction did not run.
    vlenb: Option<u64>,
    signal: Option<i32>,
}

/// Check 1's verdict for `p` under `model`; `closure` / `isa` are the host row's.
fn probe_result(
    p: &Probe,
    model: &Model,
    closure: &BTreeSet<String>,
    isa: &str,
) -> Result<String, String> {
    let shown = format!(
        "hwcap {} hwprobe {}",
        p.hwcap,
        p.hwprobe.iter().cloned().collect::<Vec<_>>().join(" ")
    );
    if model.baseline {
        let letters: String = closure
            .iter()
            .filter(|e| e.len() == 1)
            .map(String::as_str)
            .collect();
        let mut hwcap: Vec<char> = p.hwcap.chars().collect();
        hwcap.sort_unstable();
        let hwcap: String = hwcap.into_iter().collect();
        if hwcap != letters {
            return Err(format!(
                "AT_HWCAP letters {hwcap}, the row {isa} has {letters}"
            ));
        }
        // hwprobe names F+D as one bit, `fd`
        let allowed = |e: &String| e == "fd" || closure.contains(e);
        let extra: Vec<&String> = p.hwprobe.iter().filter(|e| !allowed(e)).collect();
        if !extra.is_empty() {
            return Err(format!("hwprobe reports {extra:?}, past the row {isa}"));
        }
        let rows_z = arches::isa_extensions(isa).map_err(|e| e.to_string())?;
        let missing: Vec<&String> = rows_z.iter().filter(|e| !p.hwprobe.contains(*e)).collect();
        if !missing.is_empty() {
            return Err(format!("hwprobe lacks {missing:?} of the row {isa}"));
        }
    }
    if p.v != p.hwprobe.contains("v") {
        return Err(format!("AT_HWCAP and hwprobe disagree on V: {shown}"));
    }
    match (p.v, p.vlenb, p.signal) {
        (false, None, Some(SIGILL)) => Ok(format!("V=0, vsetvli -> SIGILL; {shown}")),
        (false, _, _) => Err(format!(
            "no V probed, but the V instruction did not raise SIGILL (vlenb {:?}, signal {:?}); {shown}",
            p.vlenb, p.signal
        )),
        (true, Some(vlenb), None) => match model_vlen(&model.cpu) {
            Some(bits) if vlenb * 8 != bits => Err(format!(
                "vlenb {vlenb}, the model's vlen={bits} means {}",
                bits / 8
            )),
            _ if vlenb == 0 => Err("vlenb 0 with V".to_owned()),
            _ => Ok(format!("V=1, vlenb {vlenb}; {shown}")),
        },
        (true, _, _) => Err(format!(
            "V probed, but the V instruction did not run (signal {:?}); {shown}",
            p.signal
        )),
    }
}

/// `vlen=<bits>` of a QEMU `-cpu` string.
fn model_vlen(cpu: &str) -> Option<u64> {
    cpu.split(',')
        .find_map(|p| p.strip_prefix("vlen=")?.parse().ok())
}

impl Toolchain {
    fn compile_c(&self, src: &Path, exe: &Path, extra: &[String]) -> Result<()> {
        let mut cmd = Command::new(&self.cc);
        cmd.args(&self.flags)
            .args(["-O1", "-g", "-Wall", "-Werror"])
            .arg(src)
            .arg("-o")
            .arg(exe)
            .args(extra);
        crate::host_art::exec(&mut cmd, &format!("compiling {}", src.display()))
    }

    fn compile_cxx(&self, src: &Path, exe: &Path, extra: &[String]) -> Result<()> {
        let mut cmd = Command::new(&self.cxx);
        cmd.args(&self.flags)
            .args([
                "-O1",
                "-g",
                "-Wall",
                "-Werror",
                "-std=c++20",
                "-stdlib=libc++",
                "-nostdinc++",
                "-isystem",
            ])
            .arg(&self.libcxx)
            .arg(src)
            .arg("-o")
            .arg(exe)
            .args(extra);
        crate::host_art::exec(&mut cmd, &format!("compiling {}", src.display()))
    }
}

/// The lib unit-test executables of [`UNIT_CRATES`] for `host`'s Rust target: (crate, executable).
fn unit_tests(root: &Path, host: &str) -> Result<Vec<(String, PathBuf)>> {
    let target = cross::cargo_target(root, host)?;
    let mut cmd = crate::cargo_command();
    cmd.current_dir(root)
        .args([
            "test",
            "--no-run",
            "--lib",
            "--message-format=json",
            "--target",
            &target.triple,
        ])
        .envs(target.env.iter().map(|(k, v)| (k, v)));
    for c in UNIT_CRATES {
        cmd.args(["-p", c]);
    }
    let output = cmd.output().context("running cargo test --no-run")?;
    ensure!(
        output.status.success(),
        "cargo test --no-run for {}: {}\n{}",
        target.triple,
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let mut exes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if msg["reason"] != "compiler-artifact" || msg["profile"]["test"] != true {
            continue;
        }
        if let Some(exe) = msg["executable"].as_str() {
            let name = msg["target"]["name"]
                .as_str()
                .unwrap_or("?")
                .replace('_', "-");
            exes.push((name, PathBuf::from(exe)));
        }
    }
    ensure!(
        exes.len() == UNIT_CRATES.len(),
        "cargo built {} unit-test executables for {:?}, expected one each",
        exes.len(),
        UNIT_CRATES
    );
    Ok(exes)
}

/// How a fixture runs under qemu-user.
struct Runner {
    qemu: String,
    prefix: PathBuf,
    lib_path: String,
    verbose: bool,
}

impl Runner {
    fn command(&self, model: &Model, log: Option<&Path>) -> Command {
        let mut cmd = Command::new(&self.qemu);
        cmd.arg("-cpu")
            .arg(&model.cpu)
            .arg("-L")
            .arg(&self.prefix)
            .arg("-E")
            .arg(format!("LD_LIBRARY_PATH={}", self.lib_path));
        if let Some(log) = log {
            // every translated block's guest instructions, each block once
            cmd.args(["-d", "in_asm", "-D"]).arg(log);
        }
        cmd
    }

    fn output(&self, mut cmd: Command, what: &str) -> Result<Output> {
        let output = cmd
            .output()
            .with_context(|| format!("running {what} under qemu-user"))?;
        if self.verbose {
            println!(
                "  $ {cmd:?}\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(output)
    }

    fn probe(&self, model: &Model, exe: &Path) -> Result<Result<Probe, String>> {
        let mut cmd = self.command(model, None);
        cmd.arg(exe);
        let output = self.output(cmd, "the probe")?;
        Ok(parse_probe(&output))
    }

    fn zlib(
        &self,
        model: &Model,
        exe: &Path,
        work: &Path,
        v: bool,
    ) -> Result<Result<String, String>> {
        let log = work.join(format!("zlib-{}.asm", model.cpu.replace([',', '='], "-")));
        let mut cmd = self.command(model, Some(&log));
        cmd.arg(exe);
        let output = self.output(cmd, "the zlib check")?;
        let asm = std::fs::read_to_string(&log).unwrap_or_default();
        Ok(zlib_result(&output, &asm, v))
    }

    fn unwind(
        &self,
        model: &Model,
        exe: &Path,
        vlenb: Option<u64>,
    ) -> Result<Result<String, String>> {
        let mut cmd = self.command(model, None);
        cmd.arg(exe);
        let output = self.output(cmd, "the unwind check")?;
        Ok(unwind_result(&output, vlenb.unwrap_or(0)))
    }

    fn units(&self, model: &Model, exe: &Path) -> Result<Result<String, String>> {
        let mut cmd = self.command(model, None);
        cmd.arg(exe).arg("-q");
        let output = self.output(cmd, "unit tests")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let summary = text
            .lines()
            .rev()
            .find(|l| l.starts_with("test result:"))
            .unwrap_or("no test result line")
            .trim()
            .to_owned();
        if output.status.success() {
            return Ok(Ok(summary));
        }
        let failed: Vec<&str> = text
            .lines()
            .filter_map(|l| l.trim().strip_suffix(" --- FAILED"))
            .collect();
        Ok(Err(format!("{summary}; failed: {}", failed.join(", "))))
    }
}

fn parse_probe(output: &Output) -> Result<Probe, String> {
    use std::os::unix::process::ExitStatusExt;
    let text = String::from_utf8_lossy(&output.stdout);
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ').map(str::trim))
    };
    let hwcap =
        field("hwcap").ok_or_else(|| format!("no hwcap line ({}): {text}", output.status))?;
    let hwprobe =
        field("hwprobe").ok_or_else(|| format!("no hwprobe line ({}): {text}", output.status))?;
    if hwprobe.starts_with("unavailable") {
        return Err(format!("riscv_hwprobe {hwprobe}"));
    }
    Ok(Probe {
        hwcap: hwcap.to_owned(),
        hwprobe: hwprobe.split_whitespace().map(str::to_owned).collect(),
        v: hwcap.contains('v'),
        vlenb: field("vlenb").and_then(|v| v.parse().ok()),
        signal: output.status.signal(),
    })
}

/// V instructions in a QEMU `in_asm` log: the configuration ones and every `v`-mnemonic with a vector operand.
fn v_instructions(asm: &str) -> usize {
    asm.lines()
        .filter(|l| {
            let mut words = l.split_whitespace().skip_while(|w| !w.ends_with(':'));
            // `0x...:  <encoding>  <mnemonic> <operands>`
            let _ = words.next();
            let _ = words.next();
            let (Some(op), Some(args)) = (words.next(), words.next()) else {
                return false;
            };
            op.starts_with("vset")
                || (op.starts_with('v')
                    && args.starts_with('v')
                    && args[1..].starts_with(|c: char| c.is_ascii_digit()))
        })
        .count()
}

fn zlib_result(output: &Output, asm: &str, v: bool) -> Result<String, String> {
    let text = String::from_utf8_lossy(&output.stdout);
    let tier = text
        .lines()
        .find_map(|l| l.strip_prefix("tier v="))
        .map(str::trim);
    if tier != Some(if v { "1" } else { "0" }) {
        return Err(format!(
            "zlib's tier {tier:?}, the probe's V={} ({})",
            u8::from(v),
            output.status
        ));
    }
    if !output.status.success() {
        let bad: Vec<&str> = text
            .lines()
            .filter(|l| !l.ends_with(" ok") && !l.starts_with("tier"))
            .collect();
        return Err(format!("{} ({})", bad.join("; "), output.status));
    }
    let n = v_instructions(asm);
    match (v, n) {
        (true, 0) => Err(
            "V tier probed, but QEMU translated no V instruction: adler32_simd did not run"
                .to_owned(),
        ),
        (false, n) if n > 0 => Err(format!("no V, yet {n} V instructions translated")),
        _ => Ok(format!(
            "tier V={}, {} V instructions translated, {}",
            u8::from(v),
            n,
            text.lines()
                .filter(|l| l.ends_with(" ok"))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn unwind_result(output: &Output, vlenb: u64) -> Result<String, String> {
    let text = String::from_utf8_lossy(&output.stdout);
    let got = text
        .lines()
        .find_map(|l| l.strip_prefix("vlenb "))
        .map(str::trim);
    let frames = text
        .lines()
        .find(|l| l.starts_with("frames "))
        .unwrap_or("no frames line");
    if got != Some(vlenb.to_string().as_str()) {
        return Err(format!(
            "unwinder vlenb {got:?}, the probe's {vlenb} ({}); {frames}",
            output.status
        ));
    }
    if !output.status.success() {
        return Err(format!("{frames} ({})", output.status));
    }
    Ok(format!("vlenb {vlenb}, {frames}"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    use super::*;

    fn out(stdout: &str, status: ExitStatus) -> Output {
        Output {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn model(cpu: &str, baseline: bool) -> Model {
        Model {
            label: cpu.to_owned(),
            cpu: cpu.to_owned(),
            baseline,
        }
    }

    const ISA: &str = "rv64gc_zba_zbb";

    /// The baseline names every closure member QEMU must switch on, nothing else. Expected list = isa_closure's doc example for this row, minus the base `i`.
    #[test]
    fn baseline_switches_on_the_rows_closure() {
        assert_eq!(
            baseline_cpu(&arches::isa_closure(ISA).unwrap()),
            "rv64i,a=true,c=true,d=true,f=true,m=true,zaamo=true,zalrsc=true,zba=true,zbb=true,zca=true,zcd=true,zicsr=true,zifencei=true,zmmul=true"
        );
    }

    /// Lines as the fixture printed them under QEMU 10.0.13 on 2026-11-19 (baseline, QEMU's `rv64`, rva23u64 vlen=128).
    #[test]
    fn probe_judges_the_tier_by_what_it_probed() {
        let closure = arches::isa_closure(ISA).unwrap();
        let sigill = ExitStatus::from_raw(SIGILL);
        let base = parse_probe(&out("hwcap acdfim\nhwprobe fd c zba zbb\n", sigill)).unwrap();
        assert!(probe_result(&base, &model("b", true), &closure, ISA).is_ok());
        // QEMU's default rv64 as the baseline (--break baseline): extensions past the row
        let wide = parse_probe(&out(
            "hwcap acdfim\nhwprobe fd c zba zbb zbs zicboz zbc zihintntl zfa\n",
            sigill,
        ))
        .unwrap();
        let e = probe_result(&wide, &model("rv64", true), &closure, ISA).unwrap_err();
        assert!(e.contains("past the row"), "{e}");
        // a model without V whose vsetvli ran: the control did not fire
        let ran = parse_probe(&out(
            "hwcap acdfim\nhwprobe fd c zba zbb\nvlenb 16\n",
            ExitStatus::from_raw(0),
        ))
        .unwrap();
        assert!(probe_result(&ran, &model("b", true), &closure, ISA).is_err());
        let v = parse_probe(&out(
            "hwcap acdfimv\nhwprobe fd c v zba zbb zbs\nvlenb 16\n",
            ExitStatus::from_raw(0),
        ))
        .unwrap();
        assert!(probe_result(&v, &model("rva23u64,vlen=128", false), &closure, ISA).is_ok());
        let e = probe_result(&v, &model("rva23u64,vlen=256", false), &closure, ISA).unwrap_err();
        assert!(e.contains("vlen=256"), "{e}");
    }

    /// `in_asm` lines as QEMU 10.0.13 prints them; a scalar `vsub`-like name without a vector operand is not counted.
    #[test]
    fn v_instructions_counts_vector_ops_only() {
        let asm = "0x00007f1c2a3b4c10:  0c0072d7          vsetvli                 t0,zero,e8,m1,ta,ma\n0x00007f1c2a3b4c14:  02058407          vle8.v                  v8,(a1)\n0x00007f1c2a3b4c18:  00b50533          add                     a0,a0,a1\n";
        assert_eq!(v_instructions(asm), 2);
        assert_eq!(v_instructions(""), 0);
    }

    #[test]
    fn zlib_needs_v_code_exactly_with_v() {
        let ok = out(
            "tier v=1\nadler32 11 sizes ok\nroundtrip 11 sizes ok\n",
            ExitStatus::from_raw(0),
        );
        let asm = "0x10:  0c0072d7          vsetvli                 t0,zero,e8,m1,ta,ma\n";
        assert!(zlib_result(&ok, asm, true).is_ok());
        assert!(
            zlib_result(&ok, "", true)
                .unwrap_err()
                .contains("no V instruction")
        );
        assert!(zlib_result(&ok, asm, false).is_err());
    }

    #[test]
    fn unwind_needs_the_probed_vlenb_and_main() {
        let good = out("vlenb 0\nframes 7 main at #3\n", ExitStatus::from_raw(0));
        assert!(unwind_result(&good, 0).is_ok());
        assert!(unwind_result(&good, 16).is_err());
        // before patch 0002 on a core without V: SIGILL inside the handler, nothing printed
        let died = out("", ExitStatus::from_raw(SIGILL));
        assert!(unwind_result(&died, 0).is_err());
    }
}
