// SPDX-License-Identifier: MIT
//! `xtask test qemu-spike`: the QEMU engine spike (crates/atl-qemu-engine, QEMU bridge plan Q2) against its fixture, then every break.
//!
//! Library: the `--pic` build's own link line for `qemu-<target>` (`ninja -t commands`), linked `-shared` instead, `main` aliased `atl_qemu_user_main` (`--defsym`), QEMU's plugin `--dynamic-list` dropped. No QEMU file is compiled differently: this is D7 with a link-time change only.
//! Fixture: `xtask/testdata/qemu-spike/fixture.c`, static bionic by the NDK clang; QEMU's `main` loads it with `-B <guest base>`. Its binding pool (gate l) is `atl_bridgegen::aarch64::pool_asm` text, assembled by the same clang into the fixture; `check_pool` reads the pool back from the built binary.
//! Harness: `cargo test -p atl-qemu-engine --features qemu --test spike` with `ATL_QEMU_BUILD` (layouts), `ATL_QEMU_SPIKE_SO`, `ATL_QEMU_SPIKE_FIXTURE`, `ATL_QEMU_SIGCHAIN_OBJ` (AOSP libsigchain's object, built here from `art/sigchainlib/sigchain.cc` as-is); its own target dir under the spike dir, so the cfg breaks rebuild only it.
//! Pass: the plain run prints `spike: (x) ok` for every gate and exits 0; each break run leaves its gate without `ok` and exits nonzero.
//! Breaks: `no-p1` relinks with the pin's unpatched `cpu_loop.c` (compiled with the build's own command: P1 removed, nothing else); `no-access-ok` relinks with the pin's `uaccess.c` and `qemu.h` (upstream access_ok: gate (o) red); `whole-range` relinks with access_ok as one range, before the page walk (gate (p) red);`guest-base` runs `-B` [`SHIFTED_BASE`]; `bind-id` links a pool whose slots carry id + 1 (a text edit of the emitted pool, `pool_text`); the rest are atl-qemu-engine build.rs `BREAKS`.
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
const UACCESS: &str = "linux-user/uaccess.c";
const QEMU_H: &str = "linux-user/qemu.h";
const PACKAGE: &str = "atl-qemu-engine";
const GATES: [char; 16] = [
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p',
];
/// Thunk slots in the fixture's pool, as the NB pool is sized (atl-native-bridge build.rs: stub rows + fixed bindings + JNI slots, rounded to a power of two; 3860 + 6 + 256 > 4096). Gate (l) uses the last slot, 8191.
const POOL_SLOTS: u32 = 8192;
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
/// Whether `timeout` ended the command: 124 when the limit did, 137 when the `-s KILL` both harnesses pass did (timeout(1); run_apk.rs `stopped_at_limit` reads the same two).
fn killed_at_limit(code: Option<i32>) -> bool {
    matches!(code, Some(124 | 137))
}
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

/// A break that compiles one pin file in place of the build's copy. `beside`: pin files written next to it, since a quoted include resolves from the including file's directory first. `whole_range`: the `qemu.h` beside is the build's, with access_ok in its pre-page-walk form, not the pin's.
#[derive(Clone, Copy)]
struct Swap {
    file: &'static str,
    beside: &'static [&'static str],
    whole_range: bool,
}

/// P1 removed: the pin's `cpu_loop.c`.
const NO_P1: Swap = Swap {
    file: CPU_LOOP,
    beside: &[],
    whole_range: false,
};
/// access_ok as upstream: the pin's `uaccess.c`, with the pin's `qemu.h` beside it (`uaccess.c` includes it quoted).
const NO_ACCESS_OK: Swap = Swap {
    file: UACCESS,
    beside: &[QEMU_H],
    whole_range: false,
};
/// access_ok with the identity check as one range (the form before the page walk): the build's `uaccess.c` and `qemu.h`, the latter with [`WHOLE_RANGE_ACCESS_OK`] in place of `access_ok_untagged`. A range straddling a QEMU-known page and host memory fails (gate (p) red).
const WHOLE_RANGE: Swap = Swap {
    file: UACCESS,
    beside: &[QEMU_H],
    whole_range: true,
};
/// The identity check as one range: accepted when no QEMU page covers the range, else the upstream flag check. The page-walk patch replaces it; this text is the break's.
const WHOLE_RANGE_ACCESS_OK: &str = r#"static inline bool access_ok_untagged(int type, abi_ulong addr, abi_ulong size)
{
    bool locked;
    bool unmapped;

    if (size == 0
        ? !guest_addr_valid_untagged(addr)
        : !guest_range_valid_untagged(addr, size)) {
        return false;
    }
    if (guest_base == 0 && size != 0) {
        locked = have_mmap_lock();
        if (!locked) {
            mmap_lock();
        }
        unmapped = page_check_range_empty((vaddr)addr, (vaddr)addr + size - 1);
        if (!locked) {
            mmap_unlock();
        }
        if (unmapped) {
            return true;
        }
    }
    return page_check_range((target_ulong)addr, size, type);
}
"#;

/// The build's `qemu.h` with its `access_ok_untagged` replaced by [`WHOLE_RANGE_ACCESS_OK`] (the function ends at its first `\n}\n`).
fn whole_range_header(src: &Path) -> Result<String> {
    const HEAD: &str = "static inline bool access_ok_untagged(";
    let text =
        std::fs::read_to_string(src.join(QEMU_H)).with_context(|| format!("reading {QEMU_H}"))?;
    ensure!(
        text.matches(HEAD).count() == 1,
        "{QEMU_H}: {HEAD} is not present exactly once"
    );
    let start = text.find(HEAD).context("access_ok_untagged")?;
    let close = text[start..]
        .find("\n}\n")
        .with_context(|| format!("{QEMU_H}: access_ok_untagged has no closing brace"))?;
    let end = start + close + "\n}\n".len();
    Ok(format!(
        "{}{WHOLE_RANGE_ACCESS_OK}{}",
        &text[..start],
        &text[end..]
    ))
}

/// One harness run: which library, guest base and engine break, and the gate it must turn red (None = all green).
struct RunSpec {
    name: &'static str,
    swap: Option<Swap>,
    /// The fixture whose pool slots carry `slot + POOL_SKEW` (the `bind-id` break), not the production pool.
    pool_skew: bool,
    base: &'static str,
    engine_break: Option<&'static str>,
    red: Option<char>,
}

const RUNS: &[RunSpec] = &[
    RunSpec {
        name: "plain",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: None,
        red: None,
    },
    RunSpec {
        name: "no-p1",
        swap: Some(NO_P1),
        pool_skew: false,
        base: "0",
        engine_break: None,
        red: Some('a'),
    },
    RunSpec {
        name: "no-access-ok",
        swap: Some(NO_ACCESS_OK),
        pool_skew: false,
        base: "0",
        engine_break: None,
        red: Some('o'),
    },
    RunSpec {
        name: "whole-range",
        swap: Some(WHOLE_RANGE),
        pool_skew: false,
        base: "0",
        engine_break: None,
        red: Some('p'),
    },
    RunSpec {
        name: "ret-reg",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("ret-reg"),
        red: Some('a'),
    },
    RunSpec {
        name: "guest-base",
        swap: None,
        pool_skew: false,
        base: SHIFTED_BASE,
        engine_break: None,
        red: Some('c'),
    },
    RunSpec {
        name: "fault-route",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("fault-route"),
        red: Some('d'),
    },
    RunSpec {
        name: "chain-order",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("chain-order"),
        red: Some('e'),
    },
    RunSpec {
        name: "drop-accerr",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("drop-accerr"),
        red: Some('f'),
    },
    RunSpec {
        name: "boot-only",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("boot-only"),
        red: Some('g'),
    },
    // gate (i) hangs (observed): QEMU's own threads take the kill, the faulting thread sleeps in sigsuspend; the timeout kills the run
    RunSpec {
        name: "no-fatal",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("no-fatal"),
        red: Some('i'),
    },
    // gate (k) re-faults under SIG_IGN until the timeout kills it
    RunSpec {
        name: "ign-loop",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("ign-loop"),
        red: Some('k'),
    },
    // gate (l): the bit-14 decode lets the bind thunk's svc reach QEMU's syscall path (getpid), so no bind trap arrives
    RunSpec {
        name: "bind-decode",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("bind-decode"),
        red: Some('l'),
    },
    // gate (l): every pool slot carries the next id, so the trap names the wrong binding
    RunSpec {
        name: "bind-id",
        swap: None,
        pool_skew: true,
        base: "0",
        engine_break: None,
        red: Some('l'),
    },
    // gate (m): both host threads run on the boot CPU's one state
    RunSpec {
        name: "shared-cpu",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("shared-cpu"),
        red: Some('m'),
    },
    // gate (m): lock_user reads a NULL thread_cpu at the first write(2)
    RunSpec {
        name: "skip-thread-cpu",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("skip-thread-cpu"),
        red: Some('m'),
    },
    // gate (m): the attached thread's first translation runs with a NULL tcg_ctx
    RunSpec {
        name: "skip-tcg",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("skip-tcg"),
        red: Some('m'),
    },
    // gate (m): an attached thread has no return stub, so its first guest call asserts
    RunSpec {
        name: "ret-stub-local",
        swap: None,
        pool_skew: false,
        base: "0",
        engine_break: Some("ret-stub-local"),
        red: Some('m'),
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
    for spec in &runs {
        let lib = match spec.swap {
            Some(swap) => unpatched(root, &b, &dir, spec.name, swap)?,
            None => so.clone(),
        };
        let fx = if spec.pool_skew { &skewed } else { &plain };
        if let Err(e) = harness(root, &b, &dir, spec, &lib, fx, &sigchain) {
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
pub(crate) fn link(b: &Build, so: &Path, replace: Option<(&str, &Path)>) -> Result<PathBuf> {
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

/// The library with `swap.file` compiled from the pin in place of the build's copy, by the build's own command for that file. The pin's `swap.beside` files go into the same directory.
fn unpatched(root: &Path, b: &Build, dir: &Path, name: &str, swap: Swap) -> Result<PathBuf> {
    let pin = sources::load(root)?
        .git
        .get(qemu::UPSTREAM)
        .context("no [qemu] pin")?
        .commit
        .clone();
    let flat = dir.join(name);
    std::fs::create_dir_all(&flat).with_context(|| format!("creating {}", flat.display()))?;
    for rel in std::iter::once(swap.file).chain(swap.beside.iter().copied()) {
        let text = if swap.whole_range && rel == QEMU_H {
            whole_range_header(&b.src)?
        } else {
            git_raw(&b.src, &["show", &format!("{pin}:{rel}")])?
        };
        std::fs::write(flat.join(basename(rel)?), text)?;
    }
    let src = flat.join(basename(swap.file)?);
    let db_path = b.build_dir().join("compile_commands.json");
    let db: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&db_path)?)?;
    let entry = db
        .as_array()
        .context("compile_commands.json: not an array")?
        .iter()
        .find(|e| e["file"].as_str().is_some_and(|f| f.ends_with(swap.file)))
        .with_context(|| format!("no {} entry", swap.file))?;
    let (Some(command), Some(file), Some(output)) = (
        entry["command"].as_str(),
        entry["file"].as_str(),
        entry["output"].as_str(),
    ) else {
        bail!("{} entry lacks command/file/output", swap.file);
    };
    let obj = flat.join("swapped.o");
    let cmd = command
        .replace(output, &obj.display().to_string())
        .replace(file, &src.display().to_string());
    let log = dir.join(format!("{name}.log"));
    let out = Command::new("sh")
        .args(["-c", &cmd])
        .current_dir(b.build_dir())
        .output()
        .with_context(|| format!("compiling the {name} swap"))?;
    std::fs::write(&log, [out.stdout, out.stderr].concat())?;
    ensure!(
        out.status.success(),
        "{name} swap failed to compile, log {}",
        log.display()
    );
    link(
        b,
        &dir.join(format!("libqemu-spike-{name}.so")),
        Some((output, &obj)),
    )
}

fn basename(path: &str) -> Result<&str> {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{path}: no file name"))
}

/// The fixture pool's text for `skew`: slot k carries binding `k + skew`. Skew 0 is `aarch64::pool_asm` as emitted; any other skew is a text edit of it: one entry longer, its first `skew` entries (two `.inst` lines each) dropped.
fn pool_text(skew: u32) -> Result<String> {
    let text = aarch64::pool_asm(POOL_SLOTS + skew);
    if skew == 0 {
        return Ok(text);
    }
    let mut lines: Vec<&str> = text.lines().collect();
    let label = lines
        .iter()
        .position(|l| *l == "atl_bind_pool:")
        .context("pool text has no label")?;
    let first = label + 1;
    lines.drain(first..first + 2 * usize::try_from(skew)?);
    Ok(lines.join("\n") + "\n")
}

/// The guest fixture `name` (static bionic): fixture.c plus the thunk pool, whose slot k carries binding `k + skew`. The pool is emitted text, assembled by the same NDK clang; no assembly is written by hand.
fn fixture(root: &Path, b: &Build, dir: &Path, name: &str, skew: u32) -> Result<PathBuf> {
    let ndk = jni_fixture::default_ndk(root)?;
    let target = NdkTarget::new(root, &ndk, &b.arch, GUEST_API)?;
    let pool_src = dir.join(format!("{name}-pool.s"));
    std::fs::write(&pool_src, pool_text(skew)?)
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
    // `timeout -s KILL` kills the run's whole process group and exits 137 (not 124): see `killed_at_limit`
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
    let hung = killed_at_limit(out.status.code());
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

#[cfg(test)]
mod tests {
    use super::killed_at_limit;

    /// timeout(1): 124 at the limit, 137 for `-s KILL` (128 + SIGKILL 9); any other status is the command's own.
    #[test]
    fn timeout_kill_counts_as_hung() {
        assert!(killed_at_limit(Some(124)));
        assert!(killed_at_limit(Some(137)));
        assert!(!killed_at_limit(Some(0)));
        assert!(!killed_at_limit(Some(1)));
        assert!(!killed_at_limit(Some(134)));
        assert!(!killed_at_limit(None));
    }
}
