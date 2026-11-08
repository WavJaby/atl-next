// SPDX-License-Identifier: MIT
//! `xtask test qemu-spike`: the QEMU engine spike (crates/atl-qemu-engine, QEMU bridge plan Q2) against its fixture, then every break.
//!
//! Library: the `--pic` build's own link line for `qemu-<target>` (`ninja -t commands`), linked `-shared` instead, `main` aliased `atl_qemu_user_main` (`--defsym`), QEMU's plugin `--dynamic-list` dropped. No QEMU file is compiled differently: this is D7 with a link-time change only.
//! Fixture: `xtask/testdata/qemu-spike/fixture.c`, static bionic by the NDK clang; QEMU's `main` loads it with `-B <guest base>`. Its binding pool (gate l) is `atl_bridgegen::aarch64::pool_asm` text, assembled by the same clang into the fixture; `check_pool` reads the pool back from the built binary.
//! Harness: `cargo test -p atl-qemu-engine --features qemu --test spike` with `ATL_QEMU_BUILD` (layouts), `ATL_QEMU_SPIKE_SO`, `ATL_QEMU_SPIKE_FIXTURE`, `ATL_QEMU_SIGCHAIN_OBJ` (AOSP libsigchain's object, built here from `art/sigchainlib/sigchain.cc` as-is); its own target dir under the spike dir, so the cfg breaks rebuild only it.
//! Pass: the plain run prints `spike: (x) ok` for every gate and exits 0; each break run leaves its gate without `ok` and exits nonzero.
//! Breaks: `no-p1` relinks with the pin's unpatched `cpu_loop.c` (compiled with the build's own command: P1 removed, nothing else); `guest-base` runs `-B 4096`; `bind-id` links a pool whose slots carry id + 1 (xtask side); the rest are atl-qemu-engine build.rs `BREAKS`.
//! Output: `<out>/qemu/<target>-linux-user-pic/spike/`: the libraries, the fixture, one log per run.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_bridgegen::aarch64;
use atl_layout::Layout;
use object::{Object, ObjectSection, ObjectSymbol};

use crate::deps;
use crate::git::git_raw;
use crate::jni_fixture::{self, NdkTarget};
use crate::qemu::{self, Build, GUEST_API};
use crate::sources;
use crate::verdict::{Failure, Verdict};

const FIXTURE_SRC: &str = "xtask/testdata/qemu-spike/fixture.c";
/// P1's call site: the one object the `no-p1` break replaces.
const CPU_LOOP: &str = "linux-user/aarch64/cpu_loop.c";
const PACKAGE: &str = "atl-qemu-engine";
const GATES: [char; 12] = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l'];
/// Thunk slots in the fixture's pool: a power of two above the x86 engine's binding count (3860 stub rows and 6 fixed, qemu-thunks-plan 2.2). Gate (l) uses slot 4095.
const POOL_SLOTS: u32 = 4096;
/// The pool's symbol and its alignment (`.p2align 12` in `aarch64::pool_asm`).
const POOL_SYMBOL: &str = "atl_bind_pool";
const POOL_ALIGN: u64 = 1 << 12;
/// Bytes per thunk: two words.
const THUNK_BYTES: u64 = 8;
const POOL_BYTES: u64 = POOL_SLOTS as u64 * THUNK_BYTES;
/// The id offset of the `bind-id` break: every slot carries `slot + POOL_SKEW`.
const POOL_SKEW: u32 = 1;
/// Per harness run. A run that outlives it hangs (the fatal path in the module doc: QEMU's `die_with_signal`, a SIGSEGV that sigchain's SIG_DFL revert does not end) and counts as failed.
const RUN_TIMEOUT_SECS: u32 = 240;
/// `timeout`'s exit status when it killed the command.
const TIMEOUT_EXIT: i32 = 124;
/// The non-identity guest base of the `guest-base` break: 1 TiB, free in an x86_64 process (PIE and heap sit near 0x55.., mmap near 0x7f..) and page aligned.
/// Not 4096: with any nonzero base QEMU also maps guest page 0 (`pgb_addr_set`), and host 4096 is below `mmap_min_addr` (65536), so QEMU refuses to start and gate (c) is never reached.
const SHIFTED_BASE: &str = "0x10000000000";

#[derive(clap::Args)]
pub struct Args {
    /// arches.toml guest row whose `qemu_target` is driven (the spike engine is aarch64-only).
    #[arg(long, default_value = qemu::DEFAULT_ARCH)]
    pub arch: String,
    /// Out dir (default: as for `xtask qemu`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Only the plain run, no breaks.
    #[arg(long)]
    pub no_breaks: bool,
}

/// One harness run: which library, guest base and engine break, and the gate it must turn red (None = all green).
struct RunSpec {
    name: &'static str,
    unpatched: bool,
    /// The fixture whose pool slots carry `slot + POOL_SKEW` (the `bind-id` break), not the production pool.
    pool_skew: bool,
    base: &'static str,
    engine_break: Option<&'static str>,
    red: Option<char>,
}

const RUNS: &[RunSpec] = &[
    RunSpec {
        name: "plain",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: None,
        red: None,
    },
    RunSpec {
        name: "no-p1",
        unpatched: true,
        pool_skew: false,
        base: "0",
        engine_break: None,
        red: Some('a'),
    },
    RunSpec {
        name: "ret-reg",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: Some("ret-reg"),
        red: Some('a'),
    },
    RunSpec {
        name: "guest-base",
        unpatched: false,
        pool_skew: false,
        base: SHIFTED_BASE,
        engine_break: None,
        red: Some('c'),
    },
    RunSpec {
        name: "fault-route",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: Some("fault-route"),
        red: Some('d'),
    },
    RunSpec {
        name: "chain-order",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: Some("chain-order"),
        red: Some('e'),
    },
    RunSpec {
        name: "drop-accerr",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: Some("drop-accerr"),
        red: Some('f'),
    },
    RunSpec {
        name: "boot-only",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: Some("boot-only"),
        red: Some('g'),
    },
    // gate (i) hangs (observed): QEMU's own threads take the kill, the faulting thread sleeps in sigsuspend; the timeout kills the run
    RunSpec {
        name: "no-fatal",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: Some("no-fatal"),
        red: Some('i'),
    },
    // gate (k) re-faults under SIG_IGN until the timeout kills it
    RunSpec {
        name: "ign-loop",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: Some("ign-loop"),
        red: Some('k'),
    },
    // gate (l): the bit-14 decode lets the bind thunk's svc reach QEMU's syscall path (getpid), so no bind trap arrives
    RunSpec {
        name: "bind-decode",
        unpatched: false,
        pool_skew: false,
        base: "0",
        engine_break: Some("bind-decode"),
        red: Some('l'),
    },
    // gate (l): every pool slot carries the next id, so the trap names the wrong binding
    RunSpec {
        name: "bind-id",
        unpatched: false,
        pool_skew: true,
        base: "0",
        engine_break: None,
        red: Some('l'),
    },
];

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let b = qemu::plan(root, &args.arch, args.out.as_deref(), true)?;
    qemu::execute(root, &b)?;
    let dir = b.dir.join("spike");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let so = link(&b, &dir.join("libqemu-spike.so"), None)?;
    let plain = fixture(root, &b, &dir, "fixture", 0)?;
    let skewed = fixture(root, &b, &dir, "fixture-skew", POOL_SKEW)?;
    let sigchain = sigchain(root, &dir)?;
    let runs: Vec<&RunSpec> = RUNS
        .iter()
        .filter(|r| !args.no_breaks || r.red.is_none())
        .collect();
    let mut fails = Vec::new();
    if let Err(e) = check_pool(&plain) {
        fails.push(Failure::step("pool", format!("{e:#}")));
    }
    let mut unpatched_so = None;
    for spec in &runs {
        let lib = if spec.unpatched {
            match &unpatched_so {
                Some(p) => p,
                None => unpatched_so.insert(unpatched(root, &b, &dir)?),
            }
        } else {
            &so
        };
        let fx = if spec.pool_skew { &skewed } else { &plain };
        if let Err(e) = harness(root, &b, &dir, spec, lib, fx, &sigchain) {
            fails.push(Failure::step(spec.name, format!("{e:#}")));
        }
    }
    let verdict = Verdict {
        cmd: "qemu-spike",
        total: Some((runs.len(), "runs")),
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

/// The executable's link command, as words (meson writes it unquoted).
fn link_words(b: &Build) -> Result<Vec<String>> {
    let out = Command::new(&b.ninja)
        .arg("-C")
        .arg(b.build_dir())
        .args(["-t", "commands", &b.exe_name()])
        .output()
        .context("ninja -t commands")?;
    ensure!(
        out.status.success(),
        "ninja -t commands {} failed",
        b.exe_name()
    );
    let text = String::from_utf8(out.stdout)?;
    let line = text.lines().last().context("no link command")?;
    ensure!(
        !line.contains('\''),
        "quoted link command, not handled: {line}"
    );
    Ok(line.split_whitespace().map(str::to_owned).collect())
}

/// Links the PIC build's objects into `so`, `replace` = (object, substitute) swapped in.
fn link(b: &Build, so: &Path, replace: Option<(&str, &Path)>) -> Result<PathBuf> {
    let words = link_words(b)?;
    let mut cmd = Command::new(&words[0]);
    let mut it = words[1..].iter();
    let mut replaced = false;
    while let Some(w) = it.next() {
        match w.as_str() {
            "-o" => {
                it.next();
                cmd.arg("-shared").arg("-o").arg(so);
            }
            // QEMU's plugin export list: a shared library exports its defaults
            "-Xlinker" => {
                let next = it.next().context("-Xlinker without argument")?;
                ensure!(
                    next.starts_with("--dynamic-list="),
                    "unexpected -Xlinker {next}"
                );
            }
            "-pie" => bail!("PIE link line: the PIC build is configured with --disable-pie"),
            o if replace.is_some_and(|(obj, _)| o == obj) => {
                cmd.arg(replace.map(|(_, p)| p).context("replace")?);
                replaced = true;
            }
            o => {
                cmd.arg(o);
            }
        }
    }
    ensure!(
        replace.is_none() || replaced,
        "{:?}: not in the link line",
        replace.map(|r| r.0)
    );
    cmd.arg("-Wl,--defsym=atl_qemu_user_main=main")
        .current_dir(b.build_dir());
    let log = so.with_extension("link.log");
    let out = cmd.output().context("linking the spike library")?;
    std::fs::write(&log, [out.stdout, out.stderr].concat())?;
    ensure!(out.status.success(), "link failed, log {}", log.display());
    Ok(so.to_owned())
}

/// The library with the pin's `cpu_loop.c` (P1 removed), compiled by the build's own command for that file.
fn unpatched(root: &Path, b: &Build, dir: &Path) -> Result<PathBuf> {
    let pin = sources::load(root)?
        .git
        .get(qemu::UPSTREAM)
        .context("no [qemu] pin")?
        .commit
        .clone();
    let text = git_raw(&b.src, &["show", &format!("{pin}:{CPU_LOOP}")])?;
    let src = dir.join("cpu_loop.unpatched.c");
    std::fs::write(&src, text)?;
    let db_path = b.build_dir().join("compile_commands.json");
    let db: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&db_path)?)?;
    let entry = db
        .as_array()
        .context("compile_commands.json: not an array")?
        .iter()
        .find(|e| e["file"].as_str().is_some_and(|f| f.ends_with(CPU_LOOP)))
        .with_context(|| format!("no {CPU_LOOP} entry"))?;
    let (Some(command), Some(file), Some(output)) = (
        entry["command"].as_str(),
        entry["file"].as_str(),
        entry["output"].as_str(),
    ) else {
        bail!("{CPU_LOOP} entry lacks command/file/output");
    };
    let obj = dir.join("cpu_loop.unpatched.o");
    let cmd = command
        .replace(output, &obj.display().to_string())
        .replace(file, &src.display().to_string());
    let log = dir.join("cpu_loop.unpatched.log");
    let out = Command::new("sh")
        .args(["-c", &cmd])
        .current_dir(b.build_dir())
        .output()
        .context("compiling the unpatched cpu_loop.c")?;
    std::fs::write(&log, [out.stdout, out.stderr].concat())?;
    ensure!(
        out.status.success(),
        "unpatched cpu_loop.c failed, log {}",
        log.display()
    );
    link(b, &dir.join("libqemu-spike-no-p1.so"), Some((output, &obj)))
}

/// The guest fixture `name` (static bionic): fixture.c plus the thunk pool, whose slot k carries binding `k + skew`. The pool is emitted text, assembled by the same NDK clang; no assembly is written by hand.
fn fixture(root: &Path, b: &Build, dir: &Path, name: &str, skew: u32) -> Result<PathBuf> {
    let ndk = jni_fixture::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    let pool_src = dir.join(format!("{name}-pool.s"));
    std::fs::write(&pool_src, aarch64::pool_asm(POOL_SLOTS, |slot| slot + skew))
        .with_context(|| format!("writing {}", pool_src.display()))?;
    let pool_obj = dir.join(format!("{name}-pool.o"));
    let status = target
        .cmd("clang")
        .args(["-c", "-o"])
        .arg(&pool_obj)
        .arg(&pool_src)
        .status()
        .context("running the NDK clang on the pool")?;
    ensure!(
        status.success(),
        "NDK clang failed on {}",
        pool_src.display()
    );
    let bin = dir.join(name);
    let src = root.join(FIXTURE_SRC);
    let status = target
        .cmd("clang")
        .args(["-static", "-O1", "-Wall", "-Werror", "-o"])
        .arg(&bin)
        .arg(&src)
        .arg(&pool_obj)
        .status()
        .context("running the NDK clang")?;
    ensure!(status.success(), "NDK clang failed on {}", src.display());
    Ok(bin)
}

/// The built fixture's pool, read back from its own symbol table: `atl_bind_pool` is page aligned and `POOL_BYTES` long, and slot k holds `aarch64::place_words(k)` (`svc #(BIND | k); ret`).
/// This checks what the assembler and linker made of the emitted text. The encodings themselves are literal in atl-bridgegen's unit tests.
fn check_pool(bin: &Path) -> Result<()> {
    let data = std::fs::read(bin).with_context(|| format!("reading {}", bin.display()))?;
    let file = object::File::parse(&*data).with_context(|| format!("parsing {}", bin.display()))?;
    let sym = file
        .symbols()
        .find(|s| s.name() == Ok(POOL_SYMBOL))
        .with_context(|| format!("{}: no {POOL_SYMBOL} in .symtab", bin.display()))?;
    ensure!(
        sym.address() % POOL_ALIGN == 0,
        "{POOL_SYMBOL} at {:#x}: not page aligned",
        sym.address()
    );
    ensure!(
        sym.size() == POOL_BYTES,
        "{POOL_SYMBOL}: {} bytes, expected {POOL_BYTES}",
        sym.size()
    );
    let bytes = file
        .sections()
        .find_map(|s| s.data_range(sym.address(), sym.size()).ok().flatten())
        .with_context(|| format!("{POOL_SYMBOL} is in no section"))?;
    let (thunks, _) = bytes.as_chunks::<{ THUNK_BYTES as usize }>();
    for (slot, thunk) in thunks.iter().enumerate() {
        let word = |at: usize| {
            u32::from_le_bytes([thunk[at], thunk[at + 1], thunk[at + 2], thunk[at + 3]])
        };
        let got = [word(0), word(4)];
        let want = aarch64::place_words(u32::try_from(slot)?);
        ensure!(got == want, "slot {slot}: {got:08x?}, expected {want:08x?}");
    }
    Ok(())
}

/// AOSP's libsigchain object (art/sigchainlib/sigchain.cc) built as-is for the host: the chain ART installs its fault handlers on. Build config only: the file uses CHAR_BIT and numeric_limits without including their headers, which bionic and libc++ supply transitively and glibc with libstdc++ does not.
fn sigchain(root: &Path, dir: &Path) -> Result<PathBuf> {
    let src = Layout::of(root)?
        .src()
        .join("aosp")
        .join("art")
        .join("sigchainlib");
    let cc = deps::binary(root, "clang")?;
    let obj = dir.join("sigchain.o");
    let status = Command::new(&cc)
        .args([
            "-x",
            "c++",
            "-std=gnu++17",
            "-O1",
            "-fPIC",
            "-Wall",
            "-Wno-unused-function",
            "-include",
            "limits.h",
            "-include",
            "limits",
            "-I",
        ])
        .arg(&src)
        .arg("-c")
        .arg(src.join("sigchain.cc"))
        .arg("-o")
        .arg(&obj)
        .status()
        .context("running clang on sigchain.cc")?;
    ensure!(
        status.success(),
        "clang failed on {}",
        src.join("sigchain.cc").display()
    );
    Ok(obj)
}

/// One harness run; Ok when it ends as `spec` says.
fn harness(
    root: &Path,
    b: &Build,
    dir: &Path,
    spec: &RunSpec,
    so: &Path,
    fixture: &Path,
    sigchain: &Path,
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
        "qemu",
        "--test",
        "spike",
        "--target-dir",
    ])
    .arg(dir.join("cargo"))
    .args(["--", "--guest-base", spec.base])
    .env("ATL_QEMU_BUILD", b.build_dir())
    .env("ATL_QEMU_SPIKE_SO", so)
    .env("ATL_QEMU_SPIKE_FIXTURE", fixture)
    .env("ATL_QEMU_SIGCHAIN_OBJ", sigchain)
    .current_dir(root);
    match spec.engine_break {
        Some(x) => cmd.env("ATL_QEMU_ENGINE_BREAK", x),
        None => cmd.env_remove("ATL_QEMU_ENGINE_BREAK"),
    };
    let out = cmd.output().context("running cargo test")?;
    let log = dir.join(format!("{}.log", spec.name));
    std::fs::write(
        &log,
        [out.stdout.as_slice(), out.stderr.as_slice()].concat(),
    )?;
    let hung = out.status.code() == Some(TIMEOUT_EXIT);
    if hung {
        println!(
            "qemu-spike: {} hung, killed after {RUN_TIMEOUT_SECS} s (log {})",
            spec.name,
            log.display()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let ok = |g: char| text.lines().any(|l| l == format!("spike: ({g}) ok"));
    match spec.red {
        None => {
            let red: Vec<char> = GATES.into_iter().filter(|g| !ok(*g)).collect();
            ensure!(
                red.is_empty() && out.status.success(),
                "gates {red:?} not ok ({}{}), log {}",
                out.status,
                if hung { ", hung" } else { "" },
                log.display()
            );
        }
        Some(g) => {
            ensure!(!ok(g), "break left gate ({g}) ok, log {}", log.display());
            ensure!(
                !out.status.success(),
                "break run exited 0, log {}",
                log.display()
            );
        }
    }
    Ok(())
}
