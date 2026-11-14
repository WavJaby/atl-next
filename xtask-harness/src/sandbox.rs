// SPDX-License-Identifier: MIT
//! `xtask test sandbox`: what an app process can reach (plan Q5). atl-sandbox-probe runs as uid 10123 through the broker's App profile, as an app does (bwrap, the stub's Landlock grants, the app seccomp filter), twice: without INTERNET, then with it (rights from atl_broker::rights_of).
//! Its world: a file in the real $HOME it must not read, a name in $HOME it must not create, its own data dir (/data/user/0/<PACKAGE>) as its only writable mount, and a TCP connect to a loopback listener the gate holds (the sandbox shares the host's loopback).
//! Pass, both runs: the $HOME file and the new $HOME name refused (ENOENT or EACCES), a write to /usr refused, the own data dir written and read back. The TCP connect is refused without INTERNET and succeeds with it.
//! The UI baseline grants (each a grant class every UI app gets, not a manifest right), each with one shown and one refused path: the compositor socket (atl_platform_host::app_access::socket_of; a second socket beside it refused), the fontconfig file (config_file_of; a file beside it refused),
//! the DRM node's sysfs (atl_broker::drm_sysfs over /dev/dri: the device dir's `uevent` read, `/sys/kernel/uevent_seqnum` refused; the CPU topology dir every sandbox shows is judged by `test app`'s cpuProbe row). Font dirs are not probed: the face dirs fontconfig lists are bound the same way as the file.
//! The TCP judgement needs Landlock ABI >= 4 (the network rights): below it the gate prints the net check as not judged and counts nothing for it. WSL is ABI 3, so the net branch is asserted on the Arch laptop only.
//! The resolver (judge_resolv): with INTERNET the probe reads the host's /etc/resolv.conf through whatever link chain the host has (WSL `/mnt/wsl/resolv.conf`, systemd-resolved `/run/...`); a dangling link reads ENOENT and fails it.
//! The environment (judge_env): only atl-broker app_env::APP_ENV names, the spawn's own and the sandbox's fixed ones; the broker's HOME must not arrive.
//! Deliberate breaks, by hand: restore the writable host root in atl-broker `host_root` (every host dir bound rw); `home-read` must then fail. Drop `env_clear` in atl-broker `launch`; the `env:` checks must then fail. The gate removes the two $HOME names it makes.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use atl_android_paths::DATA;
use atl_broker::app_env::APP_ENV;
use atl_broker::{Broker, CONTROL_ENV, Mount, RESOLV_CONF, drm_sysfs, rights_of, to_log};
use atl_platform_host::app_access;

use crate::binder;
use crate::verdict;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// The gate's output dir (default: the host's gate dir).
    #[arg(long)]
    out: Option<PathBuf>,
}

/// The app uid the probe runs as: AID_APP_START + 123, any app id (no atld run, no allocator).
const APP_UID: u32 = 10_123;
/// The probe's package: its data dir /data/user/0/<PACKAGE> is the only writable mount it gets.
const PACKAGE: &str = "atl.sandbox.probe";
/// A path under the app root's read-only `usr`: the probe's write there must fail.
const OUTSIDE: &str = "/usr/atl-sandbox-probe";
/// How long the probe may run: a wedged probe fails the gate, it does not hang it.
const LIMIT: Duration = Duration::from_secs(60);
/// The permission of the INTERNET run (its row in atl_broker's PERMISSION_RIGHTS).
const INTERNET: &str = "android.permission.INTERNET";
/// The DRM nodes the probe gets, as atld's app processes do (atld DRM_DIR).
const DRM_DIR: &str = "/dev/dri";
/// A host sysfs file outside every DRM device's dir and outside atl_broker::CPU_SYSFS: the probe must not read it.
const SYS_OTHER: &str = "/sys/kernel/uevent_seqnum";
/// The Landlock ABI that enforces the network rights; below it TCP is not restricted, so the net check is not judged.
const NET_ABI: u32 = 4;
/// errno values (asm-generic/errno-base.h) a refusal may carry.
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EACCES: i32 = 13;
const EROFS: i32 = 30;
/// The home checks: the path is not shown (ENOENT) or Landlock refuses it (EACCES).
const HOME_REFUSED: &[i32] = &[ENOENT, EACCES];
/// The outside write: refused by the mount (EROFS), the grant (EACCES), or the mode (EPERM).
const WRITE_REFUSED: &[i32] = &[EPERM, EACCES, EROFS];

/// The variable the gate puts in the probe's spawn env: the explicit route must still work.
const EXPLICIT_ENV: &str = "ATL_SANDBOX_PROBE_EXPLICIT";
/// A variable of the broker's own environment that is not the desktop's to give: the real home path. The gate reads HOME itself, so it is always set here.
const SENTINEL_ENV: &str = "HOME";
/// The variable bwrap itself sets in the sandbox (bubblewrap.c, after --chdir).
const BWRAP_PWD: &str = "PWD";

/// One probe run: its checks' line, the Landlock ABI its stub reported (None: no line), and the failures the judgement finds.
struct Probed {
    line: String,
    abi: Option<u32>,
    fails: Vec<String>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, "sandbox")?;
    let work = out.join("sandbox");
    fs::create_dir_all(&work)?;
    let (broker, _binderd) = binder::start_binderd(root, &out, &work.join("binderd.log"))?;
    let status = crate::unit_use::status(
        crate::cargo_command()
            .args([
                "build",
                "--release",
                "-p",
                "atl-sandbox-probe",
                "--target-dir",
            ])
            .arg(out.join("cargo"))
            .current_dir(root),
    )?;
    ensure!(status.success(), "building atl-sandbox-probe failed");
    let probe = out.join("cargo/release/atl-sandbox-probe");
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let pid = std::process::id();
    let secret = home.join(format!(".atl-sandbox-probe-{pid}"));
    let created = home.join(format!(".atl-sandbox-probe-new-{pid}"));
    fs::write(
        &secret,
        b"the real $HOME: the app sandbox must not show this",
    )?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let ui = Ui::make(&work.join("ui"))?;
    let runs = [
        (
            "no INTERNET",
            probe_once(&broker, &probe, &work, &secret, &created, port, &ui, false),
        ),
        (
            "INTERNET",
            probe_once(&broker, &probe, &work, &secret, &created, port, &ui, true),
        ),
    ];
    drop(ui);
    // the gate's own names in $HOME and its listener, whatever the probes did
    let _ = fs::remove_file(&secret);
    let _ = fs::remove_file(&created);
    drop(listener);
    let mut fails = Vec::new();
    let mut lines = Vec::new();
    for (tag, run) in runs {
        let p = run?;
        match p.abi {
            Some(a) if a >= NET_ABI => {}
            Some(a) => println!(
                "sandbox: [{tag}] net not judged: landlock abi {a} < {NET_ABI}, TCP is not restricted here"
            ),
            None => fails.push(format!("[{tag}] no landlock line in the probe's log")),
        }
        fails.extend(p.fails.iter().map(|f| format!("[{tag}] {f}")));
        lines.push(format!("[{tag}] {}", p.line));
    }
    println!("{}", verdict::gate("sandbox", &fails));
    if !fails.is_empty() {
        for l in &lines {
            println!("{l}");
        }
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// The UI grants' world: two listening sockets (one granted as the compositor's), a fontconfig file and a file beside it, and the DRM sysfs the probe's nodes give it.
struct Ui {
    socket: PathBuf,
    other_socket: PathBuf,
    fontconf: PathBuf,
    sibling: PathBuf,
    sysfs: PathBuf,
    /// The grants, as atld makes them for a `--ui` app (atld desktop_mounts): read-only at their own paths.
    mounts: Vec<Mount>,
    _listeners: [UnixListener; 2],
}

impl Ui {
    fn make(dir: &Path) -> Result<Ui> {
        let _ = fs::remove_dir_all(dir);
        fs::create_dir_all(dir)?;
        let socket = dir.join("wayland-probe");
        let other_socket = dir.join("other-probe");
        let listeners = [
            UnixListener::bind(&socket)?,
            UnixListener::bind(&other_socket)?,
        ];
        let fontconf = dir.join("fonts.conf");
        let sibling = dir.join("fonts-sibling.conf");
        fs::write(&fontconf, b"<fontconfig/>\n")?;
        fs::write(&sibling, b"<fontconfig/>\n")?;
        let granted_socket = app_access::socket_of(Some(socket.clone().into()), None)
            .context("socket_of refused the gate's socket")?;
        let granted_conf = app_access::config_file_of(Some(fontconf.clone().into()))
            .context("config_file_of refused the gate's file")?;
        let (sys_mounts, _) = drm_sysfs(&[PathBuf::from(DRM_DIR)])?;
        let sysfs = sys_mounts
            .first()
            .map(|m| m.host.join("uevent"))
            .context("no DRM device dir under /sys for /dev/dri")?;
        let mounts = [granted_socket, granted_conf]
            .into_iter()
            .map(|p| Mount {
                host: p.clone(),
                at: p,
                writable: false,
            })
            .collect();
        Ok(Ui {
            socket,
            other_socket,
            fontconf,
            sibling,
            sysfs,
            mounts,
            _listeners: listeners,
        })
    }
}

/// One probe run as an app process, with or without INTERNET's rights.
#[expect(
    clippy::too_many_arguments,
    reason = "one run's world, each a distinct check input"
)]
fn probe_once(
    broker: &Broker,
    probe: &Path,
    work: &Path,
    secret: &Path,
    created: &Path,
    port: u16,
    ui: &Ui,
    internet: bool,
) -> Result<Probed> {
    let own_dir = format!("{DATA}/user/0/{PACKAGE}");
    let own = format!("{own_dir}/probe.txt");
    let host_own = work.join("own");
    fs::create_dir_all(&host_own)?;
    let mut cmd = Command::new(probe);
    cmd.arg(secret)
        .arg(created)
        .arg(OUTSIDE)
        .arg(&own)
        .arg(port.to_string())
        .arg(&ui.socket)
        .arg(&ui.other_socket)
        .arg(&ui.fontconf)
        .arg(&ui.sibling)
        .arg(&ui.sysfs)
        .arg(SYS_OTHER)
        .arg(RESOLV_CONF);
    let mut spawn = binder::spawn_of(&cmd, APP_UID)?;
    spawn.env.push((EXPLICIT_ENV.into(), "1".into()));
    spawn.mounts.push(Mount {
        host: host_own,
        at: own_dir.into(),
        writable: true,
    });
    spawn.mounts.extend(ui.mounts.iter().cloned());
    spawn.devices = vec![PathBuf::from(DRM_DIR)];
    // the rights come from the manifest permissions, as the app process's own do (atl-broker PERMISSION_RIGHTS)
    spawn.rights = if internet {
        rights_of(&[INTERNET])
    } else {
        rights_of::<&str>(&[])
    };
    let log = work.join(if internet {
        "probe-internet.log"
    } else {
        "probe.log"
    });
    (spawn.stdout, spawn.stderr) = to_log(File::create(&log)?)?;
    let mut p = broker.spawn(spawn).context("starting the probe")?;
    let status = binder::wait_limited(&mut p.child, LIMIT)?;
    let text = fs::read_to_string(&log)?;
    let line = text
        .lines()
        .find(|l| l.starts_with("probe: "))
        .unwrap_or("probe: no line in the probe's log")
        .to_owned();
    let abi = text
        .lines()
        .find_map(|l| l.split("landlock abi=").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<u32>().ok());
    let mut fails = Vec::new();
    match status {
        Some(s) if s.success() => {}
        Some(s) => fails.push(format!("probe exited {s}")),
        None => fails.push(format!("probe still running after {LIMIT:?}, killed")),
    }
    fails.extend(judge(&line, internet, abi));
    if internet {
        fails.extend(judge_resolv(&line, &host_resolv()));
    }
    fails.extend(judge_env(&line, |k| std::env::var_os(k).is_some()));
    Ok(Probed { line, abi, fails })
}

/// The probe's environment against this process's (the broker's host, as atld is): HOME (set here, the run needs it) must not reach the app; every name must be an app_env::APP_ENV name, the spawn's own ([`EXPLICIT_ENV`], which must arrive: the probe reports env at all), the binder fd variable, or bwrap's PWD;
/// and every APP_ENV name `host_has` must arrive.
fn judge_env(line: &str, host_has: impl Fn(&str) -> bool) -> Vec<String> {
    let names: Vec<&str> = line
        .split(' ')
        .find_map(|kv| kv.strip_prefix("env="))
        .map(|v| v.split(',').filter(|n| !n.is_empty()).collect())
        .unwrap_or_default();
    let mut fails = Vec::new();
    if names.contains(&SENTINEL_ENV) {
        fails.push(format!(
            "env: {SENTINEL_ENV} from the broker's environment reached the app sandbox"
        ));
    }
    if !names.contains(&EXPLICIT_ENV) {
        fails.push(format!(
            "env: the spawn's own {EXPLICIT_ENV} did not arrive (env={})",
            names.join(",")
        ));
    }
    let stray: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !APP_ENV.contains(n) && ![EXPLICIT_ENV, CONTROL_ENV, BWRAP_PWD].contains(n))
        .collect();
    if !stray.is_empty() {
        fails.push(format!(
            "env: names outside the allow-list reached the app: {}",
            stray.join(",")
        ));
    }
    let lost: Vec<&str> = APP_ENV
        .iter()
        .copied()
        .filter(|n| host_has(n) && !names.contains(n))
        .collect();
    if !lost.is_empty() {
        fails.push(format!(
            "env: allow-listed names set here did not arrive: {}",
            lost.join(",")
        ));
    }
    fails
}

/// What the probe's `resolv` check must read with INTERNET: what this process reads at atl_broker::RESOLV_CONF on the host, as `ok:<first nameserver>` (`ok:none` without one), else `err:<errno>` (no resolver file on the host: none in the app either).
/// The first nameserver is resolv.conf's first `nameserver <address>` line (glibc res_init's keyword), parsed here, not by the probe's code.
fn host_resolv() -> String {
    match fs::read_to_string(RESOLV_CONF) {
        Ok(text) => {
            let ns = text
                .lines()
                .map(str::split_whitespace)
                .find_map(|mut w| (w.next() == Some("nameserver")).then(|| w.next()).flatten());
            format!("ok:{}", ns.unwrap_or("none"))
        }
        Err(e) => format!("err:{}", e.raw_os_error().unwrap_or(0)),
    }
}

/// The INTERNET run's resolver view against the host's ([`host_resolv`]): an app holding INTERNET resolves through the host's resolver, so it must read the same file, wherever /etc links it (systemd-resolved, WSL).
/// Not judged without INTERNET: a resolv.conf that is a plain file in /etc is shown with the rest of /etc, and the separate network namespace that would hide it (plan Q5 stage v2) is not built.
fn judge_resolv(line: &str, want: &str) -> Option<String> {
    let got = line
        .split(' ')
        .find_map(|kv| kv.strip_prefix("resolv="))
        .unwrap_or("missing");
    (got != want).then(|| {
        format!("resolv={got}: the app's resolver file differs from the host's (want {want})")
    })
}

/// The verdict on one probe line: the file checks in every run; the TCP connect by the run's rights, judged only where Landlock enforces the network rights (ABI >= NET_ABI).
fn judge(line: &str, internet: bool, abi: Option<u32>) -> Vec<String> {
    let checks: BTreeMap<&str, &str> = line
        .strip_prefix("probe: ")
        .unwrap_or("")
        .split(' ')
        .filter_map(|kv| kv.split_once('='))
        .collect();
    let got = |key: &str| checks.get(key).copied().unwrap_or("missing").to_owned();
    let refused = |key: &str, errnos: &[i32], what: &str| -> Option<String> {
        let g = got(key);
        let ok = errnos.iter().any(|e| g == format!("err:{e}"));
        (!ok).then(|| format!("{key}={g}: {what} (want err:{errnos:?})"))
    };
    let succeeded = |key: &str, what: &str| -> Option<String> {
        let g = got(key);
        (g != "ok").then(|| format!("{key}={g}: {what}"))
    };
    let mut fails: Vec<String> = [
        refused(
            "home-read",
            HOME_REFUSED,
            "the real $HOME file is readable from the app sandbox",
        ),
        refused(
            "home-create",
            HOME_REFUSED,
            "a new file in the real $HOME is creatable from the app sandbox",
        ),
        refused(
            "outside-write",
            WRITE_REFUSED,
            "a write to /usr is not refused",
        ),
        succeeded("own-write", "the app's own data dir is not writable"),
        succeeded("own-read", "the app's own data dir is not readable"),
        succeeded("socket", "the granted compositor socket does not connect"),
        refused(
            "socket-other",
            HOME_REFUSED,
            "a host socket beside the compositor's connects",
        ),
        succeeded("fontconf", "the granted fontconfig file is not readable"),
        refused(
            "fontconf-sibling",
            HOME_REFUSED,
            "a file beside the fontconfig file is readable",
        ),
        succeeded("drm-sysfs", "the DRM device's sysfs is not readable"),
        refused(
            "sys-other",
            HOME_REFUSED,
            "sysfs outside the DRM device is readable",
        ),
    ]
    .into_iter()
    .flatten()
    .collect();
    if abi.is_some_and(|a| a >= NET_ABI) {
        fails.extend(if internet {
            succeeded("net-connect", "TCP connect refused with INTERNET")
        } else {
            refused(
                "net-connect",
                &[EACCES],
                "TCP connect not refused without INTERNET",
            )
        });
    }
    fails
}
