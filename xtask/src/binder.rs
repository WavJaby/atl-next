// SPDX-License-Identifier: MIT
//! `xtask test binder`: AOSP's servicemanager and `service` command, built by the converter on libbinder's driver seam (patches/aosp/frameworks/native 0003), talking through binderd.
//!
//! Every process goes through atl-broker as atld's do: `Broker::start` runs `atl-binderd` in its BinderDaemon sandbox, `Broker::spawn` each binder process in the app sandbox, registered with binderd tagged {uid, host pid}; so this gate runs on the argv the broker emits.
//! servicemanager: uid 1000 (AID_SYSTEM), the one process allowed to become context manager, with the test layout's /system, /vendor, /odm (empty VINTF manifests) mounted.
//! `service`: uid 2000 (AID_SHELL). Gate: `service list` lists `manager` (servicemanager registers itself, main.cpp), `service check manager` finds it, `service check <absent>` does not; binderd's counters show the driver paths that must fire on this workload: the context manager's TXN_SECURITY_CTX flag (ignored) and libbinder's oneway-spam ioctl (unsupported).
//! Then stage `services` (services.rs): the M2 Rust services registered with that servicemanager and called through binderd. Needs `xtask native` to have built crate atl-services.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use atl_broker::{Broker, Mount, Process, Spawn, to_log};
use atl_layout::Layout;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding servicemanager and service (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
}

/// Android uids (system/core/libcutils/include/private/android_filesystem_config.h): AID_SYSTEM, AID_SHELL.
const AID_SYSTEM: u32 = 1000;
const AID_SHELL: u32 = 2000;
/// The name servicemanager registers itself under (cmds/servicemanager/main.cpp `addService("manager", ...)`).
const MANAGER: &str = "manager";
/// A name nothing registers, shaped as an AIDL HAL instance (`<package>.<Interface>/<instance>`) so servicemanager's miss path consults the VINTF manifests (ServiceManager.cpp `isVintfDeclared` via `NativeName::fill` / AIDL instance names): without them it logs `NULL VINTF MANIFEST`.
const ABSENT: &str = "android.hardware.atltest.IAbsent/default";
/// How long servicemanager may take to answer its first `service check` (a cold start reads VINTF and starts its Looper: well under a second measured; 10 s leaves room for a loaded machine).
const STARTUP: Duration = Duration::from_secs(10);
/// binderd counter lines this workload must produce (atl-binderd main.rs prints them; any nonzero count): servicemanager's nodes carry TXN_SECURITY_CTX (`becomeContextManager`, and `setRequestingSid` on its services), and libbinder sends BINDER_ENABLE_ONEWAY_SPAM_DETECTION at open (ProcessState.cpp `open_driver`). Measured 2026-11-02: 3 and 4.
/// 0x40046210 = _IOW('b', 16, __u32) = BINDER_ENABLE_ONEWAY_SPAM_DETECTION (uapi binder.h), once per process open (9 in this workload, 2026-11-06). Left unsupported on purpose:
/// EINVAL is what a kernel without the feature answers, libbinder tolerates it (logs only when binderfs advertises `features/oneway_spam_detection`), and accepting it
/// without BR_ONEWAY_SPAM_SUSPECT (async-space accounting binderd lacks) would claim a detection that never runs.
const MUST_FIRE: &[&str] = &[
    "binderd: ignored TxnSecurityCtx ",
    "binderd: fault UnsupportedIoctl(40046210) ",
];
/// Every fault line not in MUST_FIRE fails the gate.
const FAULT_PREFIX: &str = "binderd: fault ";

/// The test /system, /vendor, /odm under `root`: an empty framework manifest (/system/etc/vintf/manifest.xml) and device manifests (/vendor, /odm), libvintf's default paths (system/libvintf/include/vintf/constants.h). /vendor declares `vendor_hals`.
fn layout(root: &Path, vendor_hals: &str) -> Result<()> {
    for (dir, kind) in [
        ("system", "framework"),
        ("vendor", "device"),
        ("odm", "device"),
    ] {
        let d = root.join(dir).join("etc/vintf");
        std::fs::create_dir_all(&d)?;
        let hals = if dir == "vendor" { vendor_hals } else { "" };
        std::fs::write(
            d.join("manifest.xml"),
            atl_gralloc::vintf::manifest(kind, hals),
        )?;
    }
    Ok(())
}

/// Links each (host file, absolute Android path) row below `root`, as atld's `system_layout` links its `--file` rows below the product root. The rows are `atld_run::platform_rows`.
pub(crate) fn link_rows(root: &Path, rows: &[(PathBuf, String)]) -> Result<()> {
    for (host, at) in rows {
        let dst = root.join(at.trim_start_matches('/'));
        ensure!(!dst.exists(), "two files at {}", dst.display());
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::os::unix::fs::symlink(std::path::absolute(host)?, &dst)?;
    }
    Ok(())
}

/// `root`'s system, vendor, odm, read-only at their Android paths: servicemanager's VINTF manifests, and the /system rows an app process reads (`link_rows`).
pub(crate) fn layout_mounts(root: &Path) -> Vec<Mount> {
    ["system", "vendor", "odm"]
        .into_iter()
        .map(|d| Mount {
            host: root.join(d),
            at: PathBuf::from("/").join(d),
            writable: false,
        })
        .collect()
}

/// `cmd` (program, arguments, variables set on it, working directory) as the broker's [`Spawn`] of a binder process tagged `uid`: read-only mounts of the LD_LIBRARY_PATH dirs at their own paths (the runtime's shared host paths, as atld mounts them), no devices or property area, stdin /dev/null, output dropped; the caller sets what it needs.
/// Variables `cmd` removes are not expressible (the sandbox inherits the broker's environment): an error.
pub(crate) fn spawn_of(cmd: &Command, uid: u32) -> Result<Spawn> {
    let mut env = Vec::new();
    for (k, v) in cmd.get_envs() {
        let v = v.with_context(|| format!("{k:?}: a removed variable has no Spawn form"))?;
        env.push((k.to_owned(), v.to_owned()));
    }
    // the LD_LIBRARY_PATH trees, read-only at their own paths with what their links name (atl_broker::tree_mounts)
    let mut mounts = Vec::new();
    for (k, v) in &env {
        if k == "LD_LIBRARY_PATH" {
            mounts.extend(atl_broker::library_path_mounts(v)?);
        }
    }
    Ok(Spawn {
        uid,
        context_manager: false,
        mounts,
        links: vec![],
        devices: vec![],
        property_area: None,
        rights: vec![],
        program: cmd.get_program().into(),
        args: cmd.get_args().map(OsStr::to_owned).collect(),
        env,
        cwd: cmd.get_current_dir().map(Path::to_path_buf),
        stdin: None,
        stdout: Stdio::null(),
        stderr: Stdio::null(),
    })
}

/// [`spawn_of`] with stdout + stderr to the file `log`, started.
pub(crate) fn spawn_logged(b: &Broker, cmd: &Command, uid: u32, log: &Path) -> Result<Process> {
    let mut s = spawn_of(cmd, uid)?;
    (s.stdout, s.stderr) = to_log(std::fs::File::create(log)?)?;
    b.spawn(s)
        .with_context(|| format!("spawning {}", cmd.get_program().display()))
}

/// [`spawn_of`] with stdout and stderr piped (read through `Process::child`, e.g. `wait_with_output`), started.
pub(crate) fn spawn_piped(b: &Broker, cmd: &Command, uid: u32) -> Result<Process> {
    spawn_piped_with(b, cmd, uid, Vec::new())
}

/// [`spawn_piped`] with `mounts` (`layout_mounts`: the test layout's /system, /vendor, /odm).
pub(crate) fn spawn_piped_with(
    b: &Broker,
    cmd: &Command,
    uid: u32,
    mounts: Vec<Mount>,
) -> Result<Process> {
    let mut s = spawn_of(cmd, uid)?;
    s.mounts = mounts;
    s.stdout = Stdio::piped();
    s.stderr = Stdio::piped();
    b.spawn(s)
        .with_context(|| format!("spawning {}", cmd.get_program().display()))
}

/// atl-binderd and atl-sandbox-stub, built into `out`'s cargo target dir as `xtask native` builds crates; binderd started by the broker (its BinderDaemon sandbox, Landlock through the stub) with its stderr (counters) in `log`.
pub(crate) fn start_binderd(root: &Path, out: &Path, log: &Path) -> Result<(Broker, Process)> {
    let status = crate::cargo_command()
        .args([
            "build",
            "--release",
            "-p",
            "atl-binderd",
            "-p",
            "atl-sandbox-stub",
            "--target-dir",
        ])
        .arg(out.join("cargo"))
        .current_dir(root)
        .status()?;
    ensure!(status.success(), "building atl-binderd failed");
    let bin = out.join("cargo/release");
    Broker::start(
        &bin.join("atl-binderd"),
        &bin.join("atl-sandbox-stub"),
        std::fs::File::create(log)?,
    )
    .context("starting atl-binderd")
}

/// `service <args>` as the shell uid: (success, stdout + stderr), output through `log` (a file: a pipe would hang on any grandchild holding it). Killed after `RUN_LIMIT`.
fn service(
    b: &Broker,
    service: &Path,
    env: &[(String, String)],
    args: &[&str],
    log: &Path,
) -> Result<(bool, String)> {
    let mut cmd = Command::new(service);
    cmd.args(args).envs(env.iter().cloned());
    let mut p = spawn_logged(b, &cmd, AID_SHELL, log)?;
    let status = wait_limited(&mut p.child, RUN_LIMIT)?;
    let text = std::fs::read_to_string(log)?;
    Ok(match status {
        Some(s) => (s.success(), text),
        None => (false, format!("{text}(killed after {RUN_LIMIT:?})")),
    })
}

/// How long one `service` run may take: a lookup is one binder round trip (well under 100 ms); a hang means a lost reply.
const RUN_LIMIT: Duration = Duration::from_secs(5);

/// `child`'s exit status, or None after killing it at `limit`.
pub(crate) fn wait_limited(
    child: &mut Child,
    limit: Duration,
) -> Result<Option<std::process::ExitStatus>> {
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
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The gate's verdict over the three `service` runs and binderd's counter output.
fn verdict(
    list: &(bool, String),
    found: &(bool, String),
    absent: &(bool, String),
    counters: &str,
) -> Vec<String> {
    let mut fails = Vec::new();
    if !list.0
        || !list
            .1
            .contains(&format!("{MANAGER}: [android.os.IServiceManager]"))
    {
        fails.push(format!(
            "service list does not list {MANAGER}: {}",
            list.1.trim()
        ));
    }
    if !found.0 || !found.1.contains(&format!("Service {MANAGER}: found")) {
        fails.push(format!("service check {MANAGER}: {}", found.1.trim()));
    }
    if !absent.1.contains(&format!("Service {ABSENT}: not found")) {
        fails.push(format!("service check {ABSENT}: {}", absent.1.trim()));
    }
    for want in MUST_FIRE {
        if !counters.lines().any(|l| l.starts_with(want)) {
            fails.push(format!("binderd counter missing: {want}"));
        }
    }
    // a protocol fault this workload does not cause is a bug in a peer or binderd, even when the calls pass
    for line in counters.lines().filter(|l| l.starts_with(FAULT_PREFIX)) {
        if !MUST_FIRE.iter().any(|want| line.starts_with(want)) {
            fails.push(format!("unexpected binderd fault: {line}"));
        }
    }
    fails
}

/// The environment of every process run from a converter build in `out`: LD_LIBRARY_PATH = every library the converter linked and the provided crates' (libatl_binder_driver), as native-load stages them.
pub(crate) fn lib_env(out: &Path) -> Result<Vec<(String, String)>> {
    let device = out.join("device");
    let mut lib_dirs: Vec<String> = Vec::new();
    for dir in [device.clone(), out.join("crates")] {
        for e in std::fs::read_dir(&dir)? {
            let p = e?.path();
            let p = if dir == device { p } else { p.join("lib") };
            if p.is_dir() {
                lib_dirs.push(p.display().to_string());
            }
        }
    }
    Ok(vec![("LD_LIBRARY_PATH".to_owned(), lib_dirs.join(":"))])
}

/// servicemanager as the context manager (uid 1000) in the test layout under `test_root` (which `layout` filled), and the result of its first `service check manager`, polled until it answers, exits or `STARTUP` passes.
pub(crate) fn start_servicemanager(
    b: &Broker,
    test_root: &Path,
    servicemanager: &Path,
    service: &Path,
    env: &[(String, String)],
) -> Result<(Process, (bool, String))> {
    let mut cmd = Command::new(servicemanager);
    cmd.envs(env.iter().cloned());
    let mut s = spawn_of(&cmd, AID_SYSTEM)?;
    s.context_manager = true;
    s.mounts.extend(layout_mounts(test_root));
    (s.stdout, s.stderr) = to_log(std::fs::File::create(test_root.join("servicemanager.log"))?)?;
    let mut sm = b.spawn(s).context("spawning servicemanager")?;

    // servicemanager answers once its Looper runs
    let start = Instant::now();
    let log = |n: &str| test_root.join(format!("service-{n}.log"));
    let found = loop {
        let r = self::service(b, service, env, &["check", MANAGER], &log("check"))?;
        if r.0 && r.1.contains("found") && !r.1.contains("not found") || start.elapsed() > STARTUP {
            break r;
        }
        if let Some(status) = sm.child.try_wait()? {
            break (false, format!("servicemanager exited: {status}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    Ok((sm, found))
}

/// The test /system, /vendor, /odm under `root` (see `layout`), recreated empty.
pub(crate) fn fresh_layout(root: &Path) -> Result<()> {
    fresh_layout_with(root, "")
}

/// `fresh_layout` with /vendor's manifest declaring `vendor_hals`.
pub(crate) fn fresh_layout_with(root: &Path, vendor_hals: &str) -> Result<()> {
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    layout(root, vendor_hals)
}

/// Native goals (gate_goals.rs): servicemanager and `service`, plus the crates stage `services` runs (services.rs).
pub fn goals() -> crate::gate_goals::Goals {
    crate::gate_goals::Goals::host(&[
        "servicemanager",
        "service",
        "atl-aidl-stubs-rust",
        "libbinder_rs",
        "libatl_parcelables",
    ])
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "binder")?;
    let device = out.join("device");
    let servicemanager = device.join("servicemanager/servicemanager");
    let service = device.join("service/service");
    for b in [&servicemanager, &service] {
        ensure!(
            b.is_file(),
            "{} missing: cargo xtask native --host {} --out {} servicemanager service",
            b.display(),
            args.host,
            out.display()
        );
    }
    let test_root = out.join("binder-test");
    if test_root.exists() {
        std::fs::remove_dir_all(&test_root)?;
    }
    layout(&test_root, "")?;
    let env = lib_env(&out)?;

    let (b, mut binderd) = start_binderd(root, &out, &test_root.join("binderd.log"))?;

    let sm_log_path = test_root.join("servicemanager.log");
    let log = |n: &str| test_root.join(format!("service-{n}.log"));
    let (mut sm, found) = start_servicemanager(&b, &test_root, &servicemanager, &service, &env)?;
    let list = self::service(&b, &service, &env, &["list"], &log("list"))?;
    let absent = self::service(&b, &service, &env, &["check", ABSENT], &log("absent"))?;
    let services_fails =
        crate::services::stage(root, &out, &args.host, &b, &env, &service, &test_root)?;

    let _ = sm.child.kill();
    sm.child.wait()?;
    drop(b);
    let bstatus = wait_limited(&mut binderd.child, RUN_LIMIT)?;
    let counters = std::fs::read_to_string(test_root.join("binderd.log"))?;
    let sm_log = std::fs::read_to_string(&sm_log_path)?;
    print!("{counters}");
    if sm_log.contains("NULL VINTF MANIFEST") {
        println!("binder: WARN servicemanager logs NULL VINTF MANIFEST");
    }
    let mut fails = verdict(&list, &found, &absent, &counters);
    fails.extend(services_fails);
    match bstatus {
        Some(s) if s.success() => {}
        Some(s) => fails.push(format!("atl-binderd exited {s}")),
        None => fails.push("atl-binderd did not exit when its admin connection closed".into()),
    }
    for f in &fails {
        println!("binder: FAIL {f}");
    }
    if fails.is_empty() {
        println!(
            "binder: pass (service list / check manager / check {ABSENT} through binderd to AOSP servicemanager)"
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("servicemanager log:\n{sm_log}");
    println!("{}", crate::verdict::gate("binder", &fails));
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each check fires on its own: a missing listing, a failed lookup, an absent name reported found, a counter that did not fire.
    #[test]
    fn verdicts() {
        let list = (
            true,
            format!("Found 1 services:\n0\t{MANAGER}: [android.os.IServiceManager]\n"),
        );
        let found = (true, format!("Service {MANAGER}: found\n"));
        let absent = (true, format!("Service {ABSENT}: not found\n"));
        let counters =
            "binderd: ignored TxnSecurityCtx 1\nbinderd: fault UnsupportedIoctl(40046210) 2\n";
        // another fault, or another unsupported ioctl, fails even with the expected ones present
        for extra in [
            "binderd: fault BadHandshake 1\n",
            "binderd: fault UnsupportedIoctl(c0306201) 1\n",
        ] {
            let v = verdict(&list, &found, &absent, &format!("{counters}{extra}"));
            assert_eq!(v.len(), 1, "{extra}: {v:?}");
            assert!(v[0].starts_with("unexpected binderd fault"), "{v:?}");
        }
        assert!(verdict(&list, &found, &absent, counters).is_empty());
        assert_eq!(
            verdict(&(true, String::new()), &found, &absent, counters).len(),
            1
        );
        assert_eq!(
            verdict(&list, &(false, String::new()), &absent, counters).len(),
            1
        );
        assert_eq!(
            verdict(
                &list,
                &found,
                &(true, format!("Service {ABSENT}: found")),
                counters
            )
            .len(),
            1
        );
        assert_eq!(
            verdict(
                &list,
                &found,
                &absent,
                "binderd: ignored TxnSecurityCtx 1\n"
            )
            .len(),
            1
        );
    }

    #[test]
    fn manifests_and_layout() {
        let dir = std::env::temp_dir().join(format!("atl-binder-layout-{}", std::process::id()));
        layout(&dir, "").unwrap();
        let s = std::fs::read_to_string(dir.join("vendor/etc/vintf/manifest.xml")).unwrap();
        assert!(roxmltree::Document::parse(&s).is_ok());
        assert!(dir.join("system/etc/vintf/manifest.xml").is_file());
        // declared HALs land in /vendor's manifest only, and the file stays well-formed
        layout(&dir, &atl_gralloc::vintf::device_hals()).unwrap();
        let s = std::fs::read_to_string(dir.join("vendor/etc/vintf/manifest.xml")).unwrap();
        let doc = roxmltree::Document::parse(&s).unwrap();
        assert_eq!(
            doc.descendants().filter(|n| n.has_tag_name("hal")).count(),
            2
        );
        let system = std::fs::read_to_string(dir.join("system/etc/vintf/manifest.xml")).unwrap();
        assert!(!system.contains("<hal"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// libbinder's files holding the driver I/O (patches/aosp/frameworks/native 0003), relative to `src/`.
const SEAM_FILES: &[&str] = &[
    "aosp/frameworks/native/libs/binder/ProcessState.cpp",
    "aosp/frameworks/native/libs/binder/IPCThreadState.cpp",
];

/// Lines of `text` that do driver I/O past the seam: a raw `ioctl(` / `mmap(` / `munmap(` anywhere (every one in these files is on the driver), or a raw `open(` / `close(` on a line naming the driver (`mDriverFD`, `driver`). `binder_driver_*` calls and other identifiers ending in these names (`::close`, `fclose`) are not raw.
/// Limit: a driver fd copied into another variable first and closed on a later line is not seen; review catches it, the gate guards the patched one-line sites.
fn seam_violations(text: &str) -> Vec<(usize, String)> {
    let raw = |line: &str, call: &str| {
        line.match_indices(call).any(|(i, _)| {
            line[..i].chars().next_back().is_none_or(|c| {
                !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.' || c == '>')
            })
        })
    };
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            // code only: without the line comment and the text of string literals (log messages name ioctl())
            let mut code = String::new();
            let mut quoted = false;
            let mut prev = ' ';
            for c in l.split("//").next().unwrap_or_default().chars() {
                if c == '"' && prev != '\\' {
                    quoted = !quoted;
                } else if !quoted {
                    code.push(c);
                }
                prev = c;
            }
            let code = code.as_str();
            ["ioctl(", "mmap(", "munmap("].iter().any(|c| raw(code, c))
                || ((code.contains("mDriverFD") || code.contains("driver"))
                    && ["open(", "close("].iter().any(|c| raw(code, c)))
        })
        .map(|(i, l)| (i + 1, l.trim().to_owned()))
        .collect()
}

/// `xtask check` step `seam`: no driver I/O in libbinder bypasses `BinderDriver.h`, in the tree this checkout builds (task copy or `src/`).
pub fn seam_check(root: &Path) -> Result<()> {
    let layout = Layout::of(root)?;
    let mut bad = Vec::new();
    for f in SEAM_FILES {
        let path = layout.locate(Path::new(f)).dir;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("{}: cargo xtask fetch --execute", path.display()))?;
        for (n, l) in seam_violations(&text) {
            bad.push(format!("{}:{n}: {l}", path.display()));
        }
    }
    ensure!(
        bad.is_empty(),
        "driver I/O past BinderDriver.h:\n{}",
        bad.join("\n")
    );
    eprintln!(
        "seam: ok ({} files, every driver call through binder_driver_*)",
        SEAM_FILES.len()
    );
    Ok(())
}

#[cfg(test)]
mod seam_tests {
    use super::seam_violations;

    #[test]
    fn raw_driver_calls_fire() {
        let ok = "x = binder_driver_ioctl(mDriverFD, BINDER_WRITE_READ, &bwr);\nbinder_driver_close(mDriverFD);\n    close(fd); // feature file\n";
        assert!(seam_violations(ok).is_empty());
        let bad = "if (ioctl(mDriverFD, BINDER_WRITE_READ, &bwr) >= 0)\n  close(gProcess->mDriverFD);\n p = mmap(nullptr, 1, 0, 0, fd, 0);\n";
        assert_eq!(
            seam_violations(bad).iter().map(|v| v.0).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
