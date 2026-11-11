// SPDX-License-Identifier: MIT
//! `xtask test gralloc`: our allocator service and `mapper.atl.so` against AOSP's own libui, in binder.rs's harness (binderd + AOSP servicemanager).
//! 1. The test layout's /vendor manifest declares what atld declares (`atl_gralloc::vintf::device_hals`): the allocator instance and the mapper library. servicemanager would refuse the @VintfStability service otherwise, and libui checks `isDeclared` before it loads anything.
//! 2. `atl-gralloc-allocator` as uid 1000 registers `android.hardware.graphics.allocator.IAllocator/default` and allocates through GBM on this host's DRM device (WSL: vgem).
//! 3. `atl-gralloc-probe writer` and `reader` as two app-uid processes with `mapper.atl.so` on their library path: libui (`AHardwareBuffer_*`, Gralloc5) allocates 64x64 RGBA_8888, locks, writes an analytic pattern, unlocks, and sends the handle over a unix socket;
//!    the reader (a second process: `importBuffer` of the mapper) compares every pixel, checks libui's decode of our standard metadata (describe, id, plane layout) agrees, and writes one pixel the writer then sees (shared dma-buf).
//!
//! Needs `xtask native` to have built servicemanager, service, libnativewindow and the crates atl-gralloc-service, atl-gralloc-truth (its converter modules are in the build's graph), and a DRM device that allocates: without one the allocator exits with its explanation, which this prints.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_broker::{Broker, Process, to_log};
use atl_gralloc::vintf;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

use crate::binder;
use crate::packages;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// The DRM node the allocator uses instead of trying /dev/dri's render then card nodes.
    #[arg(long)]
    drm_node: Option<PathBuf>,
}

const ALLOCATOR_PACKAGE: &str = "atl-gralloc-service";
const ALLOCATOR_BIN: &str = "atl-gralloc-allocator";
const PROBE_PACKAGE: &str = "atl-gralloc-truth";
const PROBE_BIN: &str = "atl-gralloc-probe";
/// Android uids (android_filesystem_config.h): AID_SYSTEM runs the HAL, AID_APP_START + 123 stands for an app.
const AID_SYSTEM: u32 = 1000;
const AID_APP: u32 = 10_123;
/// How long the allocator may take to register: GBM probing of up to a few nodes and one addService (well under a second measured on WSL); 15 s for a loaded machine.
const READY: Duration = Duration::from_secs(15);
/// How long the probe pair may take: two allocation round trips through binderd plus 4096 pixel reads; about a second measured. 30 s covers a loaded machine.
const PROBE_LIMIT: Duration = Duration::from_secs(30);
/// The line the allocator prints when registered (crates/atl-gralloc-service/src/bin/allocator.rs).
const READY_LINE: &str = "gralloc: ready";
/// The prefix of the allocator's counter line (`Allocator::counters_line`).
const COUNTERS_LINE: &str = "gralloc: counters ";

/// Every case each probe role reports, in the order it reports them (crates/atl-gralloc-truth/src/bin/probe.rs). A missing one fails the stage.
const WRITER_CASES: [&str; 9] = [
    "allocate",
    "describe",
    "id",
    "lock-planes",
    "unlock",
    "send-handle",
    "reader-finished",
    "sees-reader-write",
    // the line the probe prints last is a case of its own so a probe that died early is not mistaken for one that passed
    "end",
];
const READER_CASES: [&str; 7] = [
    "recv-handle",
    "describe-imported",
    "id-matches",
    "layout-from-handle",
    "reads-identical-bytes",
    "write-back",
    "end",
];

/// The `gralloc: pass|FAIL <role>:<case>[: why]` lines of one probe's output against its case list: the passes, and every failure (a FAIL line, a missing or extra case, a bad exit).
fn verdict(role: &str, cases: &[&str], ok: bool, stdout: &str) -> (Vec<String>, Vec<String>) {
    let mut passes = Vec::new();
    let mut fails = Vec::new();
    let mut seen = Vec::new();
    for line in stdout.lines().filter_map(|l| l.strip_prefix("gralloc: ")) {
        let Some((kind, rest)) = line.split_once(' ') else {
            continue;
        };
        let Some(case) = rest
            .strip_prefix(role)
            .and_then(|r| r.strip_prefix(':'))
            .map(|r| r.split(':').next().unwrap_or(r))
        else {
            continue;
        };
        match kind {
            "pass" => passes.push(format!("{role}:{case}")),
            "FAIL" => fails.push(rest.to_owned()),
            _ => continue,
        }
        seen.push(case.to_owned());
    }
    // `end` is added by the harness when the process ended with status 0, so a probe that stopped early lacks it
    if ok {
        seen.push("end".to_owned());
        passes.push(format!("{role}:end"));
    }
    let want: Vec<String> = cases.iter().map(|c| (*c).to_owned()).collect();
    let mut sorted_seen = seen.clone();
    sorted_seen.sort();
    let mut sorted_want = want.clone();
    sorted_want.sort();
    if sorted_seen != sorted_want {
        fails.push(format!("{role}: cases reported {seen:?}, want {want:?}"));
    }
    if !ok && fails.is_empty() {
        fails.push(format!("{role}: ended badly with no FAIL line"));
    }
    (passes, fails)
}

/// Builds `mapper.atl.so` (crate atl-gralloc-mapper, plain cargo into `<out>/cargo`) and installs it under the file name libui looks for, in `<out>/gralloc/lib`; returns that directory (for LD_LIBRARY_PATH: libui's `android_load_sphal_library` is `dlopen` here).
pub fn stage_mapper(root: &Path, out: &Path) -> Result<PathBuf> {
    crate::cdylib::stage(
        root,
        out,
        "atl-gralloc-mapper",
        "gralloc/lib",
        &vintf::mapper_library_name(),
    )
}

/// Appends `dir` (the staged mapper's) to `env`'s LD_LIBRARY_PATH.
pub fn append_lib_path(env: &mut [(String, String)], dir: &Path) {
    for (k, v) in env.iter_mut() {
        if k == "LD_LIBRARY_PATH" {
            v.push(':');
            v.push_str(&dir.display().to_string());
        }
    }
}

/// `child`'s status, or None after killing it at `limit`.
fn wait_limited(child: &mut Child, limit: Duration) -> Result<Option<std::process::ExitStatus>> {
    let start = Instant::now();
    loop {
        if let Some(s) = child.try_wait()? {
            return Ok(Some(s));
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            child.wait()?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The DRM device directory the allocator opens (atl_gralloc_service::backend::DRM_DIR), bound into its sandbox as atld binds it.
const DRM_DIR: &str = "/dev/dri";

/// The probe's `--sock` fd: its stdin, the one fd the broker passes besides the binder connection.
const PROBE_SOCK_FD: &str = "0";

/// A probe role as a binder process of uid `AID_APP` with `sock` as its stdin (`--sock 0`).
fn spawn_probe(
    b: &Broker,
    probe: &Path,
    role: &str,
    sock: OwnedFd,
    env: &[(String, String)],
    log: &Path,
) -> Result<Process> {
    let mut cmd = Command::new(probe);
    cmd.arg(role)
        .args(["--sock", PROBE_SOCK_FD])
        .envs(env.iter().cloned());
    let mut s = binder::spawn_of(&cmd, AID_APP)?;
    s.stdin = Some(sock);
    (s.stdout, s.stderr) = to_log(std::fs::File::create(log)?)?;
    b.spawn(s).context("spawning the probe")
}

/// Starts the allocator as the system uid in binder.rs's harness (servicemanager up, the layout's /vendor manifest from `vintf::device_hals`), output to `log`; returns once it printed its ready line.
/// `drm_node`: its `--drm-node`, bound into the sandbox when outside `/dev/dri`.
pub fn start_allocator(
    broker: &Broker,
    allocator: &Path,
    env: &[(String, String)],
    log: &Path,
    drm_node: Option<&Path>,
) -> Result<Child> {
    let mut cmd = Command::new(allocator);
    if let Some(node) = drm_node {
        cmd.arg("--drm-node").arg(node);
    }
    cmd.envs(env.iter().cloned());
    let mut s = binder::spawn_of(&cmd, AID_SYSTEM)?;
    (s.stdout, s.stderr) = to_log(std::fs::File::create(log)?)?;
    // the DRM nodes it allocates on (the sandbox's /dev is otherwise minimal), and a --drm-node elsewhere
    s.devices = std::iter::once(PathBuf::from(DRM_DIR))
        .chain(
            drm_node
                .filter(|n| !n.starts_with(DRM_DIR))
                .map(Path::to_path_buf),
        )
        .collect();
    let Process {
        child: mut alloc, ..
    } = broker.spawn(s).context("starting the allocator")?;
    let start = Instant::now();
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains(READY_LINE) {
            return Ok(alloc);
        }
        if let Some(s) = alloc.try_wait()? {
            bail!(
                "the allocator exited {s} before it registered:\n{}",
                text.trim()
            );
        }
        if start.elapsed() > READY {
            let _ = alloc.kill();
            bail!(
                "the allocator was not ready after {READY:?}:\n{}",
                text.trim()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The allocator binary (`xtask native` builds it into the slot).
pub fn allocator(root: &Path, out: &Path) -> Result<PathBuf> {
    packages::bin(root, out, ALLOCATOR_PACKAGE, ALLOCATOR_BIN)
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "gralloc")?;
    let device = out.join("device");
    let servicemanager = device.join("servicemanager/servicemanager");
    let service = device.join("service/service");
    for b in [&servicemanager, &service] {
        ensure!(
            b.is_file(),
            "{} missing: cargo xtask native --host {} --out {} servicemanager service libnativewindow",
            b.display(),
            args.host,
            out.display()
        );
    }
    let allocator = allocator(root, &out)?;
    let probe = packages::bin(root, &out, PROBE_PACKAGE, PROBE_BIN)?;
    let mapper_dir = stage_mapper(root, &out)?;

    let test_root = out.join("gralloc-test");
    binder::fresh_layout_with(&test_root, &vintf::device_hals())?;
    let mut env = crate::stage_env::lib_env(&out)?;
    append_lib_path(&mut env, &mapper_dir);

    let (broker, mut binderd) = binder::start_binderd(root, &out, &test_root.join("binderd.log"))?;
    let (mut sm, found) =
        binder::start_servicemanager(&broker, &test_root, &servicemanager, &service, &env)?;
    let result = (|| -> Result<Vec<String>> {
        ensure!(
            found.0,
            "servicemanager did not come up: {}",
            found.1.trim()
        );
        let alloc_log = test_root.join("allocator.log");
        let mut alloc = start_allocator(
            &broker,
            &allocator,
            &env,
            &alloc_log,
            args.drm_node.as_deref(),
        )?;

        // the two processes of the probe, joined by a unix socket
        let (a, b) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )?;
        let writer_log = test_root.join("writer.log");
        let reader_log = test_root.join("reader.log");
        let mut writer = spawn_probe(&broker, &probe, "writer", a, &env, &writer_log)?;
        let mut reader = spawn_probe(&broker, &probe, "reader", b, &env, &reader_log)?;
        let reader_status = wait_limited(&mut reader.child, PROBE_LIMIT)?;
        let writer_status = wait_limited(&mut writer.child, PROBE_LIMIT)?;
        let _ = alloc.kill();
        let _ = alloc.wait();

        let mut fails = Vec::new();
        for (role, cases, status, log) in [
            ("writer", &WRITER_CASES[..], writer_status, &writer_log),
            ("reader", &READER_CASES[..], reader_status, &reader_log),
        ] {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            let ok = status.is_some_and(|s| s.success());
            if status.is_none() {
                fails.push(format!("{role}: killed after {PROBE_LIMIT:?}"));
            }
            let (passes, f) = verdict(role, cases, ok, &text);
            for p in &passes {
                println!("gralloc: pass {p}");
            }
            fails.extend(f);
            if !ok {
                println!("--- {role} output ---\n{}", text.trim());
            }
        }
        let alloc_text = std::fs::read_to_string(&alloc_log).unwrap_or_default();
        if alloc_text.contains("panicked") {
            fails.push(format!("the allocator panicked:\n{}", alloc_text.trim()));
        }
        // the allocator prints its counters at start and on each change: the last line is the final value (vgem: >= 1, the probe allocation)
        match alloc_text
            .lines()
            .rev()
            .find(|l| l.starts_with(COUNTERS_LINE))
        {
            Some(line) => println!("{line}"),
            None => fails.push(format!("the allocator printed no `{COUNTERS_LINE}` line")),
        }
        Ok(fails)
    })();
    let _ = sm.child.kill();
    sm.child.wait().ok();
    drop(broker);
    binderd.child.wait()?;
    let fails = result?;
    for f in &fails {
        println!("gralloc: FAIL {f}");
    }
    if fails.is_empty() {
        println!(
            "gralloc: pass (AOSP libui allocated, locked, shared and read a 64x64 RGBA_8888 buffer through {} and {})",
            vintf::allocator_service_name(),
            vintf::mapper_library_name()
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", crate::verdict::gate("gralloc", &fails));
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(role: &str, cases: &[&str]) -> String {
        cases
            .iter()
            .filter(|c| **c != "end")
            .map(|c| format!("gralloc: pass {role}:{c}\n"))
            .collect()
    }

    #[test]
    fn complete_passing_run() {
        let (p, f) = verdict(
            "reader",
            &READER_CASES,
            true,
            &lines("reader", &READER_CASES),
        );
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(p.len(), READER_CASES.len());
    }

    /// Each way a run can be wrong is caught on its own.
    #[test]
    fn each_failure_fires() {
        let full = lines("reader", &READER_CASES);
        // a FAIL line
        let bad = full.replace("pass reader:write-back", "FAIL reader:write-back: nope");
        assert!(!verdict("reader", &READER_CASES, true, &bad).1.is_empty());
        // a missing case (the probe stopped early but exited 0)
        let short = full.replace("gralloc: pass reader:id-matches\n", "");
        assert!(!verdict("reader", &READER_CASES, true, &short).1.is_empty());
        // an extra case
        let extra = format!("{full}gralloc: pass reader:surprise\n");
        assert!(!verdict("reader", &READER_CASES, true, &extra).1.is_empty());
        // a bad exit with no FAIL line, and an empty log
        assert!(!verdict("reader", &READER_CASES, false, &full).1.is_empty());
        assert!(!verdict("reader", &READER_CASES, true, "").1.is_empty());
        // another role's lines are not this role's
        assert!(!verdict("writer", &WRITER_CASES, true, &full).1.is_empty());
    }
}
