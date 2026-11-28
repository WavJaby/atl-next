// SPDX-License-Identifier: MIT
//! `xtask test rv-art --host <riscv64 host>`: the host build's ART (libart, atl-app-launcher) under qemu-user in this build machine, once per CPU model. The CPU-only proof that the build's isa is all ART needs: hand-written asm (nterp, entrypoints) and the JIT's emitted code.
//!
//! Models: `baseline` = the host row's isa as QEMU `rv64i` + exactly its extensions (rv_qemu.rs `baseline_cpu`); `--model` adds QEMU `-cpu` strings.
//! Checks, each per model (ART with no boot image, no binder: host_art.rs `art_command` with qemu in front):
//! 1. hello: `RvArt hello` prints its line (runtime start, class init interpreted, libcore natives).
//! 2. nterp: `RvArt loop` with `-Xusejit:false` prints the sum this file recomputes ([`expected_sum`]), and QEMU translated guest code inside libart's `ExecuteNterpImpl..EndExecuteNterpImpl` (`-d in_asm -dfilter`; libart's load address from a `-strace` run, every run under `setarch -R`: with host ASLR qemu-user's guest mmaps move between runs; each filtered run straces too and must see the same address).
//!    Control: `-Xint` (interpret-only: ART's switch interpreter, nterp.cc `CanRuntimeUseNterp`) translates no block there, so the probe responds to nterp use.
//! 3. jit: `RvArt loop` with `-Xusejit:true -verbose:jit` prints the same sum, ART logged `Compiling method` and `Compilation of` for `RvArt.hot` and no `Failed to compile`. A JIT that emits an extension the model lacks ends in SIGILL.
//!
//! Negative control, by hand: a build whose isa has an extension the model lacks must fail check 1 by SIGILL (proves the model lacks it).
//! Limit: the property area comes from the build machine's atl-property-service (its abilist is the build machine's); no check here reads the abilist.
//! Needs: `cargo xtask native --host <host> --for rv-art`; this checkout's build-machine build (Java tools, boot jars); the `qemu-user` and `setarch` deps rows; the host row's sysroot.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};
use object::{Object, ObjectSymbol};

use crate::host_art::{ArtRun, HostArt, JavaFixture, NO_BINDER};
use crate::{arches, cross, deps, rv_qemu, verdict};

const CMD: &str = "rv-art";
const FIXTURE: &str = "xtask-harness/testdata/rv-art/java";
const CLASS: &str = "RvArt";
/// The fixture's hot method as ART's PrettyMethod names it (the `-verbose:jit` lines).
const HOT: &str = "long RvArt.hot(int)";
/// hot()'s calls per loop run and its argument (RvArt.java): enough back-edges for the JIT to compile hot() well before the run ends.
const REPS: u32 = 200;
const HOT_N: i32 = 10_000;
/// The deps.toml row of the user-mode emulator.
const QEMU_ROW: &str = "qemu-user";
const SETARCH_ROW: &str = "setarch";
/// nterp's code range in libart (nterp.cc `ExecuteNterpImpl` / `EndExecuteNterpImpl`).
const NTERP_START: &str = "ExecuteNterpImpl";
const NTERP_END: &str = "EndExecuteNterpImpl";

#[derive(clap::Args)]
pub struct Args {
    /// The host: an arches.toml riscv64 `[host]` row or one of its variants (`riscv64-gc`).
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's build dir for --host).
    #[arg(long)]
    out: Option<PathBuf>,
    /// The build-machine build holding the Java tools and boot jars (default: this checkout's build dir).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// QEMU `-cpu` strings run besides the derived baseline.
    #[arg(long = "model")]
    models: Vec<String>,
    /// Print each command and its full output.
    #[arg(long)]
    verbose: bool,
}

/// What every run of one model shares.
struct Runner<'a> {
    art: &'a HostArt,
    fixture: &'a JavaFixture,
    /// `setarch` (deps row): qemu runs with ADDR_NO_RANDOMIZE, else host ASLR moves the guest's mmaps between runs
    setarch: String,
    qemu: String,
    cpu: String,
    /// QEMU's `-L`: where the guest's /lib/ld-linux-riscv64-lp64d.so.1 and glibc's absolute loads resolve.
    prefix: PathBuf,
    lib_path: String,
    work: PathBuf,
}

impl Runner<'_> {
    /// `RvArt <mode>` under the model with VM options `vm`; `qemu` = extra QEMU options before the guest program.
    fn run(&self, qemu: &[String], vm: &[&str], mode: &[&str]) -> Result<ArtRun> {
        let mut own: Vec<String> = vec![
            std::env::consts::ARCH.to_owned(),
            "-R".to_owned(),
            self.qemu.clone(),
            "-cpu".to_owned(),
            self.cpu.clone(),
            "-L".to_owned(),
            self.prefix.display().to_string(),
        ];
        own.extend(qemu.iter().cloned());
        own.push(self.art.launcher.display().to_string());
        own.extend(NO_BINDER.iter().map(|s| s.to_string()));
        let own: Vec<&str> = own.iter().map(String::as_str).collect();
        let fx = self.fixture;
        let mut cmd: Command = self.art.art_command(
            Path::new(&self.setarch),
            &own,
            &fx.work.join("data"),
            (&fx.area.0, &fx.area.1),
        );
        // qemu-user is static: the guest reads the library path; the sysroot's multiarch dir holds libc++ and libunwind
        cmd.env("LD_LIBRARY_PATH", &self.lib_path)
            .args(vm)
            .arg("-cp")
            .arg(&fx.dex)
            .arg(CLASS)
            .args(mode);
        self.art.run(&mut cmd)
    }

    /// libart's load address: the first mmap of the fd its openat returned, from a `-strace` run of `hello`.
    fn libart_base(&self) -> Result<std::result::Result<u64, String>> {
        let run = self.run(&["-strace".to_owned()], &[], &["hello"])?;
        Ok(libart_base(&run.stderr).ok_or_else(|| {
            format!(
                "no libart openat + mmap in -strace output ({})",
                run.failure()
            )
        }))
    }

    /// The run with QEMU logging each translated block inside `range` (absolute guest addresses, libart at `base`); the number of blocks logged.
    /// The run straces too: a libart base other than `base` (layout not reproduced) is an error, not a count.
    fn blocks_in(
        &self,
        base: u64,
        range: (u64, u64),
        tag: &str,
        vm: &[&str],
        mode: &[&str],
    ) -> Result<(ArtRun, std::result::Result<usize, String>)> {
        let log = self.work.join(format!("{tag}.in_asm"));
        let qemu = vec![
            "-strace".to_owned(),
            "-d".to_owned(),
            "in_asm".to_owned(),
            "-D".to_owned(),
            log.display().to_string(),
            "-dfilter".to_owned(),
            format!("{:#x}+{:#x}", range.0, range.1 - range.0),
        ];
        let run = self.run(&qemu, vm, mode)?;
        // with -D, qemu writes the -strace lines into the log too (qemu_log), unfiltered by -dfilter
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let seen = libart_base(&text);
        if seen != Some(base) {
            let err = format!(
                "{tag}: libart at {seen:x?}, not {base:#x} as in the -strace run: layout not reproduced"
            );
            return Ok((run, Err(err)));
        }
        Ok((
            run,
            Ok(text.lines().filter(|l| l.starts_with("IN:")).count()),
        ))
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let arches_table = arches::load(root)?;
    let row = arches::host(&arches_table, &args.host)?;
    let host = &row.host;
    let (Some(isa), Some(clang), Some(sysroot)) = (&host.isa, &host.clang, &host.sysroot) else {
        anyhow::bail!("{}: not a cross host (isa, clang, sysroot)", args.host);
    };
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, CMD)?;
    let java_out = crate::out::out_dir(root, args.java_out.as_deref())?;
    let property_service = crate::host_art::build_property_service(root, &java_out.join("cargo"))?;
    let art = HostArt::new(
        root,
        &args.host,
        Some(&java_out),
        &out,
        "rv-art-art",
        &property_service,
        args.verbose,
    )?;
    let work = out.join(CMD);
    let fixture = art.compile(&root.join(FIXTURE), &work)?;
    let sysroot = deps::sysroot_root(root, sysroot)?;
    let [lib, multiarch] = cross::sysroot_lib_dirs(&sysroot, clang);
    let lib_path = format!("{}:{}", art.lib_path.display(), multiarch.display());
    let nterp = nterp_range(&out.join("device/libart/libart.so"))?;
    let expected = format!("rv-art sum {}", expected_sum());

    let mut models = vec![(
        format!("baseline {isa}"),
        rv_qemu::baseline_cpu(&arches::isa_closure(isa)?),
    )];
    models.extend(args.models.iter().map(|m| (m.clone(), m.clone())));
    let mut fails = Vec::new();
    for (label, cpu) in &models {
        println!("model {label}: -cpu {cpu}");
        let run = Runner {
            art: &art,
            fixture: &fixture,
            setarch: deps::binary(root, SETARCH_ROW)?,
            qemu: deps::binary(root, QEMU_ROW)?,
            cpu: cpu.clone(),
            prefix: lib.parent().context("sysroot lib dir")?.to_owned(),
            lib_path: lib_path.clone(),
            work: work.clone(),
        };
        let mut model_fails = Vec::new();
        let mut check = |name: &str, r: std::result::Result<String, String>| match r {
            Ok(detail) => println!("  {name}: PASS {detail}"),
            Err(e) => {
                println!("  {name}: FAIL {e}");
                model_fails.push(format!("[{label}] {name}: {e}"));
            }
        };
        let hello = run.run(&[], &[], &["hello"])?;
        check("hello", printed(&hello, "rv-art hello"));
        let loop_args = ["loop", &REPS.to_string()].map(str::to_owned);
        let loop_args: Vec<&str> = loop_args.iter().map(String::as_str).collect();
        match run.libart_base()? {
            Err(e) => check("nterp", Err(e)),
            Ok(base) => {
                let range = (base + nterp.0, base + nterp.1);
                let (on, on_blocks) =
                    run.blocks_in(base, range, "nterp", &["-Xusejit:false"], &loop_args)?;
                let (int, int_blocks) =
                    run.blocks_in(base, range, "xint", &["-Xint"], &["hello"])?;
                check(
                    "nterp",
                    on_blocks.and_then(|on_n| {
                        int_blocks.and_then(|int_n| {
                            nterp_result(
                                &printed(&on, &expected),
                                on_n,
                                &printed(&int, "rv-art hello"),
                                int_n,
                            )
                        })
                    }),
                );
            }
        }
        let jit = run.run(&[], &["-Xusejit:true", "-verbose:jit"], &loop_args)?;
        check(
            "jit",
            printed(&jit, &expected).and_then(|_| jit_result(&jit.stderr)),
        );
        println!(
            "{}",
            verdict::gate(&format!("{CMD}[{label}]"), &model_fails)
        );
        fails.extend(model_fails);
    }
    println!("{}", verdict::gate(CMD, &fails));
    Ok(if fails.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Ok when `run` exited 0 and printed `line`; else why not (a signal named: SIGILL is the missing-extension answer).
fn printed(run: &ArtRun, line: &str) -> std::result::Result<String, String> {
    if run.ok && run.stdout.lines().any(|l| l.trim() == line) {
        Ok(format!("`{line}`"))
    } else if let Some(sig) = run.signal {
        Err(format!(
            "signal {sig}{} ({})",
            if sig == 4 { " SIGILL" } else { "" },
            run.failure()
        ))
    } else {
        Err(format!("no `{line}` in stdout: {}", run.failure()))
    }
}

/// Check 2: the nterp run printed the sum with blocks in nterp's range, the -Xint control ran with none there.
fn nterp_result(
    on: &std::result::Result<String, String>,
    on_blocks: usize,
    int: &std::result::Result<String, String>,
    int_blocks: usize,
) -> std::result::Result<String, String> {
    let on = on.as_ref().map_err(|e| format!("-Xusejit:false: {e}"))?;
    int.as_ref().map_err(|e| format!("-Xint control: {e}"))?;
    ensure_blocks(on_blocks, int_blocks)?;
    Ok(format!(
        "{on}, {on_blocks} translated blocks in nterp; -Xint control 0"
    ))
}

fn ensure_blocks(on: usize, int: usize) -> std::result::Result<(), String> {
    if on == 0 {
        return Err(
            "-Xusejit:false translated no block in nterp's range: nterp did not run".to_owned(),
        );
    }
    if int != 0 {
        return Err(format!(
            "-Xint control translated {int} blocks in nterp's range: the probe does not tell nterp from the switch interpreter"
        ));
    }
    Ok(())
}

/// Check 3's log part: hot() was compiled and none of its compilations failed.
fn jit_result(stderr: &str) -> std::result::Result<String, String> {
    let has = |what: &str| stderr.lines().any(|l| l.contains(what));
    let compiling = format!("Compiling method {HOT}");
    if has(&format!("Failed to compile method {HOT}")) {
        return Err(format!("ART logged a failed compilation of {HOT}"));
    }
    if !has(&compiling) || !has(&format!("Compilation of {HOT} took")) {
        return Err(format!(
            "no `{compiling}` + `Compilation of {HOT} took` in -verbose:jit output"
        ));
    }
    let kinds: BTreeSet<&str> = stderr
        .lines()
        .filter(|l| l.contains(&compiling))
        .filter_map(|l| l.split("kind=").nth(1))
        .map(str::trim)
        .collect();
    Ok(format!(
        "{HOT} compiled ({})",
        kinds.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

/// libart's load address from qemu `-strace` lines: `openat(...libart.so...) = <fd>`, then the first `mmap(...,<fd>,0) = <addr>`.
fn libart_base(strace: &str) -> Option<u64> {
    let mut fd: Option<String> = None;
    for line in strace.lines() {
        match &fd {
            None if line.contains("openat(") && line.contains("/libart.so\"") => {
                fd = line
                    .rsplit_once(" = ")
                    .map(|(_, v)| v.trim().to_owned())
                    .filter(|v| !v.starts_with('-'));
            }
            Some(f) if line.contains("mmap(") && line.contains(&format!(",{f},0)")) => {
                let addr = line.rsplit_once(" = ")?.1.trim();
                return u64::from_str_radix(addr.trim_start_matches("0x"), 16).ok();
            }
            _ => {}
        }
    }
    None
}

/// nterp's [start, end) as offsets in libart (its symbol table; lld links the first PT_LOAD at vaddr 0, so offset = vaddr).
fn nterp_range(libart: &Path) -> Result<(u64, u64)> {
    let data = std::fs::read(libart).with_context(|| format!("reading {}", libart.display()))?;
    let file =
        object::File::parse(&*data).with_context(|| format!("parsing {}", libart.display()))?;
    let find = |name: &str| {
        file.symbols()
            .chain(file.dynamic_symbols())
            .find(|s| s.name() == Ok(name))
            .map(|s| s.address())
            .with_context(|| format!("{}: no symbol {name}", libart.display()))
    };
    let (start, end) = (find(NTERP_START)?, find(NTERP_END)?);
    ensure!(
        start < end,
        "{}: {NTERP_START} {start:#x} not below {NTERP_END} {end:#x}",
        libart.display()
    );
    Ok((start, end))
}

/// The loop run's printed sum: RvArt.java `main` over `hot`, in Java's wrapping long arithmetic.
fn expected_sum() -> i64 {
    (0..REPS).fold(0i64, |r, _| r.wrapping_add(hot(HOT_N)))
}

/// RvArt.java `hot`, transcribed: `>>>` = logical shift, `+=`/`*` wrap, `Integer.bitCount` = count_ones.
fn hot(n: i32) -> i64 {
    let mut a = [0i64; 64];
    let mut s: i64 = 0;
    for i in 0..n {
        let li = i64::from(i);
        s = s.wrapping_add(li.wrapping_mul(li) ^ ((s as u64 >> 3) as i64));
        s = s.rotate_left(7).wrapping_add(i64::from(i.count_ones()));
        let k = (i & 63) as usize;
        a[k] = a[k].wrapping_add(s.max(li).wrapping_sub(s.min(-li)));
    }
    s ^ a.iter().fold(0, |t, v| t ^ v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libart_base_from_qemu_strace() {
        let log = "10 openat(AT_FDCWD,\"/o/lib64/libc++.so\",O_RDONLY|O_CLOEXEC) = 3\n10 mmap(NULL,4096,PROT_READ,MAP_PRIVATE,3,0) = 0x0000002000000000\n10 openat(AT_FDCWD,\"/o/lib64/libart.so\",O_RDONLY|O_CLOEXEC) = 4\n10 read(4,0x1,832) = 832\n10 mmap(NULL,4096,PROT_READ,MAP_PRIVATE,3,0) = 0x0000002000100000\n10 mmap(NULL,9999,PROT_READ,MAP_PRIVATE|MAP_DENYWRITE,4,0) = 0x0000002000200000\n";
        assert_eq!(libart_base(log), Some(0x2000200000));
        // a failed open has no base
        assert_eq!(
            libart_base("1 openat(AT_FDCWD,\"/o/libart.so\",O_RDONLY) = -1 errno=2\n"),
            None
        );
    }

    #[test]
    fn nterp_probe_must_respond() {
        assert!(ensure_blocks(5, 0).is_ok());
        assert!(ensure_blocks(0, 0).unwrap_err().contains("did not run"));
        assert!(ensure_blocks(5, 1).unwrap_err().contains("does not tell"));
    }

    #[test]
    fn jit_lines() {
        let ok = format!(
            "I jit: Compiling method {HOT} kind=Baseline\nI jit: Compilation of {HOT} took 3ms\n"
        );
        assert_eq!(
            jit_result(&ok).unwrap(),
            format!("{HOT} compiled (Baseline)")
        );
        assert!(jit_result("nothing").is_err());
        assert!(
            jit_result(&format!(
                "{ok}Failed to compile method {HOT} kind=Optimized\n"
            ))
            .is_err()
        );
    }

    #[test]
    fn hot_matches_hand_steps() {
        // n = 2 by hand: i=0: s = 0 ^ 0 = 0, rotl 0 + 0 = 0, a[0] += max(0,0) - min(0,0) = 0; i=1: s = 0 + (1 ^ 0) = 1, rotl7 = 128 + 1 = 129, a[1] += 129 - (-1) = 130; result 129 ^ 130 = 3
        assert_eq!(hot(2), 3);
    }
}
