// SPDX-License-Identifier: MIT
//! Host LLVM for the in-process JIT (llvmpipe's gallivm, plan llvmpipe S1): LLVM's own CMake build of the pinned llvm-project `llvm/` project, static archives only (owner D2), for the host's `llvm_target` (arches.toml).
//! `xtask native --for llvm` builds it after ninja, under the build token, before Mesa (native.rs); `xtask test llvm` is the JIT gate. The build's compiled files pass the licence gate (`licence`). Distro libLLVM never enters an app process (owner D1).
//! Output: `<out>/upstream/llvm/<host>/build/` is the CMake build dir: `bin/llvm-config` and `bin/lli` (the tools; `LLVM_BUILD_TOOLS=OFF` leaves them out of `all`), `lib/*.a`, `include/`. Nothing is installed: the build tree is the output (Mesa's meson reads `llvm-config` from it).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};

use crate::gate_goals::Goals;
use crate::licence_gate;
use crate::verdict::{Failure, Verdict};
use crate::{arches, build_state, provided_gate, runtimes, upstream};

/// The native goal and `--for` gate name of this build (gate_goals.rs). Not an Android.bp module: `native` keeps it out of the converter's goals.
pub const GOAL: &str = "llvm";

/// What `llvm-config --version` prints at the pin: MAJOR.MINOR.PATCH 20.0.0 plus the release branch's suffix `git` (observed 2026-11-07; the branch is a snapshot, not the 20.0.0 tag).
pub const VERSION: &str = "20.0.0git";

/// Built after `all`, which leaves tools out under `LLVM_BUILD_TOOLS=OFF`: `llvm-config` (Mesa's meson, S2) and `lli` (the JIT gate).
const TOOLS: &[&str] = &["llvm-config", "lli"];

/// The JIT gate's program returns `EXPECTED`; `--break` makes it return `BREAK_RETURN` and the gate must fail.
const EXPECTED: i32 = 42;
const BREAK_RETURN: i32 = 43;

/// CMake `-D` entries besides the compilers and target. Why each:
/// - static archives only (D2): `BUILD_SHARED_LIBS=OFF`, no `libLLVM.so` to stage beside `libgallium_dri`;
/// - libc++, no RTTI, no EH: the device's C++ flags (`-fno-rtti -fno-exceptions`, distro libc++ 19, as the device compile);
/// - no zlib, zstd, libxml2, libedit, curl: the distro libLLVM pulls them in, and the archives must not NEED them;
/// - tests, examples, benchmarks, docs, utils and runtimes off: not built here; utils and runtimes are outside the keep-set (sources.toml), and `LLVM_INCLUDE_RUNTIMES` would build `llvm/runtimes`, a stub.
const OPTIONS: &[(&str, &str)] = &[
    ("BUILD_SHARED_LIBS", "OFF"),
    ("LLVM_BUILD_LLVM_DYLIB", "OFF"),
    ("LLVM_LINK_LLVM_DYLIB", "OFF"),
    ("LLVM_BUILD_TOOLS", "OFF"),
    ("LLVM_INCLUDE_UTILS", "OFF"),
    ("LLVM_INCLUDE_RUNTIMES", "OFF"),
    ("LLVM_INCLUDE_TESTS", "OFF"),
    ("LLVM_INCLUDE_EXAMPLES", "OFF"),
    ("LLVM_INCLUDE_BENCHMARKS", "OFF"),
    ("LLVM_INCLUDE_DOCS", "OFF"),
    ("LLVM_ENABLE_LIBCXX", "ON"),
    ("LLVM_ENABLE_RTTI", "OFF"),
    ("LLVM_ENABLE_EH", "OFF"),
    ("LLVM_ENABLE_PIC", "ON"),
    ("LLVM_ABI_BREAKING_CHECKS", "FORCE_OFF"),
    ("LLVM_ENABLE_ASSERTIONS", "OFF"),
    ("LLVM_ENABLE_ZLIB", "OFF"),
    ("LLVM_ENABLE_ZSTD", "OFF"),
    ("LLVM_ENABLE_LIBXML2", "OFF"),
    ("LLVM_ENABLE_LIBEDIT", "OFF"),
    ("LLVM_ENABLE_CURL", "OFF"),
    ("LLVM_ENABLE_HTTPLIB", "OFF"),
    // config-ix.cmake probes `__unw_add_dynamic_fde` with the default link (libgcc_s: absent), so the JIT registers each section libgcc's way (one pointer to the whole .eh_frame, a CIE first).
    // The process's unwinder is libunwind.so.1 (libc++abi NEEDs it), which `__register_frame` binds to and which takes one FDE: "bad fde: FDE is really a CIE" per registered section.
    // Forced on: per-FDE registration, which libunwind takes and libgcc takes too (measured: lli, S2).
    ("HAVE_UNW_ADD_DYNAMIC_FDE", "1"),
];

/// This build as a native goal: `native --for llvm` builds it and records it; the gate `llvm` reads the record.
pub fn goals() -> Goals {
    Goals::host(&[GOAL])
}

/// `<out>/upstream/llvm/<host>/`: holds `build/`, the CMake build dir.
pub fn dir(out: &Path, host: &str) -> PathBuf {
    out.join("upstream").join("llvm").join(host)
}

/// The host's `llvm-config` in the build dir: the one Mesa's meson is configured with (mesa.rs `native_file`).
pub fn config(out: &Path, host: &str) -> PathBuf {
    tool(&dir(out, host).join("build"), "llvm-config")
}

/// The `llvm_target` of the `[<host>.host]` row: the LLVM target this host builds and JITs for.
fn llvm_target(root: &Path, host: &str) -> Result<String> {
    arches::load(root)?
        .get(host)
        .and_then(|a| a.host.as_ref())
        .and_then(|h| h.llvm_target.clone())
        .with_context(|| {
            format!(
                "{host}: no llvm_target in [{host}.host] of {}",
                arches::FILE
            )
        })
}

/// Configures and builds (`all`, then `TOOLS`) the host LLVM into `dir(out, host)/build`, with compilers `cc` / `cxx`. All output goes to `build.log`.
pub fn build(root: &Path, out: &Path, host: &str, cc: &str, cxx: &str) -> Result<()> {
    let target = llvm_target(root, host)?;
    let (_, src) = runtimes::llvm_checkout(root)?;
    let project = src.join("llvm");
    ensure!(
        project.join("CMakeLists.txt").is_file(),
        "{}: no llvm/ project: run `cargo xtask fetch --execute`",
        src.display()
    );
    let base = dir(out, host);
    std::fs::create_dir_all(&base)?;
    let mut cache = vec![
        ("CMAKE_BUILD_TYPE", "Release"),
        ("CMAKE_C_COMPILER", cc),
        ("CMAKE_CXX_COMPILER", cxx),
        ("LLVM_TARGETS_TO_BUILD", target.as_str()),
    ];
    cache.extend_from_slice(OPTIONS);
    let log = base.join("build.log");
    upstream::cmake(&project, &base.join("build"), &cache, &log, TOOLS, false)
        .with_context(|| format!("host LLVM {target}, log {}", log.display()))
}

/// The licence gate over every file the host build compiled (`check_build`, reaching all of them: the archives are not linked into a shipped artifact yet). True when all pass.
pub fn licence(root: &Path, out: &Path, host: &str) -> Result<bool> {
    let build =
        std::fs::canonicalize(dir(out, host).join("build")).context("host LLVM build dir")?;
    licence_gate::gate_builds(
        root,
        "llvm",
        &[licence_gate::Build {
            name: host.to_owned(),
            dir: build,
            staged: Vec::new(),
        }],
    )
}

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): its LLVM is `upstream/llvm/<host>` in the build's out dir.
    #[arg(long)]
    host: String,
    /// Native build directory that `native --for llvm` built (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Deliberate break: the program returns 43 while the gate expects 42, so the gate must go red.
    #[arg(long = "break")]
    deliberate_break: bool,
}

/// The JIT gate: `llvm-config` reports LLVM 20.0.0git with the host's target built, neither `llvm-config` nor `lli` NEEDs libstdc++, and `lli -jit-kind=mcjit` on `ret i32 42` exits 42.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = build_state::gate_out(root, args.out.as_deref(), &args.host, GOAL)?;
    let base = dir(&out, &args.host);
    let build = base.join("build");
    let target = llvm_target(root, &args.host)?;
    let config = tool(&build, "llvm-config");
    let mut failed = Vec::new();
    let version = version_of(&config)?;
    println!("llvm: llvm-config --version {version}");
    if version != VERSION {
        failed.push(Failure::step(
            "version",
            format!("llvm-config --version is {version}, expected {VERSION}"),
        ));
    }
    let targets = stdout_of(Command::new(&config).arg("--targets-built"))?;
    println!("llvm: llvm-config --targets-built {targets}");
    let built: Vec<&str> = targets.split_whitespace().collect();
    if built != [target.as_str()] {
        failed.push(Failure::step(
            "targets",
            format!("llvm-config --targets-built is {targets:?}, expected {target}"),
        ));
    }
    let jit = base.join("jit-gate");
    std::fs::create_dir_all(&jit)?;
    let program = jit.join("main.ll");
    let returns = if args.deliberate_break {
        BREAK_RETURN
    } else {
        EXPECTED
    };
    std::fs::write(&program, ir(returns))?;
    let ran = Command::new(tool(&build, "lli"))
        .arg("-jit-kind=mcjit")
        .arg(&program)
        .output()
        .context("running lli")?;
    let got = ran.status.code();
    if got == Some(EXPECTED) {
        println!("llvm: lli -jit-kind=mcjit main.ll returned {EXPECTED}");
    } else {
        failed.push(Failure::step(
            "jit",
            format!(
                "lli -jit-kind=mcjit returned {got:?}, expected {EXPECTED}; stderr: {}",
                String::from_utf8_lossy(&ran.stderr).trim()
            ),
        ));
    }
    for name in ["llvm-config", "lli"] {
        let needed = provided_gate::needed(&tool(&build, name))?;
        if needed.iter().any(|n| n.starts_with("libstdc++")) {
            failed.push(Failure::step(
                "libstdc++",
                format!("{name} NEEDs libstdc++"),
            ));
        }
    }
    let verdict = Verdict {
        cmd: "llvm",
        total: None,
        skipped: Vec::new(),
        failures: failed,
    };
    println!("{}", verdict.line());
    Ok(if verdict.failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `bin/<name>` of the CMake build dir.
fn tool(build: &Path, name: &str) -> PathBuf {
    build.join("bin").join(name)
}

/// The IR the JIT gate runs: `main` returns `code`.
fn ir(code: i32) -> String {
    format!("define i32 @main() {{\n  ret i32 {code}\n}}\n")
}

/// `llvm-config --version` of `config`.
pub fn version_of(config: &Path) -> Result<String> {
    stdout_of(Command::new(config).arg("--version"))
}

/// Refuses a `llvm-config` that is not the pinned LLVM: Mesa is configured only against the host LLVM this module builds (mesa.rs `configure`).
pub fn check_config(config: &Path) -> Result<()> {
    let version = version_of(config)?;
    ensure!(
        version == VERSION,
        "{} reports LLVM {version}, expected {VERSION}",
        config.display()
    );
    Ok(())
}

/// Whether `config`'s LLVM has RTTI (`llvm-config --has-rtti`, YES/NO). Mesa's meson checks its `cpp_rtti` against it (mesa.rs `native_file`).
pub fn has_rtti(config: &Path) -> Result<bool> {
    match stdout_of(Command::new(config).arg("--has-rtti"))?.as_str() {
        "YES" => Ok(true),
        "NO" => Ok(false),
        other => bail!("{} --has-rtti printed {other:?}", config.display()),
    }
}

/// Trimmed stdout of a command that must succeed; otherwise its exit status and stderr.
fn stdout_of(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().with_context(|| format!("running {cmd:?}"))?;
    ensure!(
        out.status.success(),
        "{cmd:?} exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ir_returns_its_code() {
        assert_eq!(ir(42), "define i32 @main() {\n  ret i32 42\n}\n");
    }
}
