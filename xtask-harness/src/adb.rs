// SPDX-License-Identifier: MIT
//! `xtask test adb` (plan adbd S2): AOSP's adbd, unpatched, in the broker's [`Profile::Adb`](atl_broker::Profile::Adb), answering AOSP's own host `adb` client over TCP on loopback.
//!
//! adbd reads only what its environment gives (plan adbd A3): ATL's property area (+ `service.adb.listen_addrs=tcp:localhost:5555`, a loopback listener by property), a device view with `/system/bin/{sh,toolbox,getprop -> toolbox}` and `/bin -> /system/bin`,
//! `/data/misc/adb/adb_keys` holding the client's public key, init's environment. `ro.debuggable=0` (a user build) keeps auth required, as on a phone.
//! Gate: `adb -s 127.0.0.1:5555 shell getprop ro.build.version.sdk` prints the build.prop's value (the source the property service loads, read here independently); the only listener on the port is 127.0.0.1 (`/proc/net/tcp{,6}`, what `ss -ltn` reads); `adb shell id` shows uid 2000 with drop_privileges' groups.
//! Break `--break-auth`: no adb_keys -> the client's device state is `unauthorized` and the shell never runs.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_android_paths::{DATA, SDCARD, SYSTEM, SYSTEM_BIN};
use atl_broker::{Link, Mount, Spawn, adb, to_log};

use crate::binder::{spawn_of, wait_limited};
use crate::stage_env::lib_env;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding adbd, sh, toolbox and the host adb (default: this checkout's slot, see `xtask slot`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Break: adbd gets no adb_keys; passes only if the client sees `unauthorized` and no shell output.
    #[arg(long)]
    break_auth: bool,
    /// Keep adbd serving this many seconds after the checks (a manual client, e.g. another machine's adb, connects meanwhile); the verdict is printed first.
    #[arg(long, value_name = "SECS")]
    serve: Option<u64>,
    /// Also authorize this public key (a manual client's `adbkey.pub`) in adb_keys.
    #[arg(long, value_name = "PUB")]
    key: Option<PathBuf>,
    /// Run this adb instead of the slot's AOSP host adb (e.g. an SDK's, for a side-by-side with a phone); it keeps its own key home, so --key must name its adbkey.pub.
    #[arg(long, value_name = "ADB", requires = "key")]
    client: Option<PathBuf>,
}

/// adbd's TCP port (adb's DEFAULT_ADB_LOCAL_TRANSPORT_PORT, adb.h) and the listen spec adbd reads first (daemon/main.cpp `adbd_main`: `service.adb.listen_addrs`; socket_spec.cpp: `tcp:localhost:N` = a loopback listener).
const PORT: u16 = 5555;
const LISTEN_PROP: &str = "service.adb.listen_addrs";
/// The property the gate reads through `adb shell getprop`.
const SDK_PROP: &str = "ro.build.version.sdk";
/// init's environment as adbd's shell inherits it on a device (system/core/rootdir/init.environ.rc.in), the part this shell reads; the APEX roots and BOOTCLASSPATH family join with ART in the shell (S4).
const INIT_ENV: &[(&str, &str)] = &[
    (
        "PATH",
        "/product/bin:/apex/com.android.runtime/bin:/apex/com.android.art/bin:/system_ext/bin:/system/bin:/system/xbin:/odm/bin:/vendor/bin:/vendor/xbin",
    ),
    ("ANDROID_ROOT", SYSTEM),
    ("ANDROID_DATA", DATA),
    ("EXTERNAL_STORAGE", SDCARD),
];
/// How long adbd may take to listen, and one client command to answer (a loaded machine: a cold adbd listens well under a second).
const START_LIMIT: Duration = Duration::from_secs(10);
const RUN_LIMIT: Duration = Duration::from_secs(20);
/// drop_privileges' supplementary groups as `id` prints them (daemon/main.cpp; names from android_filesystem_config.h via bionic's getgrgid, which toybox's id uses): the phone's `adb shell id` (A024) lists these.
const SHELL_GROUPS: &[&str] = &[
    "1004(input)",
    "1007(log)",
    "1011(adb)",
    "1015(sdcard_rw)",
    "1028(sdcard_r)",
    "3001(net_bt_admin)",
    "3002(net_bt)",
    "3003(inet)",
    "3006(net_bw_stats)",
    "3009(readproc)",
    "3011(uhid)",
];

/// The value `key` has in build.prop text `text` (last assignment wins, as the property service loads it).
fn prop_of(text: &str, key: &str) -> Option<String> {
    text.lines()
        .rev()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_owned())
}

/// The local addresses listening on `port` in `/proc/net/tcp` / `tcp6` text (proc(5): `sl local_address rem_address st ...`, st 0A = LISTEN, address hex in host byte order).
fn listeners(text: &str, port: u16) -> Vec<String> {
    let want = format!(":{port:04X}");
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.get(3) == Some(&"0A") && f.get(1)?.ends_with(&want)).then(|| f[1].to_owned())
        })
        .collect()
}

/// 127.0.0.1:PORT in /proc/net/tcp form (little-endian host order on x86_64 / riscv64 / aarch64).
fn loopback(port: u16) -> String {
    format!("0100007F:{port:04X}")
}

/// The device view under `dir`: system/bin with sh, toolbox and getprop -> toolbox (the phone's layout), data/misc/adb with `keys` concatenated as adb_keys (none: no key file).
fn device_view(dir: &Path, device: &Path, keys: &[PathBuf]) -> Result<()> {
    let bin = dir.join("system/bin");
    std::fs::create_dir_all(&bin)?;
    std::os::unix::fs::symlink(device.join("sh/sh"), bin.join("sh"))?;
    std::os::unix::fs::symlink(device.join("toolbox/toolbox"), bin.join("toolbox"))?;
    std::os::unix::fs::symlink("toolbox", bin.join("getprop"))?;
    let misc = dir.join("data/misc/adb");
    std::fs::create_dir_all(&misc)?;
    if !keys.is_empty() {
        let mut text = String::new();
        for k in keys {
            text += std::fs::read_to_string(k)?.trim_end();
            text.push('\n');
        }
        std::fs::write(misc.join("adb_keys"), text)?;
    }
    Ok(())
}

/// One run of the host client with its own server port and key home under `home`: (stdout + stderr, success).
fn client(adb: &Path, home: &Path, server_port: u16, args: &[&str]) -> Result<(String, bool)> {
    let log = home.join("client.log");
    let mut child = Command::new(adb)
        .args(args)
        .env("ANDROID_USER_HOME", home)
        .env("HOME", home)
        .env("ANDROID_ADB_SERVER_PORT", server_port.to_string())
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::File::options().append(true).open(&log)?)
        .spawn()
        .with_context(|| format!("running {}", adb.display()))?;
    let status = wait_limited(&mut child, RUN_LIMIT)?;
    let text = std::fs::read_to_string(&log)?;
    Ok((text, status.is_some_and(|s| s.success())))
}

/// A free loopback TCP port for the client's server (the default 5037 may be a developer's own server).
fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "adb")?;
    let device = out.join("device");
    let adbd = device.join("adbd/adbd");
    let adb_client = args
        .client
        .clone()
        .unwrap_or_else(|| out.join("host/adb/adb"));
    for b in [
        &adbd,
        &device.join("sh/sh"),
        &device.join("toolbox/toolbox"),
        &adb_client,
    ] {
        ensure!(
            b.is_file(),
            "{} missing: cargo xtask native --host {} --for adb",
            b.display(),
            args.host
        );
    }
    let test = out.join("adb-test");
    if test.exists() {
        std::fs::remove_dir_all(&test)?;
    }
    let home = test.join("home");
    std::fs::create_dir_all(&home)?;
    let server_port = free_port()?;
    // the client's key pair, made by the client itself (adb keygen), as ~/.android/adbkey{,.pub} on a desktop
    let mut keys = Vec::new();
    if args.client.is_none() {
        let key = home.join("adbkey");
        let (text, ok) = client(
            &adb_client,
            &home,
            server_port,
            &["keygen", &key.display().to_string()],
        )?;
        ensure!(ok, "adb keygen failed: {text}");
        keys.push(home.join("adbkey.pub"));
    }
    keys.extend(args.key.clone());
    if args.break_auth {
        keys.clear();
    }

    // ATL's property area: the out dir's build.prop + the listen spec, laid out by the property service (the broker's own writer)
    let base = std::fs::read_to_string(out.join(crate::host_art::BUILD_PROP))?;
    let want_sdk = prop_of(&base, SDK_PROP).context("build.prop has no ro.build.version.sdk")?;
    let build_prop = test.join("build.prop");
    std::fs::write(
        &build_prop,
        format!("{base}\n{LISTEN_PROP}=tcp:localhost:{PORT}\n"),
    )?;
    let service = crate::host_art::build_property_service(root, &out.join("cargo"))?;
    let area = test.join("properties");
    let status = Command::new(&service)
        .arg("--build-prop")
        .arg(&build_prop)
        .arg("--out")
        .arg(&area)
        .status()?;
    ensure!(status.success(), "atl-property-service failed ({status})");

    let view = test.join("view");
    device_view(&view, &device, &keys)?;
    let (b, mut binderd) = crate::binder::start_binderd(root, &out, &test.join("binderd.log"))?;
    let mut cmd = Command::new(&adbd);
    cmd.envs(lib_env(&out)?).envs(INIT_ENV.iter().copied());
    let mut s: Spawn = spawn_of(&cmd, adb::AID_SHELL)?;
    for d in ["system", "data"] {
        s.mounts.push(Mount {
            host: view.join(d),
            at: PathBuf::from("/").join(d),
            writable: false,
        });
        s.mounts
            .extend(atl_broker::tree_mounts(&view.join(d))?.into_iter().skip(1));
    }
    s.links.push(Link {
        target: SYSTEM_BIN.into(),
        at: "/bin".into(),
    });
    s.property_area = Some(area);
    let adbd_log = test.join("adbd.log");
    (s.stdout, s.stderr) = to_log(std::fs::File::create(&adbd_log)?)?;
    let maps = adb::host_maps()?;
    let mut p = b.spawn_adbd(s, &maps).context("starting adbd")?;

    let mut fails = Vec::new();
    let start = Instant::now();
    let found = loop {
        let mut l = listeners(&std::fs::read_to_string("/proc/net/tcp")?, PORT);
        l.extend(listeners(&std::fs::read_to_string("/proc/net/tcp6")?, PORT));
        if !l.is_empty() || start.elapsed() > START_LIMIT {
            break l;
        }
        if let Some(st) = p.child.try_wait()? {
            bail!(
                "adbd exited {st} before listening:\n{}",
                std::fs::read_to_string(&adbd_log)?
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if found != [loopback(PORT)] {
        fails.push(format!(
            "listeners on port {PORT}: {found:?}, want only 127.0.0.1"
        ));
    }
    let serial = format!("127.0.0.1:{PORT}");
    let (conn, _) = client(&adb_client, &home, server_port, &["connect", &serial])?;
    println!("adb: connect: {}", conn.trim());
    let (sdk, sdk_ok) = client(
        &adb_client,
        &home,
        server_port,
        &["-s", &serial, "shell", "getprop", SDK_PROP],
    )?;
    let (id, _) = client(
        &adb_client,
        &home,
        server_port,
        &["-s", &serial, "shell", "id"],
    )?;
    let (devices, _) = client(&adb_client, &home, server_port, &["devices"])?;
    println!(
        "adb: getprop {SDK_PROP}: {}\nadb: id: {}\nadb: devices: {}",
        sdk.trim(),
        id.trim(),
        devices.trim()
    );
    if args.break_auth {
        if !devices.contains("unauthorized") || sdk.trim() == want_sdk {
            fails.push(format!("without adb_keys: devices {devices:?}, getprop {sdk:?}; want unauthorized and no answer"));
        }
    } else {
        if !sdk_ok || sdk.trim() != want_sdk {
            fails.push(format!(
                "getprop {SDK_PROP} = {:?}, want {want_sdk:?} (build.prop)",
                sdk.trim()
            ));
        }
        if !id.contains("uid=2000(shell)") || SHELL_GROUPS.iter().any(|g| !id.contains(g)) {
            fails.push(format!(
                "adb shell id = {:?}, want uid 2000 with {SHELL_GROUPS:?}",
                id.trim()
            ));
        }
    }
    for f in &fails {
        println!("adb: FAIL {f}");
    }
    let pass = fails.is_empty();
    if pass {
        println!(
            "adb: pass ({} via AOSP adb -> adbd on 127.0.0.1:{PORT})",
            if args.break_auth {
                "unauthorized without adb_keys"
            } else {
                "getprop + id"
            }
        );
    } else {
        println!("adbd log:\n{}", std::fs::read_to_string(&adbd_log)?);
        println!("{}", crate::verdict::gate("adb", &fails));
    }
    if let Some(secs) = args.serve {
        println!("adb: serving on {serial} for {secs} s");
        let _ = wait_limited(&mut p.child, Duration::from_secs(secs))?;
    }
    let _ = client(&adb_client, &home, server_port, &["kill-server"]);
    let _ = atl_broker::kill_sandbox(p.host_pid);
    let _ = p.child.wait();
    drop(b);
    let _ = wait_limited(&mut binderd.child, RUN_LIMIT)?;
    Ok(if pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// /proc/net/tcp rows (proc(5) layout): a 127.0.0.1:5555 listener, a 0.0.0.0:5555 listener, an established 5555 connection; only LISTEN rows count.
    #[test]
    fn listeners_read_proc_net_tcp() {
        let t = "  sl  local_address rem_address   st\n   0: 0100007F:15B3 00000000:0000 0A 0\n   1: 00000000:15B3 00000000:0000 0A 0\n   2: 0100007F:15B3 0100007F:9C40 01 0\n";
        // 5555 = 0x15B3
        assert_eq!(listeners(t, 5555), ["0100007F:15B3", "00000000:15B3"]);
        assert_eq!(loopback(5555), "0100007F:15B3");
    }

    #[test]
    fn prop_of_takes_the_last_assignment() {
        assert_eq!(
            prop_of(
                "a=1\nro.build.version.sdk = 35\nro.build.version.sdk=36\n",
                SDK_PROP
            )
            .as_deref(),
            Some("36")
        );
        assert_eq!(prop_of("a=1\n", SDK_PROP), None);
    }
}
