// SPDX-License-Identifier: MIT
//! `xtask test ui-module` (launcher-ui-module plan slice M1): GTK, GLib and PipeWire loaded as an `RTLD_LOCAL` module stay out of the process's global symbol scope, while the module still drives a window from the process's Looper and logs through the process's subscriber.
//! The process is `atl-platform-ui-module-fixture` (linked as the launcher is: libsigchain leads DT_NEEDED; built by `xtask native`). It finds `libatl_platform_ui_linux.so` by file name through LD_LIBRARY_PATH (staged here like the other runtime cdylibs), opens one colour window in the gate's headless labwc, connects the module's audio host to a private PipeWire server, then dlopens an app library. The app (NDK-built stand-in) imports `vkCreateInstance` from `libvulkan.so`; the AOSP stand-in of that library returns [`AOSP_VALUE`], a value no Vulkan loader returns.
//! Gates (normal run, `LD_DEBUG=bindings`):
//! 1. painted >= 1: the module's window painted a scene through the process's Looper.
//! 2. the app's `vkCreateInstance` returned [`AOSP_VALUE`]: its import bound the AOSP stand-in, not the host's libvulkan.so.1 that GTK loads.
//! 3. the module's `module attached` record arrived in the process's subscriber (`MODULE` line).
//! 4. bindings: every symbol of an app object (the stand-in dir) bound inside that dir or to glibc; `dlsym(RTLD_DEFAULT)` finds none of the fixture's host symbols (`global_leaks=0`), after GTK init, a paint, and the PipeWire connect that loads SPA plugins.
//! 5. the fixture's `sigaction` comes from libsigchain (the launcher's link shape holds).
//!
//! Controls (each must fail the gate it targets): the module loaded `RTLD_GLOBAL` -> the app gets something other than [`AOSP_VALUE`] and leaks > 0; no log sink -> no `MODULE` line.
//! Evidence tier: isolated runtime with the launcher's link shape (sigchain, glibc ld.so, the converter's runtime library path); ART, bionic's linker namespaces and a real app are the M3/V0 gates.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::audio_pcm::PrivateServer;
use crate::host_art;
use crate::ndk::NdkTarget;
use crate::nested_wayland::NestedCompositor;

/// The stand-in's `vkCreateInstance` result (testdata/ui-module/aosp_vulkan.c): not a VkResult, so no Vulkan loader returns it.
const AOSP_VALUE: i64 = 4242;
/// Sources of the app and the AOSP stand-in.
const SOURCES: &str = "xtask-harness/testdata/ui-module";
/// The fixture executable, in the converter packages' target dir (`packages::TARGET_DIR`).
const FIXTURE: &str = "atl-platform-ui-module-fixture";
/// The module's package and its file (atl-platform-ui-api `abi::MODULE_FILE`; the harness does not link the api crate).
const MODULE_PACKAGE: &str = "atl-platform-ui-linux";
const MODULE_FILE: &str = "libatl_platform_ui_linux.so";
/// NDK API level of the stand-ins: the level the plan's probe measured with (NDK r30, API 33).
const API: u32 = 33;
/// The record the module logs once attached (atl-platform-ui-linux module.rs `ui_attach`).
const ATTACHED: &str = "module attached";
/// The target the facade gives every forwarded module record (atl-platform-ui module.rs `forward`), as the fixture's subscriber prints it.
const MODULE_TARGET: &str = "atl_platform_ui_linux:";
/// glibc's objects: app objects may bind to them (`__cxa_finalize`), they are in every scope.
const GLIBC: &[&str] = &["/libc.so.6", "/ld-linux-x86-64.so.2"];

#[derive(clap::Args)]
pub struct Args {
    /// The native build dir (default: this checkout's slot).
    #[arg(long)]
    out: Option<PathBuf>,
    /// NDK root (default: the workspace's tools dir).
    #[arg(long)]
    ndk: Option<PathBuf>,
    /// Keep the run's files here instead of `<out>/ui-module`.
    #[arg(long)]
    keep: Option<PathBuf>,
}

/// One fixture run's `RESULT key=value` lines and whether any `MODULE` line arrived.
struct Run {
    text: String,
    results: Vec<(String, String)>,
    module_lines: usize,
}

impl Run {
    fn get(&self, key: &str) -> Option<&str> {
        self.results
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.parse().ok())
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), "x86_64", "ui-module")?;
    let fixture = out
        .join(crate::packages::TARGET_DIR)
        .join("release")
        .join(FIXTURE);
    ensure!(
        fixture.is_file(),
        "missing {} (built by `cargo xtask native`)",
        fixture.display()
    );
    let work = args.keep.clone().unwrap_or_else(|| out.join("ui-module"));
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    std::fs::create_dir_all(&work)?;
    let module_dir = stage_module(root, &out)?;
    let app = build_app(root, args, &work.join("app"))?;
    let app_dir = app.parent().context("app dir")?.to_path_buf();
    let nested = NestedCompositor::start(&work)?;
    let runtime = work.join("pw");
    std::fs::create_dir_all(&runtime)?;
    let _server = PrivateServer::start(&runtime, &work)?;
    let mut lib_path = vec![
        app_dir.display().to_string(),
        module_dir.display().to_string(),
    ];
    for (k, v) in crate::stage_env::lib_env(&out)? {
        ensure!(k == "LD_LIBRARY_PATH", "unexpected stage variable {k}");
        lib_path.push(v);
    }
    let go_with = |name: &str, extra: &[&str], module_dir: Option<&Path>| -> Result<Run> {
        let mut lib_path = lib_path.clone();
        match module_dir {
            Some(d) => lib_path[1] = d.display().to_string(),
            None => {
                lib_path.remove(1);
            }
        }
        let ld = work.join(format!("ld-{name}"));
        let mut cmd = Command::new(&fixture);
        cmd.arg("--app")
            .arg(&app)
            .arg("--audio")
            .arg(runtime.join("pipewire-0"))
            .args(extra)
            .env("LD_LIBRARY_PATH", lib_path.join(":"))
            .env("WAYLAND_DISPLAY", &nested.socket)
            .env("GDK_BACKEND", "wayland")
            .env("GSK_RENDERER", "ngl")
            .env("GTK_A11Y", "none")
            .env("LD_DEBUG", "bindings")
            .env("LD_DEBUG_OUTPUT", &ld);
        let o = cmd
            .output()
            .with_context(|| format!("running {}", fixture.display()))?;
        let text =
            String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
        std::fs::write(work.join(format!("{name}.log")), &text)?;
        ensure!(
            o.status.success(),
            "fixture run {name}: {}\n{text}",
            o.status
        );
        let results = text
            .lines()
            .filter_map(|l| {
                l.split_once("RESULT ")
                    .and_then(|(_, kv)| kv.split_once('='))
            })
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let module_lines = text.lines().filter(|l| l.contains(MODULE_TARGET)).count();
        Ok(Run {
            text,
            results,
            module_lines,
        })
    };
    let go = |name: &str, extra: &[&str]| go_with(name, extra, Some(&module_dir));
    let normal = go("local", &[])?;
    let global = go("global", &["--global"])?;
    let silent = go("no-module-records", &["--drop-module-records"])?;
    let missing = go_with("no-module", &[], None)?;
    let foreign_dir = foreign_build(
        &module_dir.join(MODULE_FILE),
        &normal,
        &work.join("abi-bad"),
    )?;
    let foreign = go_with("abi-bad", &[], Some(&foreign_dir))?;

    let mut failures: Vec<String> = Vec::new();
    let mut check = |ok: bool, what: String| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failures.push(what);
        }
    };
    check(
        normal.int("painted").is_some_and(|p| p >= 1),
        format!("painted >= 1 (got {:?})", normal.get("painted")),
    );
    check(
        normal.int("app_vkCreateInstance") == Some(AOSP_VALUE),
        format!(
            "app vkCreateInstance -> {AOSP_VALUE} (got {:?})",
            normal.get("app_vkCreateInstance")
        ),
    );
    check(
        normal
            .text
            .lines()
            .any(|l| l.contains(MODULE_TARGET) && l.contains(ATTACHED)),
        format!(
            "module record `{ATTACHED}` reached the process subscriber ({} module lines)",
            normal.module_lines
        ),
    );
    let leaked = foreign_bindings(&work.join("ld-local"), &app_dir)?;
    check(
        leaked.is_empty(),
        format!(
            "app objects bind only inside their dir or glibc ({} foreign: {})",
            leaked.len(),
            leaked.first().map_or("", String::as_str)
        ),
    );
    check(
        normal.int("global_leaks") == Some(0),
        format!(
            "the module put no host symbol into global scope (got {:?} {:?})",
            normal.get("global_leaks"),
            normal.get("global_leak_names")
        ),
    );
    // measured, not gated (an owner decision, state note ui-module-m1): RTLD_LOCAL keeps the module's symbols out of global scope, but its own objects still look global scope up first, so a host library inside the module binds an AOSP library the process already has (libsigchain -> libunwindstack -> AOSP libz.so: host libz.so.1, libpng16, libfreetype bind its `inflate`)
    let reverse = reverse_bindings(&work.join("ld-local"), &out.join("device"))?;
    println!(
        "info reverse bindings, host object -> AOSP object in global scope: {} (first: {})",
        reverse.len(),
        reverse.first().map_or("", String::as_str)
    );
    check(
        normal.get("audio_connect") == Some("1"),
        format!(
            "module audio host connected, SPA plugins loaded locally (got {:?})",
            normal.get("audio_connect")
        ),
    );
    check(
        normal
            .get("sigaction_from")
            .is_some_and(|s| s.ends_with("/libsigchain.so")),
        format!(
            "sigaction from libsigchain (got {:?})",
            normal.get("sigaction_from")
        ),
    );
    // controls
    let global_foreign = foreign_bindings(&work.join("ld-global"), &app_dir)?;
    check(
        global.int("app_vkCreateInstance") != Some(AOSP_VALUE)
            && global.int("global_leaks").is_some_and(|n| n > 0)
            && !global_foreign.is_empty(),
        format!(
            "control RTLD_GLOBAL: app gets {:?}, leaks {:?}, foreign bindings {}",
            global.get("app_vkCreateInstance"),
            global.get("global_leaks"),
            global_foreign.len()
        ),
    );
    check(
        silent.module_lines == 0 && silent.int("painted").is_some_and(|p| p >= 1),
        format!(
            "control module records filtered: {} module lines, painted {:?}",
            silent.module_lines,
            silent.get("painted")
        ),
    );
    let refused = |r: &Run, want: &[&str]| {
        r.get("attach_error")
            .is_some_and(|e| want.iter().all(|w| e.contains(w)))
            && r.get("painted").is_none()
            && r.int("app_vkCreateInstance") == Some(AOSP_VALUE)
    };
    check(
        refused(&missing, &[&format!("platform module {MODULE_FILE}:")]),
        format!(
            "control module absent: named refusal, app still loads (attach_error {:?})",
            missing.get("attach_error")
        ),
    );
    check(
        refused(&foreign, &["abi ", ", launcher "]),
        format!(
            "control module of another build (abi id patched): named refusal (attach_error {:?})",
            foreign.get("attach_error")
        ),
    );
    match failures.first() {
        None => {
            println!(
                "ui-module: ok (controls: RTLD_GLOBAL leaks, filtered records vanish, absent module and foreign abi refused by name)"
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(first) => {
            println!(
                "ui-module: FAILED {} of 11; first error: {first:?}",
                failures.len()
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

/// The app library and its AOSP libvulkan.so stand-in, NDK-built into `dir`. `-nostdlib`: the fixture has no bionic linker, and neither object needs libc (the stand-ins call nothing).
fn build_app(root: &Path, args: &Args, dir: &Path) -> Result<PathBuf> {
    let ndk = match &args.ndk {
        Some(n) => n.clone(),
        None => crate::ndk::default_ndk(root)?,
    };
    let t = NdkTarget::new(root, &ndk, "x86_64", API)?;
    std::fs::create_dir_all(dir)?;
    let src = root.join(SOURCES);
    let vulkan = dir.join("libvulkan.so");
    host_art::exec(
        t.cmd("clang")
            .args(["-shared", "-nostdlib", "-fPIC", "-Wl,-soname,libvulkan.so"])
            .arg(src.join("aosp_vulkan.c"))
            .arg("-o")
            .arg(&vulkan),
        "NDK clang AOSP libvulkan stand-in",
    )?;
    let app = dir.join("libapp.so");
    host_art::exec(
        t.cmd("clang")
            .args(["-shared", "-nostdlib", "-fPIC"])
            .arg(src.join("app.c"))
            .arg(&vulkan)
            .arg("-o")
            .arg(&app),
        "NDK clang app",
    )?;
    Ok(app)
}

/// Builds the platform module and installs it as `<out>/ui-module-lib/libatl_platform_ui_linux.so`; returns the dir, for the library path of every process that uses the atl-platform-ui facade (the launcher in a runtime run, this gate's fixture).
pub fn stage_module(root: &Path, out: &Path) -> Result<PathBuf> {
    crate::cdylib::stage(root, out, MODULE_PACKAGE, "ui-module-lib", MODULE_FILE)
}

/// A copy of `module` in `dir` whose build identity differs: every little-endian occurrence of the fixture's reported `abi_id` (the vtable's first field, atl-platform-ui-api `abi::ABI_ID`) with each byte inverted. Stands for a module of another build without a second build.
fn foreign_build(module: &Path, run: &Run, dir: &Path) -> Result<PathBuf> {
    let id = run.get("abi_id").context("fixture reported no abi_id")?;
    let id = u64::from_str_radix(id, 16).with_context(|| format!("abi_id {id}"))?;
    let (from, to) = (id.to_le_bytes(), (!id).to_le_bytes());
    let mut bytes =
        std::fs::read(module).with_context(|| format!("reading {}", module.display()))?;
    let mut n = 0;
    let mut i = 0;
    while i + from.len() <= bytes.len() {
        if bytes[i..i + from.len()] == from {
            bytes[i..i + from.len()].copy_from_slice(&to);
            n += 1;
            i += from.len();
        } else {
            i += 1;
        }
    }
    ensure!(n >= 1, "abi id {id:016x} not found in {}", module.display());
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(MODULE_FILE), &bytes)?;
    Ok(dir.to_path_buf())
}

/// The `LD_DEBUG=bindings` records of one run (files `<prefix>.<pid>`) as (line, from object, to object).
fn bindings(prefix: &Path) -> Result<Vec<(String, String, String)>> {
    let dir = prefix.parent().context("ld log dir")?;
    let stem = format!(
        "{}.",
        prefix.file_name().context("ld log name")?.to_string_lossy()
    );
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if !p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(&stem))
        {
            continue;
        }
        for line in std::fs::read_to_string(&p)?.lines() {
            let Some((_, rest)) = line.split_once("binding file ") else {
                continue;
            };
            let Some((from, to)) = rest.split_once(" to ") else {
                continue;
            };
            let object = |s: &str| s.split(" [").next().unwrap_or(s).to_owned();
            found.push((line.trim().to_owned(), object(from), object(to)));
        }
    }
    Ok(found)
}

/// Bindings where an object in `app_dir` bound a symbol outside `app_dir` and glibc.
fn foreign_bindings(prefix: &Path, app_dir: &Path) -> Result<Vec<String>> {
    let app = app_dir.display().to_string();
    Ok(bindings(prefix)?
        .into_iter()
        .filter(|(_, from, to)| {
            from.starts_with(&app)
                && !to.starts_with(&app)
                && !GLIBC.iter().any(|g| to.ends_with(g))
        })
        .map(|(line, _, _)| line)
        .collect())
}

/// Bindings where a host object (a system library dir) bound a symbol of an AOSP object under `device`.
fn reverse_bindings(prefix: &Path, device: &Path) -> Result<Vec<String>> {
    let device = device.display().to_string();
    Ok(bindings(prefix)?
        .into_iter()
        .filter(|(_, from, to)| {
            (from.starts_with("/usr/") || from.starts_with("/lib")) && to.starts_with(&device)
        })
        .map(|(line, _, _)| line)
        .collect())
}
