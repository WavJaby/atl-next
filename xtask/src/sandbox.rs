// SPDX-License-Identifier: MIT
//! `xtask test sandbox`: what an app process can reach (plan Q5). atl-sandbox-probe runs as uid 10123 through the broker's App profile, as an app does (bwrap, the stub's Landlock grants, the app seccomp filter).
//! Its world: a file in the real $HOME it must not read, a name in $HOME it must not create, and its own data dir (/data/user/0/<PACKAGE>) as its only writable mount.
//! Pass: the $HOME file and the new $HOME name refused (ENOENT or EACCES), a write to /usr refused, its own data dir written and read back.
//! Deliberate break, by hand: restore the writable host root in atl-broker `host_root` (every host dir bound rw); `home-read` must then fail. The gate removes the two $HOME names it makes.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use atl_android_paths::DATA;
use atl_broker::{Broker, Mount, to_log};

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

/// The app uid the probe runs as: AID_APP_START + 123, as app.rs's APP_UID.
const APP_UID: u32 = 10_123;
/// The probe's package: its data dir /data/user/0/<PACKAGE> is the only writable mount it gets.
const PACKAGE: &str = "atl.sandbox.probe";
/// A path under the app root's read-only `usr`: the probe's write there must fail.
const OUTSIDE: &str = "/usr/atl-sandbox-probe";
/// How long the probe may run: a wedged probe fails the gate, it does not hang it.
const LIMIT: Duration = Duration::from_secs(60);
/// errno values (asm-generic/errno-base.h) a refusal may carry.
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EACCES: i32 = 13;
const EROFS: i32 = 30;
/// The home checks: the path is not shown (ENOENT) or Landlock refuses it (EACCES).
const HOME_REFUSED: &[i32] = &[ENOENT, EACCES];
/// The outside write: refused by the mount (EROFS), the grant (EACCES), or the mode (EPERM).
const WRITE_REFUSED: &[i32] = &[EPERM, EACCES, EROFS];

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host)?;
    let work = out.join("sandbox");
    fs::create_dir_all(&work)?;
    let (broker, _binderd) = binder::start_binderd(root, &out, &work.join("binderd.log"))?;
    let status = crate::cargo_command()
        .args(["build", "--release", "-p", "atl-sandbox-probe", "--target-dir"])
        .arg(out.join("cargo"))
        .current_dir(root)
        .status()?;
    ensure!(status.success(), "building atl-sandbox-probe failed");
    let probe = out.join("cargo/release/atl-sandbox-probe");
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let pid = std::process::id();
    let secret = home.join(format!(".atl-sandbox-probe-{pid}"));
    let created = home.join(format!(".atl-sandbox-probe-new-{pid}"));
    fs::write(&secret, b"the real $HOME: the app sandbox must not show this")?;
    let run = probe_once(&broker, &probe, &work, &secret, &created);
    // the gate's own names in $HOME, whatever the probe did
    let _ = fs::remove_file(&secret);
    let _ = fs::remove_file(&created);
    let (line, fails) = run?;
    println!("{}", verdict::gate("sandbox", &fails));
    if !fails.is_empty() {
        println!("{line}");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// One probe run as an app process: its checks' line (`probe: ...`) and the failures the judgement finds.
fn probe_once(
    broker: &Broker,
    probe: &Path,
    work: &Path,
    secret: &Path,
    created: &Path,
) -> Result<(String, Vec<String>)> {
    let own_dir = format!("{DATA}/user/0/{PACKAGE}");
    let own = format!("{own_dir}/probe.txt");
    let host_own = work.join("own");
    fs::create_dir_all(&host_own)?;
    let mut cmd = Command::new(probe);
    cmd.arg(secret).arg(created).arg(OUTSIDE).arg(&own);
    let mut spawn = binder::spawn_of(&cmd, APP_UID)?;
    spawn.mounts.push(Mount {
        host: host_own,
        at: own_dir.into(),
        writable: true,
    });
    let log = work.join("probe.log");
    (spawn.stdout, spawn.stderr) = to_log(File::create(&log)?)?;
    let mut p = broker.spawn(spawn).context("starting the probe")?;
    let status = binder::wait_limited(&mut p.child, LIMIT)?;
    let text = fs::read_to_string(&log)?;
    let line = text
        .lines()
        .find(|l| l.starts_with("probe: "))
        .unwrap_or("probe: no line in the probe's log")
        .to_owned();
    let mut fails = Vec::new();
    match status {
        Some(s) if s.success() => {}
        Some(s) => fails.push(format!("probe exited {s}")),
        None => fails.push(format!("probe still running after {LIMIT:?}, killed")),
    }
    let checks: BTreeMap<&str, &str> = line
        .strip_prefix("probe: ")
        .unwrap_or("")
        .split(' ')
        .filter_map(|kv| kv.split_once('='))
        .collect();
    let refused = |key: &str, errnos: &[i32], what: &str| -> Option<String> {
        let got = checks.get(key).copied().unwrap_or("missing");
        let ok = errnos
            .iter()
            .any(|e| got == format!("err:{e}").as_str());
        (!ok).then(|| format!("{key}={got}: {what} (want err:{errnos:?})"))
    };
    let succeeded = |key: &str, what: &str| -> Option<String> {
        let got = checks.get(key).copied().unwrap_or("missing");
        (got != "ok").then(|| format!("{key}={got}: {what}"))
    };
    fails.extend(refused(
        "home-read",
        HOME_REFUSED,
        "the real $HOME file is readable from the app sandbox",
    ));
    fails.extend(refused(
        "home-create",
        HOME_REFUSED,
        "a new file in the real $HOME is creatable from the app sandbox",
    ));
    fails.extend(refused(
        "outside-write",
        WRITE_REFUSED,
        "a write to /usr is not refused",
    ));
    fails.extend(succeeded("own-write", "the app's own data dir is not writable"));
    fails.extend(succeeded("own-read", "the app's own data dir is not readable"));
    Ok((line, fails))
}
