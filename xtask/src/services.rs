// SPDX-License-Identifier: MIT
//! `xtask test binder` stage `services`: the M2 Rust services (crate atl-services) registered with AOSP servicemanager and called through binderd, inside binder.rs's harness.
//! 1. atl-system-services as uid 1000 (AID_SYSTEM) registers `activity` and `package` (addService) for one test package, whose launcher the services start (`--launch`). The harness stands in for atld:
//!    it serves the services' process control (atl_broker::control, their stdin) and starts the process they ask for, the client below, as the package's uid with the startSeq they chose.
//! 2. atl-services-client `app` as that process: getService of each, attach -> bindApplication, finishAttach -> LaunchActivityItem, getDisplayId, the PackageManager subset,
//!    the settings provider and its `call`, null returns, an unimplemented method, a denied @EnforcePermission method (crates/atl-services/src/bin/client.rs).
//! 3. atl-services-client `system` as uid 1000: the same @EnforcePermission method passes the hook and reaches the unimplemented default.
//! 4. Java nullability through a C++ client: AOSP `service call` (shell uid) sends a null String (int32 -1, Parcel.writeString(null)) to hasSystemFeature and getApplicationInfo; the Rust
//!    server takes it as None (before aidl --rust_java_nullability it failed the transaction with UNEXPECTED_NULL) and getApplicationInfo answers null (parcelable flag 0).
//! 5. The services' counter lines: the unimplemented calls of 2 and 3 were counted.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use atl_android_paths::{DATA_APP, DATA_USER, DATA_USER_DE};
use atl_broker::control::{self, Request, Response};
use atl_broker::{Broker, Process};
use atl_layout::Layout;
use atl_services::native_libs::LIB_DIR_NAME;
use atl_services::packages::{ActivityDecl, InstrumentationDecl, Manifest, PackageRecord};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

use crate::atld_run::without_stamp;
use crate::{binder, packages, tree};

const PACKAGE: &str = "atl-services";
/// The test package: an app uid (AID_APP_START 10000 + 123, any app id), its paths, launcher activity and start.
const APP: &str = "com.example.atltest";
const APP_UID: u32 = 10_123;
const ACTIVITY: &str = "com.example.atltest.Main";
/// The instrumentation it declares (client.rs checks the PackageManager's answers for it).
const INSTRUMENTATION: &str = "com.example.atltest.Runner";
/// Its manifest values: versionCode 1, min and target SDK 34 (any; the client checks only what it is told here).
const VERSION_CODE: i64 = 1;
const SDK: i32 = 34;
/// The prefix of the startSeq argument the services pass (ActivityThread.PROC_START_SEQ_IDENT).
const SEQ_ARG: &str = "seq=";
/// Android uids (android_filesystem_config.h): AID_SYSTEM, AID_SHELL.
const AID_SYSTEM: u32 = 1000;
const AID_SHELL: u32 = 2000;
/// How long the services may take to register: two addService round trips, well under a second; 10 s for a loaded machine.
const READY: Duration = Duration::from_secs(10);
/// How long a client run may take: about 20 round trips and two oneway callbacks (client.rs waits 5 s per callback at most).
const CLIENT_LIMIT: Duration = Duration::from_secs(20);
/// What `service call` prints for a reply of exactly status 0 and an int32 0 (false, or a null parcelable's flag): Parcel's text dump (libs/binder TextOutput), words then their
/// characters, as measured 2026-11-03; the closing `'...')` pins the length to those two words.
const REPLY_OK_ZERO: &str = "Result: Parcel(\t00000000 00000000   '........')";
/// Counter lines the client runs must produce (atl_services::counters): getProcessLimit (app run) and getMoveStatus (system run, past the permission hook).
const MUST_COUNT: &[&str] = &[
    "services: unimplemented IActivityManager.getProcessLimit 1",
    "services: unimplemented IPackageManager.getMoveStatus 1",
];

/// One process's (exit success, output).
type Run = (bool, String);

fn wait(child: &mut Child, limit: Duration) -> Result<Option<std::process::ExitStatus>> {
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

/// `cmd` as a binder process of `uid`, output to `log`; killed after `limit`.
fn run(b: &Broker, cmd: Command, uid: u32, log: &Path, limit: Duration) -> Result<Run> {
    let mut p = binder::spawn_logged(b, &cmd, uid, log)?;
    let status = wait(&mut p.child, limit)?;
    let text = std::fs::read_to_string(log)?;
    Ok(match status {
        Some(s) => (s.success(), text),
        None => (false, format!("{text}(killed after {limit:?})")),
    })
}

/// The transaction code `service call` needs, from `atl-services-client codes` output.
fn code(codes: &str, method: &str) -> Option<String> {
    codes.lines().find_map(|l| {
        l.strip_prefix(&format!("code {method} "))
            .map(str::to_owned)
    })
}

/// The verdict: every client check passed (and the runs exited 0), both `service call` replies are status 0 + 0, every counter line is there.
fn verdict(app: &Run, system: &Run, feature: &Run, info: &Run, services_log: &str) -> Vec<String> {
    let mut fails = Vec::new();
    for (what, r) in [("app", app), ("system", system)] {
        for l in r.1.lines().filter(|l| l.starts_with("client: FAIL")) {
            fails.push(format!("{what}: {l}"));
        }
        if !r.0 && !r.1.contains("client: FAIL") {
            fails.push(format!("client {what} failed: {}", r.1.trim()));
        }
    }
    for (what, r) in [
        ("hasSystemFeature(null)", feature),
        ("getApplicationInfo(null)", info),
    ] {
        if !r.1.contains(REPLY_OK_ZERO) {
            fails.push(format!(
                "service call {what}: want `{REPLY_OK_ZERO}...`, got {}",
                r.1.trim()
            ));
        }
    }
    for want in MUST_COUNT {
        if !services_log.lines().any(|l| without_stamp(l) == *want) {
            fails.push(format!("counter line missing: {want}"));
        }
    }
    fails
}

/// Runs the stage; the passes are printed, the failures returned. `host_arch`: the arches.toml `[host]` row the services run on; the record's native library dir is the one the installer gives a package without native code there (native_libs.rs: the first supported ISA, the host's).
pub fn stage(
    root: &Path,
    out: &Path,
    host_arch: &str,
    b: &Broker,
    env: &[(String, String)],
    service: &Path,
    test_root: &Path,
) -> Result<Vec<String>> {
    let arches = crate::arches::load(root)?;
    ensure!(
        arches.get(host_arch).is_some_and(|a| a.host.is_some()),
        "{host_arch}: no {} row with [host]",
        crate::arches::FILE
    );
    let host = packages::bin(root, out, PACKAGE, "atl-system-services")?;
    // the services read their .kcm maps from one directory: AOSP's keyboards dir holds both
    let keychars = tree::keyboards(&Layout::of(root)?);
    for (file, _) in tree::KEYBOARD_FILES {
        let path = keychars.join(file);
        ensure!(path.is_file(), "missing input {}", path.display());
    }
    let client = packages::bin(root, out, PACKAGE, "atl-services-client")?;
    let services_log = test_root.join("services.log");
    // the installed package, as atld's install step would write it
    let record = PackageRecord {
        manifest: Manifest {
            name: APP.to_owned(),
            version_code: VERSION_CODE,
            version_name: None,
            min_sdk: SDK,
            target_sdk: SDK,
            process_name: None,
            application_theme: 0,
            application_class: None,
            app_component_factory: None,
            application_meta_data: vec![],
            application_booleans: BTreeMap::new(),
            activities: vec![ActivityDecl {
                name: ACTIVITY.to_owned(),
                launcher: true,
                theme: 0,
                config_changes: 0,
                booleans: BTreeMap::new(),
                launch_mode: 0,
                task_affinity: None,
                process: None,
                meta_data: vec![],
            }],
            receivers: vec![],
            requested_permissions: vec![],
            permissions: Vec::new(),
            libraries: vec![],
            // one instrumentation, targeting the package itself (client.rs INSTRUMENTATION)
            instrumentation: vec![InstrumentationDecl {
                name: INSTRUMENTATION.to_owned(),
                target_package: APP.to_owned(),
                target_processes: None,
                handle_profiling: false,
                functional_test: true,
            }],
        },
        uid: APP_UID,
        source_dir: format!("{DATA_APP}/{APP}/base.apk"),
        primary_cpu_abi: None,
        native_library_dir: format!("{DATA_APP}/{APP}/{LIB_DIR_NAME}/{host_arch}"),
        data_dir: format!("{DATA_USER}/0/{APP}"),
        device_protected_data_dir: format!("{DATA_USER_DE}/0/{APP}"),
        granted_permissions: vec![],
    };
    let packages_file = test_root.join("packages.json");
    std::fs::write(&packages_file, serde_json::to_vec_pretty(&[&record])?)?;
    let (control_ours, control_theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let mut cmd = Command::new(&host);
    cmd.arg("--packages")
        .arg(&packages_file)
        .arg("--keychars")
        .arg(&keychars)
        .args(["--launch", APP])
        .envs(env.iter().cloned());
    let mut services_spawn = binder::spawn_of(&cmd, AID_SYSTEM)?;
    services_spawn.stdin = Some(control_theirs);
    services_spawn.stdout = Stdio::inherit();
    services_spawn.stderr = std::fs::File::create(&services_log)?.into();
    // the process the services start: the client, as the package's uid; the handler sends its Process here
    let (started_tx, started_rx) = channel::<Process>();
    let client_log = test_root.join("client-app.log");
    // the harness is atld for this stage: what the services start, it spawns; it kills nothing (the client ends by itself)
    let serve = |tx: Sender<Process>| {
        let result = control::serve(&control_ours, |request| match request {
            Request::Start(start) => {
                let Some(seq) = start
                    .args
                    .iter()
                    .find_map(|a| a.strip_prefix(SEQ_ARG))
                    .map(str::to_owned)
                else {
                    return Response::Error {
                        message: format!("no {SEQ_ARG}<startSeq> in {:?}", start.args),
                    };
                };
                let mut app = Command::new(&client);
                app.envs(env.iter().cloned()).args([
                    "app",
                    "--package",
                    &start.package,
                    "--uid",
                    &start.uid.to_string(),
                    "--activity",
                    ACTIVITY,
                    "--start-seq",
                    &seq,
                ]);
                match binder::spawn_logged(b, &app, start.uid, &client_log) {
                    Ok(p) => {
                        // the tag binderd reports, as atld answers
                        let pid = p.host_pid;
                        tx.send(p).ok();
                        Response::Started { pid }
                    }
                    Err(e) => Response::Error {
                        message: format!("{e:#}"),
                    },
                }
            }
            Request::Kill { pid } => Response::Error {
                message: format!("the services stage starts no process it kills (pid {pid})"),
            },
        });
        // the services ending their end of the socket is how the loop stops; any other error is theirs to report
        drop(result);
    };
    std::thread::scope(|scope| -> Result<Vec<String>> {
        scope.spawn(|| serve(started_tx));
        let Process {
            child: mut services,
            ..
        } = b
            .spawn(services_spawn)
            .context("starting atl-system-services")?;
        let start = Instant::now();
        while !std::fs::read_to_string(&services_log)?.contains("services: ready") {
            if let Some(s) = services.try_wait()? {
                return Ok(vec![format!(
                    "atl-system-services exited {s}: {}",
                    std::fs::read_to_string(&services_log)?.trim()
                )]);
            }
            if start.elapsed() > READY {
                let _ = services.kill();
                let _ = services.wait();
                return Ok(vec![format!(
                    "atl-system-services not ready after {READY:?}"
                )]);
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let codes = Command::new(&client)
            .arg("codes")
            .envs(env.iter().cloned())
            .output()?;
        let codes = String::from_utf8_lossy(&codes.stdout).into_owned();
        // the services' `--launch` made them ask for the app process: the client the handler spawned
        let app: Run = match started_rx.recv_timeout(READY) {
            Ok(mut p) => {
                let status = wait(&mut p.child, CLIENT_LIMIT)?;
                let text = std::fs::read_to_string(&client_log)?;
                match status {
                    Some(s) => (s.success(), text),
                    None => (false, format!("{text}(killed after {CLIENT_LIMIT:?})")),
                }
            }
            Err(_) => (
                false,
                format!("the services did not start a process within {READY:?}"),
            ),
        };
        let mut system = Command::new(&client);
        system.envs(env.iter().cloned()).arg("system");
        let system = run(
            b,
            system,
            AID_SYSTEM,
            &test_root.join("client-system.log"),
            CLIENT_LIMIT,
        )?;
        let call = |method: &str, args: &[&str], log: &str| -> Result<Run> {
            let Some(c) = code(&codes, method) else {
                return Ok((
                    false,
                    format!("no transaction code for {method} in {codes:?}"),
                ));
            };
            let mut cmd = Command::new(service);
            cmd.envs(env.iter().cloned())
                // `--`: service parses options with getopt, which would take the argument -1 for one
                .args(["--", "call", "package", &c])
                .args(args);
            run(b, cmd, AID_SHELL, &test_root.join(log), CLIENT_LIMIT)
        };
        // hasSystemFeature(String name = null, int version = 0); getApplicationInfo(String packageName = null, long flags = 0, int userId = 0)
        let feature = call(
            "hasSystemFeature",
            &["i32", "-1", "i32", "0"],
            "service-feature.log",
        )?;
        let info = call(
            "getApplicationInfo",
            &["i32", "-1", "i64", "0", "i32", "0"],
            "service-info.log",
        )?;

        // the services' end of the control socket closes with them, which ends the handler's loop (the scope waits for it)
        let _ = services.kill();
        services.wait()?;
        let log = std::fs::read_to_string(&services_log)?;
        for l in app
            .1
            .lines()
            .chain(system.1.lines())
            .filter(|l| l.starts_with("client: pass"))
        {
            println!("binder: services {}", l.trim_start_matches("client: "));
        }
        let fails = verdict(&app, &system, &feature, &info, &log);
        if fails.is_empty() {
            println!(
                "binder: services pass (the services started the app through process control; null String in, null parcelable out through `service call`; unimplemented calls counted)"
            );
        }
        Ok(fails)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each check fires on its own: a failed client check, a client that died, a reply that is not 0 + 0, a missing counter line.
    #[test]
    fn verdicts() {
        let ok: Run = (true, "client: pass getService activity, package\n".into());
        let reply: Run = (true, format!("{REPLY_OK_ZERO}\n"));
        let log = MUST_COUNT.join("\n");
        assert!(verdict(&ok, &ok, &reply, &reply, &log).is_empty());
        assert_eq!(
            verdict(
                &(false, "client: FAIL x y\n".into()),
                &ok,
                &reply,
                &reply,
                &log
            )
            .len(),
            1
        );
        assert_eq!(
            verdict(&ok, &(false, "Error: boom".into()), &reply, &reply, &log).len(),
            1
        );
        assert_eq!(
            verdict(
                &ok,
                &ok,
                &(true, "Result: Parcel(fffffff6 '....')".into()),
                &reply,
                &log
            )
            .len(),
            1
        );
        assert_eq!(verdict(&ok, &ok, &reply, &reply, MUST_COUNT[0]).len(), 1);
        assert_eq!(
            code(
                "code getApplicationInfo 9\ncode hasSystemFeature 105\n",
                "hasSystemFeature"
            )
            .as_deref(),
            Some("105")
        );
    }
}
