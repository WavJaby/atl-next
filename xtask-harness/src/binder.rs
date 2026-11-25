// SPDX-License-Identifier: MIT
//! `xtask test binder`: AOSP's servicemanager and `service` command, built by the converter on libbinder's driver seam (patches/aosp/frameworks/native 0002), talking through binderd.
//!
//! Every process goes through atl-broker as atld's do: `Broker::start` runs `atl-binderd` in its BinderDaemon sandbox, `Broker::spawn` each binder process in the app sandbox, registered with binderd tagged {uid, host pid}; so this gate runs on the argv the broker emits.
//! servicemanager: uid 1000 (AID_SYSTEM), domain servicemanager, the one process allowed to become context manager, with the test layout's /system, /system_ext, /vendor, /odm mounted (empty VINTF
//! manifests, the build's policy files: it decides every add / find / list by the service access table). Each process takes the domain of the work it does, as atld gives it.
//! `service`: uid 2000 (AID_SHELL), domain shell. Gate: `service list` lists `manager` (servicemanager registers itself, main.cpp), `service check manager` finds it, `service check <absent>` does not; binderd's counters show the driver path that must fire on this workload: libbinder's oneway-spam ioctl (unsupported).
//! Then stage `services` (services.rs): the M2 Rust services registered with that servicemanager and called through binderd. Needs `xtask native` to have built crate atl-services.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::installer::{new_records, seccomp_records};
use crate::stage_env::lib_env;
use crate::wait::{Miss, Wait, until_child};
use anyhow::{Context, Result, bail, ensure};
use atl_broker::{Broker, Mount, Process, Spawn, to_log};
use atl_sepolicy::domains;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding servicemanager and service (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Break: the harness answers the services' control `Install` with atld's installer-exit error (atl-installer exited non-zero); the services stage passes only if they exit before `ready` with no package installed. The sweep stage is skipped.
    #[arg(long)]
    break_install: bool,
}

/// Android uids (system/core/libcutils/include/private/android_filesystem_config.h): AID_SYSTEM, AID_SHELL.
const AID_SYSTEM: u32 = 1000;
const AID_SHELL: u32 = 2000;
/// AID_APP_START (android_filesystem_config.h): the first application uid; the sweep client runs as one.
const AID_APP: u32 = 10000;
/// Longest the sweep client may take: each direction's grid, a bisection of about 20 probes of up to 1 MiB each, and the rows around each limit.
const SWEEP_LIMIT: Duration = Duration::from_secs(300);
/// [`wait_limited`]'s poll: an exit is seen up to this late. A poll is one `waitpid`, no log read, so it is finer than wait.rs `POLL`.
const EXIT_LIMITED_POLL: Duration = Duration::from_millis(10);
/// The name servicemanager registers itself under (cmds/servicemanager/main.cpp `addService("manager", ...)`).
const MANAGER: &str = "manager";
/// A name nothing registers, shaped as an AIDL HAL instance (`<package>.<Interface>/<instance>`) so servicemanager's miss path consults the VINTF manifests (ServiceManager.cpp `isVintfDeclared` via `NativeName::fill` / AIDL instance names): without them it logs `NULL VINTF MANIFEST`.
const ABSENT: &str = "android.hardware.atltest.IAbsent/default";
/// How long servicemanager may take to answer its first `service check` (a cold start reads VINTF and starts its Looper: well under a second measured on a quiet host). The bound only bites
/// when it never comes up; 10 s, with each check killed at RUN_LIMIT, failed gates compositor and gralloc at load 10-22 (2026-11-20, `killed after 5s`); 60 s is generous on purpose.
const STARTUP: Duration = Duration::from_secs(60);
/// binderd counter lines this workload must produce (atl-binderd main.rs prints them; any nonzero count): libbinder sends BINDER_ENABLE_ONEWAY_SPAM_DETECTION at open (ProcessState.cpp `open_driver`).
/// 0x40046210 = _IOW('b', 16, __u32) = BINDER_ENABLE_ONEWAY_SPAM_DETECTION (uapi binder.h), once per process open (9 in this workload, 2026-11-06). Left unsupported on purpose:
/// EINVAL is what a kernel without the feature answers, libbinder tolerates it (logs only when binderfs advertises `features/oneway_spam_detection`), and accepting it
/// without BR_ONEWAY_SPAM_SUSPECT (async-space accounting binderd lacks) would claim a detection that never runs.
const MUST_FIRE: &[&str] = &["binderd: fault UnsupportedIoctl(40046210) "];
/// Every fault line not in MUST_FIRE fails the gate.
const FAULT_PREFIX: &str = "binderd: fault ";
/// The sweep's test-only service_contexts row (appended to the layout's system_ext_service_contexts, never the product's): ATL's own registry type, `app_api_service` + `system_server_service`,
/// so the server (system_server) may add `atl.sweep` and the client (untrusted_app) find it, without borrowing an AOSP service's label.
const SWEEP_CONTEXT_ROW: &str = "atl.sweep    u:object_r:atl_compositor_registry_service:s0\n";

/// The test /system, /system_ext, /vendor, /odm under `root`: an empty framework manifest (/system/etc/vintf/manifest.xml) and device manifests (/vendor, /odm), libvintf's default paths (system/libvintf/include/vintf/constants.h). /vendor declares `vendor_hals`.
/// /system_ext starts empty: [`start_servicemanager`] places the policy files.
fn layout(root: &Path, vendor_hals: &str) -> Result<()> {
    std::fs::create_dir_all(root.join("system_ext"))?;
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

/// `root`'s system, vendor, odm, read-only at their Android paths: servicemanager's VINTF manifests, and the /system rows an app process reads (`link_rows`), with the host files those rows link to at their own paths (`atl_broker::tree_mounts`), which the sandbox otherwise does not show.
pub(crate) fn layout_mounts(root: &Path) -> Result<Vec<Mount>> {
    let mut mounts = Vec::new();
    for d in ["system", "system_ext", "vendor", "odm"] {
        let dir = root.join(d);
        mounts.push(Mount {
            host: dir.clone(),
            at: PathBuf::from("/").join(d),
            writable: false,
        });
        // tree_mounts' first entry is `dir` at its host path: the Android path above replaces it
        mounts.extend(atl_broker::tree_mounts(&dir)?.into_iter().skip(1));
    }
    Ok(mounts)
}

/// `cmd` (program, arguments, variables set on it, working directory) as the broker's [`Spawn`] of a binder process tagged `uid` in SELinux `context` (the domain of the work it does, as atld
/// gives it: servicemanager decides by it): read-only mounts of the LD_LIBRARY_PATH dirs at their own paths (the runtime's shared host paths, as atld mounts them), no devices or property area, stdin /dev/null, output dropped; the caller sets what it needs.
/// Variables `cmd` removes are not expressible (the sandbox inherits the broker's environment): an error.
pub(crate) fn spawn_of(cmd: &Command, uid: u32, context: &str) -> Result<Spawn> {
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
        // the gate's processes see the host's pids, as before sandbox tags existed: its checks read sender pids as host pids
        binder_pids: atl_broker::BinderPids::Host,
        context: context.to_owned(),
        mounts,
        links: vec![],
        devices: vec![],
        property_area: None,
        rights: vec![],
        app_id: None,
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
pub(crate) fn spawn_logged(
    b: &Broker,
    cmd: &Command,
    uid: u32,
    context: &str,
    log: &Path,
) -> Result<Process> {
    let mut s = spawn_of(cmd, uid, context)?;
    (s.stdout, s.stderr) = to_log(std::fs::File::create(log)?)?;
    b.spawn(s)
        .with_context(|| format!("spawning {}", cmd.get_program().display()))
}

/// [`spawn_of`] with stdout and stderr piped (read through `Process::child`, e.g. `wait_with_output`), started.
pub(crate) fn spawn_piped(b: &Broker, cmd: &Command, uid: u32, context: &str) -> Result<Process> {
    spawn_piped_with(b, cmd, uid, context, Vec::new())
}

/// [`spawn_piped`] with `mounts` added to [`spawn_of`]'s (e.g. `layout_mounts`: the test layout's /system, /vendor, /odm).
pub(crate) fn spawn_piped_with(
    b: &Broker,
    cmd: &Command,
    uid: u32,
    context: &str,
    mounts: Vec<Mount>,
) -> Result<Process> {
    let mut s = spawn_of(cmd, uid, context)?;
    s.mounts.extend(mounts);
    s.stdout = Stdio::piped();
    s.stderr = Stdio::piped();
    b.spawn(s)
        .with_context(|| format!("spawning {}", cmd.get_program().display()))
}

/// atl-binderd and atl-sandbox-stub as gate_build.rs builds them; binderd started by the broker (its BinderDaemon sandbox, Landlock through the stub) with its stderr (counters) in `log`.
pub(crate) fn start_binderd(root: &Path, out: &Path, log: &Path) -> Result<(Broker, Process)> {
    let built = crate::gate_build::ensure(root, out, &[crate::gate_build::Group::Release])?;
    Broker::start(
        &built.bin("atl-binderd"),
        &built.bin("atl-sandbox-stub"),
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
    service_limited(b, service, env, args, log, RUN_LIMIT)
}

/// [`service`] killed after `limit`.
fn service_limited(
    b: &Broker,
    service: &Path,
    env: &[(String, String)],
    args: &[&str],
    log: &Path,
    limit: Duration,
) -> Result<(bool, String)> {
    let mut cmd = Command::new(service);
    cmd.args(args).envs(env.iter().cloned());
    let mut p = spawn_logged(b, &cmd, AID_SHELL, domains::SHELL, log)?;
    let status = wait_limited("exit-binder-service", &mut p.child, limit)?;
    let text = std::fs::read_to_string(log)?;
    Ok(match status {
        Some(s) => (s.success(), text),
        None => (false, format!("{text}(killed after {limit:?})")),
    })
}

/// How long one `service` run may take: a lookup is one binder round trip (well under 100 ms); a hang means a lost reply.
const RUN_LIMIT: Duration = Duration::from_secs(5);

/// `child`'s exit status, or None after killing it at `limit`. `name` is the wait's record and its need-ledger key (gate_need.rs): one per call site, so a site's samples never mix with another's.
pub(crate) fn wait_limited(
    name: &'static str,
    child: &mut Child,
    limit: Duration,
) -> Result<Option<std::process::ExitStatus>> {
    let exit = Wait::exit(
        name,
        "the process outlived its limit",
        limit,
        EXIT_LIMITED_POLL,
    );
    match until_child(&exit, child) {
        Ok(_) => Ok(Some(child.wait()?)),
        Err(Miss::Late { .. }) => {
            let _ = child.kill();
            child.wait()?;
            Ok(None)
        }
        Err(miss) => bail!("{}", miss.text()),
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

/// The build's policy files (converter goal atl-sepolicy in `out`) below `root` where a device installs them, as every atld run places them (`atld_run::sepolicy_rows`): servicemanager labels each name
/// through the service_contexts files and decides by the service access table. `test_rows`, if any, are a gate's own service_contexts rows, appended to a copy of system_ext_service_contexts.
fn place_policy(root: &Path, out: &Path, test_rows: &str) -> Result<()> {
    let mut rows = crate::atld_run::sepolicy_rows(out)?;
    if !test_rows.is_empty() {
        let at = atl_android_paths::SYSTEM_EXT_SERVICE_CONTEXTS;
        let row = rows
            .iter_mut()
            .find(|(_, a)| a == at)
            .context("sepolicy rows lack system_ext_service_contexts")?;
        let copy = root.join("system_ext_service_contexts.test");
        std::fs::write(&copy, std::fs::read_to_string(&row.0)? + test_rows)?;
        row.0 = copy;
    }
    link_rows(root, &rows)
}

/// servicemanager as the context manager (uid 1000, domain servicemanager) in the test layout under `test_root` (which `layout` filled; the build's policy files from `out` are placed here, with
/// `test_rows`, see [`place_policy`]), and the result of its first `service check manager`, polled until it answers, exits or `STARTUP` passes.
pub(crate) fn start_servicemanager(
    b: &Broker,
    out: &Path,
    test_root: &Path,
    test_rows: &str,
    servicemanager: &Path,
    service: &Path,
    env: &[(String, String)],
) -> Result<(Process, (bool, String))> {
    place_policy(test_root, out, test_rows)?;
    let mut cmd = Command::new(servicemanager);
    cmd.envs(env.iter().cloned());
    let mut s = spawn_of(&cmd, AID_SYSTEM, domains::SERVICEMANAGER)?;
    s.context_manager = true;
    s.mounts.extend(layout_mounts(test_root)?);
    (s.stdout, s.stderr) = to_log(std::fs::File::create(test_root.join("servicemanager.log"))?)?;
    let mut sm = b.spawn(s).context("spawning servicemanager")?;

    // servicemanager answers once its Looper runs; a check started before servicemanager is the context manager waits for it inside libbinder, so one check may take all of STARTUP. Both
    // children are watched together: a servicemanager that exits is reported at once, not when the check it left waiting times out.
    let start = Instant::now();
    let log = test_root.join("service-check.log");
    let mut cmd = Command::new(service);
    cmd.args(["check", MANAGER]).envs(env.iter().cloned());
    let found = 'checks: loop {
        let mut check = spawn_logged(b, &cmd, AID_SHELL, domains::SHELL, &log)?;
        loop {
            let sm_exit = sm.child.try_wait()?;
            let done = match check.child.try_wait()? {
                Some(s) => Some((s.success(), std::fs::read_to_string(&log)?)),
                None => None,
            };
            match startup_step(done, sm_exit, start.elapsed()) {
                Step::Wait => std::thread::sleep(Duration::from_millis(10)),
                Step::Retry => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue 'checks;
                }
                Step::End(r) => {
                    let _ = check.child.kill();
                    let _ = check.child.wait();
                    break 'checks r;
                }
            }
        }
    };
    Ok((sm, found))
}

/// What [`start_servicemanager`]'s wait does next.
#[derive(Debug, PartialEq)]
enum Step {
    /// the check still runs
    Wait,
    /// the check answered without finding the manager: start another
    Retry,
    /// the result: the check's answer, or why there is none
    End((bool, String)),
}

/// One step of the start-up wait, from the check's answer (`done`: success + output, once it exited), servicemanager's exit status and the time since start. servicemanager's exit wins over
/// everything (the check it leaves waits until killed: its status is the diagnosis); then a found manager; then STARTUP.
fn startup_step(
    done: Option<(bool, String)>,
    sm_exit: Option<std::process::ExitStatus>,
    elapsed: Duration,
) -> Step {
    if let Some(status) = sm_exit {
        return Step::End((false, format!("servicemanager exited: {status}")));
    }
    match done {
        Some(r) if r.0 && r.1.contains("found") && !r.1.contains("not found") => Step::End(r),
        _ if elapsed > STARTUP => Step::End((
            false,
            format!("servicemanager did not answer a check in {STARTUP:?}"),
        )),
        Some(_) => Step::Retry,
        None => Step::Wait,
    }
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

/// Stage `sweep`: transactions of every size up to libbinder's limit, in both directions and oneway, through binderd (`atl-binder-sweep`, crate atl-services). The server is uid 1000 (it registers `atl.sweep`); the client an app uid.
/// Fails when the client fails, binderd ended, or dmesg gained a seccomp record naming atl-binderd: kill mode, so the record names the call the list lacks.
fn sweep_stage(
    b: &Broker,
    out: &Path,
    env: &[(String, String)],
    test_root: &Path,
    binderd: &mut Process,
) -> Result<Vec<String>> {
    let bin = out.join("cargo-converter/release/atl-binder-sweep");
    let mut server = Command::new(&bin);
    server.envs(env.iter().cloned()).arg("server");
    let mut client = Command::new(&bin);
    client.envs(env.iter().cloned()).arg("client");
    let before = seccomp_records();
    let mut srv = spawn_logged(
        b,
        &server,
        AID_SYSTEM,
        domains::SYSTEM_SERVER,
        &test_root.join("sweep-server.log"),
    )?;
    let log = test_root.join("sweep-client.log");
    let mut cli = spawn_logged(b, &client, AID_APP, domains::UNTRUSTED_APP, &log)?;
    let status = wait_limited("exit-binder-sweep", &mut cli.child, SWEEP_LIMIT)?;
    let _ = srv.child.kill();
    let _ = srv.child.wait();
    let text = std::fs::read_to_string(&log)?;
    print!("{text}");
    let mut fails = Vec::new();
    match status {
        Some(s) if s.success() => {}
        Some(s) => fails.push(format!(
            "sweep client exited {s}: {}",
            text.lines().last().unwrap_or_default()
        )),
        None => fails.push(format!(
            "sweep client did not finish within {SWEEP_LIMIT:?}"
        )),
    }
    if let Some(s) = binderd.child.try_wait()? {
        fails.push(format!("atl-binderd ended during the sweep: {s}"));
    }
    for r in new_records(before, seccomp_records())
        .iter()
        .filter(|r| r.contains("comm=\"atl-binderd\""))
    {
        println!("sweep: {r}");
        fails.push(format!("seccomp killed atl-binderd: {r}"));
    }
    Ok(fails)
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "binder")?;
    let device = out.join("device");
    let servicemanager = device.join("servicemanager/servicemanager");
    let service = device.join("service/service");
    let cmd_bin = device.join("cmd/cmd");
    for b in [&servicemanager, &service, &cmd_bin] {
        ensure!(
            b.is_file(),
            "{} missing: cargo xtask native --host {} --out {} servicemanager service cmd",
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
    let (mut sm, found) = start_servicemanager(
        &b,
        &out,
        &test_root,
        SWEEP_CONTEXT_ROW,
        &servicemanager,
        &service,
        &env,
    )?;
    let list = self::service(&b, &service, &env, &["list"], &log("list"))?;
    let absent = self::service(&b, &service, &env, &["check", ABSENT], &log("absent"))?;
    let services_fails = crate::services::stage(
        root,
        &out,
        &args.host,
        &b,
        &env,
        &service,
        &cmd_bin,
        &test_root,
        args.break_install,
    )?;
    let sweep_fails = if args.break_install {
        Vec::new()
    } else {
        sweep_stage(&b, &out, &env, &test_root, &mut binderd)?
    };

    let _ = sm.child.kill();
    sm.child.wait()?;
    drop(b);
    let bstatus = wait_limited("exit-binder-binderd", &mut binderd.child, RUN_LIMIT)?;
    let counters = std::fs::read_to_string(test_root.join("binderd.log"))?;
    let sm_log = std::fs::read_to_string(&sm_log_path)?;
    print!("{counters}");
    if sm_log.contains("NULL VINTF MANIFEST") {
        println!("binder: WARN servicemanager logs NULL VINTF MANIFEST");
    }
    let mut fails = verdict(&list, &found, &absent, &counters);
    fails.extend(services_fails);
    fails.extend(sweep_fails);
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

    /// Each `wait_limited` call site names its own wait (`wait: exit-<site>`), so one site's samples never mix with another's. Scans the call sites of the modules that use it: 14, all literal and distinct.
    #[test]
    fn every_wait_limited_call_site_names_its_own_wait() {
        let needle = concat!("wait_", "limited(");
        let sources = [
            include_str!("adb.rs"),
            include_str!("binder.rs"),
            include_str!("gralloc.rs"),
            include_str!("installer.rs"),
            include_str!("sandbox.rs"),
        ];
        let mut names = Vec::new();
        for src in sources {
            for (at, _) in src.match_indices(needle) {
                if src[..at].ends_with("fn ") {
                    continue;
                }
                let literal = src[at + needle.len()..]
                    .strip_prefix('"')
                    .and_then(|rest| rest.split('"').next());
                names.push(
                    literal
                        .expect("a call site with no literal name")
                        .to_owned(),
                );
            }
        }
        assert_eq!(names.len(), 14, "{names:?}");
        let distinct: std::collections::BTreeSet<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(distinct.len(), names.len(), "{names:?}");
        assert!(names.iter().all(|n| n.starts_with("exit-")), "{names:?}");
    }

    /// R7-10: a servicemanager that exited is reported as exited at once, with its status, while its check still waits (`None`) and also past STARTUP (was `killed after 60s`, 60 s late).
    /// Raw wait status 0x100 = exit code 1 (wait(2) WEXITSTATUS: the code in bits 8-15).
    #[test]
    fn startup_reports_an_exited_servicemanager_at_once() {
        use std::os::unix::process::ExitStatusExt;
        let exited = std::process::ExitStatus::from_raw(0x100);
        let want = Step::End((false, "servicemanager exited: exit status: 1".to_owned()));
        for elapsed in [Duration::from_millis(50), STARTUP + Duration::from_secs(1)] {
            assert_eq!(startup_step(None, Some(exited), elapsed), want);
        }
        let found = (true, format!("Service {MANAGER}: found\n"));
        let small = Duration::from_millis(50);
        assert_eq!(startup_step(None, None, small), Step::Wait);
        assert_eq!(
            startup_step(Some(found.clone()), None, small),
            Step::End(found)
        );
        let absent = (true, format!("Service {MANAGER}: not found\n"));
        assert_eq!(startup_step(Some(absent), None, small), Step::Retry);
        assert!(matches!(
            startup_step(None, None, STARTUP + Duration::from_secs(1)),
            Step::End((false, _))
        ));
    }

    /// Each check fires on its own: a missing listing, a failed lookup, an absent name reported found, a counter that did not fire.
    #[test]
    fn verdicts() {
        let list = (
            true,
            format!("Found 1 services:\n0\t{MANAGER}: [android.os.IServiceManager]\n"),
        );
        let found = (true, format!("Service {MANAGER}: found\n"));
        let absent = (true, format!("Service {ABSENT}: not found\n"));
        let counters = "binderd: fault UnsupportedIoctl(40046210) 2\n";
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
            verdict(&list, &found, &absent, "binderd: reject NoSpace 1\n").len(),
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
