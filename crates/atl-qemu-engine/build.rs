// SPDX-License-Identifier: MIT
//! Always: `OUT_DIR/aarch64_constants.rs`, atl-bridgegen's aarch64 thunk constants (`lib.rs` `thunk`).
//! With feature `qemu`: `OUT_DIR/qemu_layout.rs`, atl-bridgegen's `layout_module` of QEMU's `target/arm/cpu.h` (`CPUArchState` and the records it holds), parsed with the exact flags the PIC build compiles `linux-user/aarch64/cpu_loop.c` with -- the file P1's hook is called from.
//! With feature `qemu`: `OUT_DIR/engine_abi_layout.rs`, the engine C ABI's sizes, offsets and values from atl-engine-api's `atl_engine.h` (atl-bridgegen `engine_abi`); lib.rs checks the Rust mirror against them.
//! Flags and the source tree come from `$ATL_QEMU_BUILD/compile_commands.json` (meson's), so the offsets are clang's for the pinned, patched headers and that build's generated config headers; nothing is hand-copied.
//! Breaks (`ATL_QEMU_ENGINE_BREAK=<name>[,<name>]`, [`BREAKS`]) turn a cfg on for this package only.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript;
use atl_bridgegen::{Lang, Source, Target};

/// The PIC build dir (`<out>/qemu/<target>-linux-user-pic/build`); `xtask test qemu-spike` sets it.
const BUILD_ENV: &str = "ATL_QEMU_BUILD";
/// The translation unit whose compile command is reused: P1's call site.
const UNIT: &str = "linux-user/aarch64/cpu_loop.c";
/// Headers whose records form the interface, relative to the QEMU tree.
const HEADERS: &[&str] = &["target/arm/cpu.h"];
/// Headers the unit also includes for [`FIELD_OFFSETS`] only; their records are not part of the interface. linux-user/qemu.h: `TaskState`; exec/translation-block.h: `CF_PARALLEL`.
const OFFSET_HEADERS: &[&str] = &["linux-user/qemu.h", "exec/translation-block.h"];
/// Offsets, sizes and flag values the engine reads, evaluated by clang in the unit: (macro name, C integer expression).
/// TPIDR_EL0 = `cp15.tpidr_el[0]`, the register linux-user's `cpu_set_tls` writes (linux-user/aarch64/target_cpu.h).
/// The rest is what a host-attached CPU needs (attach.rs), as linux-user's clone path builds a thread (syscall.c `do_fork`): `env_cpu` = env - sizeof(CPUState) (include/exec/cpu-common.h), `CPUState.opaque` = the TaskState, the TaskState fields `do_fork` copies from the parent, GCS state, and `CF_PARALLEL` (`begin_parallel_context`, linux-user/user-internals.h).
const FIELD_OFFSETS: &[(&str, &str)] = &[
    (
        "ATL_OFFSET_TPIDR_EL0",
        "offsetof(CPUARMState, cp15.tpidr_el[0])",
    ),
    // v<i>'s low lane is `vfp.zregs[i].d[0]` (target/arm/cpu.h:675, vector-type.h:35); the stride is one ARMVectorReg (Q6-1)
    ("ATL_OFFSET_ZREGS", "offsetof(CPUARMState, vfp.zregs)"),
    ("ATL_SIZEOF_ZREG", "sizeof(ARMVectorReg)"),
    ("ATL_OFFSET_ZREG_D", "offsetof(ARMVectorReg, d)"),
    ("ATL_SIZEOF_CPUSTATE", "sizeof(CPUState)"),
    ("ATL_OFFSET_CPU_OPAQUE", "offsetof(CPUState, opaque)"),
    ("ATL_SIZEOF_TASKSTATE", "sizeof(TaskState)"),
    ("ATL_OFFSET_TS_INFO", "offsetof(TaskState, info)"),
    ("ATL_OFFSET_TS_BPRM", "offsetof(TaskState, bprm)"),
    (
        "ATL_OFFSET_TS_SIGNAL_MASK",
        "offsetof(TaskState, signal_mask)",
    ),
    (
        "ATL_SIZEOF_TS_SIGNAL_MASK",
        "sizeof(((TaskState *)0)->signal_mask)",
    ),
    (
        "ATL_OFFSET_TS_GCS_EL0_LOCKED",
        "offsetof(TaskState, gcs_el0_locked)",
    ),
    (
        "ATL_OFFSET_GCSPR_EL0",
        "offsetof(CPUARMState, cp15.gcspr_el[0])",
    ),
    (
        "ATL_OFFSET_GCSCR_EL0",
        "offsetof(CPUARMState, cp15.gcscr_el[0])",
    ),
    ("ATL_GCSCR_PCRSEL", "GCSCR_PCRSEL"),
    ("ATL_CF_PARALLEL", "CF_PARALLEL"),
];

const BREAK_ENV: &str = "ATL_QEMU_ENGINE_BREAK";
/// AOSP libsigchain's object (art/sigchainlib/sigchain.cc, built as-is by `xtask test qemu-spike`); the spike harness links it.
const SIGCHAIN_ENV: &str = "ATL_QEMU_SIGCHAIN_OBJ";
/// The deliberate breaks: name -> cfg. Red with it, green without (xtask qemu_spike.rs runs each).
const BREAKS: &[(&str, &str)] = &[
    // gate (a): the CALL trap answers the callback's result with bit 0 flipped
    ("ret-reg", "atl_qe_ret_reg"),
    // gate (d): a fault in TCG code goes to the pre-QEMU handler instead of QEMU's
    ("fault-route", "atl_qe_fault_route"),
    // gate (e): a host-only fault goes to QEMU's handler (which aborts) instead of the pre-QEMU one
    ("chain-order", "atl_qe_chain_order"),
    // gate (f): a host write to a write-protected translated-code page goes to the previous handler, not to QEMU's (which un-protects it)
    ("drop-accerr", "atl_qe_drop_accerr"),
    // gate (g): every thread classifies its faults with the BOOT thread's helper_retaddr
    ("boot-only", "atl_qe_boot_only"),
    // gates (i), (j): no fatal special handler, so QEMU's die_with_signal kill is consumed by sigchain's SIG_DFL revert (exit 1 / hang)
    ("no-fatal", "atl_qe_no_fatal"),
    // gate (k): a host fault with previous action SIG_IGN keeps re-running under sigchain's SIG_IGN return
    ("ign-loop", "atl_qe_ign_loop"),
    // gate (l): the bind decode tests bit 14, so a binding thunk (bit 15) reaches QEMU's syscall path
    ("bind-decode", "atl_qe_bind_decode"),
    // gate (m): attach hands every host thread the boot CPU instead of a `cpu_copy`
    ("shared-cpu", "atl_qe_shared_cpu"),
    // gate (m): attach leaves `thread_cpu` NULL, which `lock_user` (linux-user/uaccess.c) reads on every syscall buffer
    ("skip-thread-cpu", "atl_qe_skip_thread_cpu"),
    // gate (m): attach skips `tcg_register_thread`, so the thread's `tcg_ctx` stays NULL for its first translation
    ("skip-tcg", "atl_qe_skip_tcg"),
    // gate (m): the `svc #RET` stub address stays thread-local, set only on the BOOT thread
    ("ret-stub-local", "atl_qe_ret_stub_local"),
    // gate (q, Q5-1a): the engine table says major + 1: the host refuses the library
    ("abi-major-next", "atl_qe_abi_major_next"),
    // gate (q, D9): the boot env is published after BOOT's callback, so map and attach inside BOOT fail
    ("boot-late", "atl_qe_boot_late"),
    // gate (q, D9): the creator returns before BOOT (no wait on the boot helper)
    ("no-boot-join", "atl_qe_no_boot_join"),
    // gate (r): attach skips the copy's queued work (its plugin init hook), so detach's exit hook asserts (plugins/core.c:293)
    ("no-init-drain", "atl_qe_no_init_drain"),
    // gate (qemu-abi, d): call_ex writes no stack words, so the callee's 9th int and 9th double read whatever lies at sp
    ("call-stack", "atl_qe_call_stack"),
    // gate (qemu-abi, d): call_ex sets no FP lane, so the callee's double arguments read the CPU's own v0..v7
    ("call-v", "atl_qe_call_v"),
    // gate (s): every attached CPU gets the first CPU's guest stack, so concurrent guest frames overwrite each other
    ("shared-stack", "atl_qe_shared_stack"),
    // gate (t): the guard page below each guest stack is RW and VALID, so a store below the stack succeeds
    ("no-guard", "atl_qe_no_guard"),
    // gate (qemu-abi, z): attach_ex drops the requested stack size, every stack gets the 1 MiB default
    ("ignore-size", "atl_qe_ignore_size"),
    // gate (u): attach copies the oldest live attached CPU (a running parent) instead of the parked boot CPU, keeping its sp: the child's frames land on the parent's stack
    ("copy-caller", "atl_qe_copy_caller"),
];

fn selected_breaks() -> Result<Vec<&'static str>> {
    let Ok(value) = std::env::var(BREAK_ENV) else {
        return Ok(Vec::new());
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|name| {
            BREAKS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|&(_, cfg)| cfg)
                .with_context(|| {
                    let known: Vec<&str> = BREAKS.iter().map(|&(n, _)| n).collect();
                    format!(
                        "{BREAK_ENV}={name}: no such break (known: {})",
                        known.join(", ")
                    )
                })
        })
        .collect()
}

/// Splits a meson compile command: words by spaces, `'...'` quoted (meson quotes only `-D` values holding `"`).
fn words(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in cmd.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                any = true;
            }
            ' ' if !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            _ => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// The preprocessor arguments of `cmd` (include dirs absolute against `dir`, defines, the C standard); codegen and output flags are dropped.
fn preprocessor_args(cmd: &[String], dir: &Path) -> Vec<String> {
    let abs = |p: &str| dir.join(p).display().to_string();
    let mut out = Vec::new();
    let mut it = cmd.iter();
    while let Some(a) = it.next() {
        if let Some(p) = a.strip_prefix("-I") {
            out.push(format!("-I{}", abs(p)));
        } else if a == "-iquote" || a == "-isystem" {
            if let Some(p) = it.next() {
                out.extend([a.clone(), abs(p)]);
            }
        } else if let Some(p) = a.strip_prefix("-isystem") {
            out.extend(["-isystem".to_owned(), abs(p)]);
        } else if a.starts_with("-D") || a.starts_with("-U") || a.starts_with("-std=") {
            out.push(a.clone());
        }
    }
    out
}

fn layout(build: &Path, out: &Path, host: &str) -> Result<String> {
    let db_path = build.join("compile_commands.json");
    println!("cargo::rerun-if-changed={}", db_path.display());
    let db: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&db_path).with_context(|| db_path.display().to_string())?,
    )?;
    let entry = db
        .as_array()
        .context("compile_commands.json: not an array")?
        .iter()
        .find(|e| e["file"].as_str().is_some_and(|f| f.ends_with(UNIT)))
        .with_context(|| format!("{}: no entry for {UNIT}", db_path.display()))?;
    let dir = PathBuf::from(entry["directory"].as_str().context("entry: no directory")?);
    let file = dir.join(entry["file"].as_str().context("entry: no file")?);
    let src = file
        .to_str()
        .and_then(|f| f.strip_suffix(UNIT))
        .map(PathBuf::from)
        .context("entry file does not end with the unit")?;
    let cmd = words(entry["command"].as_str().context("entry: no command")?);
    let args = preprocessor_args(&cmd, &dir);
    ensure!(
        args.iter().any(|a| a.starts_with("-DCONFIG_TARGET=")),
        "{UNIT}: compile command defines no CONFIG_TARGET"
    );
    let also: Vec<PathBuf> = HEADERS.iter().map(|h| src.join(h)).collect();
    let offset_headers: Vec<PathBuf> = OFFSET_HEADERS.iter().map(|h| src.join(h)).collect();
    for h in also.iter().chain(&offset_headers) {
        println!("cargo::rerun-if-changed={}", h.display());
    }
    let unit = out.join("qemu_cpu.c");
    let mut text = String::from("#include \"qemu/osdep.h\"\n");
    for h in HEADERS.iter().chain(OFFSET_HEADERS) {
        text.push_str(&format!("#include \"{h}\"\n"));
    }
    // fields inside anonymous unions get generated names (`cp15.anon13`) that renumber when cpu.h gains a union: name the path instead, clang evaluates it
    for (name, expr) in FIELD_OFFSETS {
        text.push_str(&format!("#define {name} {expr}\n"));
    }
    std::fs::write(&unit, text)?;
    let macros: Vec<&str> = FIELD_OFFSETS.iter().map(|&(n, _)| n).collect();
    let source = Source {
        path: &unit,
        lang: Lang::C,
        also: &also,
        macros: &macros,
    };
    // the host view: the library runs in the host process, compiled by the PIC build's host compiler
    let target = Target {
        arch: host.to_owned(),
        triple: buildscript::clang_triple_from_cargo()?,
        args,
    };
    let iface = atl_bridgegen::interface(
        "qemu_cpu",
        "qemu target/arm/cpu.h (aarch64-linux-user, PIC build flags)",
        &source,
        std::slice::from_ref(&target),
    )
    .context("QEMU CPU layouts")?;
    atl_bridgegen::rust::layout_module(&iface, host)
}

fn main() -> Result<()> {
    for &(_, cfg) in BREAKS {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
    println!("cargo::rerun-if-env-changed={BREAK_ENV}");
    println!("cargo::rerun-if-env-changed={BUILD_ENV}");
    println!("cargo::rerun-if-env-changed={SIGCHAIN_ENV}");
    println!("cargo::rustc-check-cfg=cfg(atl_qe_sigchain)");
    for cfg in selected_breaks()? {
        println!("cargo::rustc-cfg={cfg}");
    }
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    // the thunk ABI's constants (BIND, the trap decode's bit): atl-bridgegen's emitter is the one source, so they cannot drift from the pool
    std::fs::write(
        out.join("aarch64_constants.rs"),
        atl_bridgegen::aarch64::constants_rust(),
    )?;
    if std::env::var_os("CARGO_FEATURE_QEMU").is_none() {
        return Ok(());
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let arches = buildscript::checkout(&manifest)?.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let build = PathBuf::from(std::env::var(BUILD_ENV).with_context(|| {
        format!("feature qemu needs {BUILD_ENV} (`xtask qemu --pic` build dir)")
    })?);
    std::fs::write(
        out.join("qemu_layout.rs"),
        layout(&build, &out, &host.arch)?,
    )?;
    // the engine C ABI's facts, read from the header by clang; lib.rs checks atl-engine-api's Rust mirror against them
    let header = manifest.join("../atl-engine-api/include/atl_engine.h");
    println!("cargo::rerun-if-changed={}", header.display());
    std::fs::write(
        out.join("engine_abi_layout.rs"),
        atl_bridgegen::engine_abi::layout_module(&header, &out, &host.arch)?,
    )?;
    // the spike harness is an executable: P1's call in the dlopened QEMU library binds to this strong definition only if it is in the dynamic symbol table
    println!("cargo::rustc-link-arg-tests=-Wl,--export-dynamic-symbol=atl_svc_hook");
    // ART's sigchain, linked into the harness: its `sigaction` / `signal` / `sigprocmask` are this executable's, and QEMU's library binds to them only if they are exported
    if let Some(obj) = std::env::var_os(SIGCHAIN_ENV) {
        println!("cargo::rustc-cfg=atl_qe_sigchain");
        println!(
            "cargo::rustc-link-arg-tests={}",
            PathBuf::from(obj).display()
        );
        println!("cargo::rustc-link-arg-tests=-lstdc++");
        for sym in ["sigaction", "signal", "sigprocmask"] {
            println!("cargo::rustc-link-arg-tests=-Wl,--export-dynamic-symbol={sym}");
        }
    }
    Ok(())
}
