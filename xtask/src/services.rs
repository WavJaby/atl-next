// SPDX-License-Identifier: MIT
//! `xtask test binder` stage `services`: the M2 Rust services (crate atl-services) registered with AOSP servicemanager and called through binderd, inside binder.rs's harness.
//! 1. atl-system-services as uid 1000 (AID_SYSTEM) registers `activity` and `package` (addService) for one test package with one expected start.
//! 2. atl-services-client `app` as the package's uid: getService of each, attach -> bindApplication, finishAttach -> LaunchActivityItem, getDisplayId, the PackageManager subset,
//!    the settings provider and its `call`, null returns, an unimplemented method, a denied @EnforcePermission method (crates/atl-services/src/bin/client.rs).
//! 3. atl-services-client `system` as uid 1000: the same @EnforcePermission method passes the hook and reaches the unimplemented default.
//! 4. Java nullability through a C++ client: AOSP `service call` (shell uid) sends a null String (int32 -1, Parcel.writeString(null)) to hasSystemFeature and getApplicationInfo; the Rust
//!    server takes it as None (before aidl --rust_java_nullability it failed the transaction with UNEXPECTED_NULL) and getApplicationInfo answers null (parcelable flag 0).
//! 5. The services' counter lines: the unimplemented calls of 2 and 3 were counted.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::binder::Harness;
use crate::packages;

const PACKAGE: &str = "atl-services";
/// The test package: an app uid (AID_APP_START 10000 + 123, any app id), its paths, launcher activity and start.
const APP: &str = "com.example.atltest";
const APP_UID: u32 = 10_123;
const ACTIVITY: &str = "com.example.atltest.Main";
const START_SEQ: &str = "7";
/// Its manifest values: versionCode 1, min and target SDK 34 (any; the client checks only what it is told here).
const VERSION_CODE: &str = "1";
const SDK: &str = "34";
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
fn run(h: &Harness, mut cmd: Command, uid: u32, log: &Path, limit: Duration) -> Result<Run> {
    let file = std::fs::File::create(log)?;
    cmd.stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    let mut child = h.spawn(cmd, uid, false)?;
    let status = wait(&mut child, limit)?;
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
        if !services_log.lines().any(|l| l == *want) {
            fails.push(format!("counter line missing: {want}"));
        }
    }
    fails
}

/// Runs the stage; the passes are printed, the failures returned.
pub fn stage(
    root: &Path,
    out: &Path,
    h: &Harness,
    env: &[(String, String)],
    service: &Path,
    test_root: &Path,
) -> Result<Vec<String>> {
    let host = packages::bin(root, out, PACKAGE, "atl-system-services")?;
    let client = packages::bin(root, out, PACKAGE, "atl-services-client")?;
    let services_log = test_root.join("services.log");
    let mut cmd = Command::new(&host);
    let app_dir = format!("/data/user/0/{APP}");
    let device_dir = format!("/data/user_de/0/{APP}");
    let apk = format!("/data/app/{APP}/base.apk");
    let uid = APP_UID.to_string();
    cmd.args([
        "--package",
        APP,
        "--uid",
        &uid,
        "--apk",
        &apk,
        "--data",
        &app_dir,
        "--device-data",
        &device_dir,
        "--activity",
        ACTIVITY,
        "--start-seq",
        START_SEQ,
        "--version-code",
        VERSION_CODE,
        "--min-sdk",
        SDK,
        "--target-sdk",
        SDK,
        // the client is a Rust IApplicationThread, which cannot read the raw descriptor a Java app gets
        "--no-app-shared-memory",
    ])
    .envs(env.iter().cloned())
    .stdin(Stdio::null())
    .stderr(std::fs::File::create(&services_log)?);
    let mut services = h
        .spawn(cmd, AID_SYSTEM, false)
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
    let mut app = Command::new(&client);
    app.envs(env.iter().cloned()).args([
        "app",
        "--package",
        APP,
        "--uid",
        &uid,
        "--activity",
        ACTIVITY,
        "--start-seq",
        START_SEQ,
    ]);
    let app = run(
        h,
        app,
        APP_UID,
        &test_root.join("client-app.log"),
        CLIENT_LIMIT,
    )?;
    let mut system = Command::new(&client);
    system.envs(env.iter().cloned()).arg("system");
    let system = run(
        h,
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
        run(h, cmd, AID_SHELL, &test_root.join(log), CLIENT_LIMIT)
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
            "binder: services pass (null String in, null parcelable out through `service call`; unimplemented calls counted)"
        );
    }
    Ok(fails)
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
