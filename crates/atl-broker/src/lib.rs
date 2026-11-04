// SPDX-License-Identifier: MIT
//! atl-broker: the one launcher of sandboxed processes (plan Q5). Each process gets its binderd control connection, registered tagged {uid, host pid}.
//! M2 depth (owner Q4(a), 2026-10-31): bubblewrap with a user namespace mapping the process's uid and gid, a pid namespace, the host's root (Landlock narrows it in a later Q5 stage)
//! with the caller's mounts over it (`/system`, `/data`, ...), a fresh `/dev` holding the property area at bionic's default path; Landlock and seccomp come with the Q5 stages.
//!
//! Tag pid: the host pid bwrap reports on `--info-fd` (`child-pid`), the namespace's first process, which the broker owns (killing it ends the sandbox: `--unshare-pid` makes it pid 1).
//! Spawns are serialised by a lock in [`Broker`]: the control and info fds are made inheritable just before the spawn and closed right after it, and a concurrent spawn would inherit them.
//! [`control`] is how the system services ask atld (which owns the broker) to start and kill processes.
#![forbid(unsafe_code)]

pub mod control;

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use rustix::process::{Pid, Signal, kill_process};

/// libbinder's driver-fd variable (atl-binder-driver `CONTROL_ENV`).
pub const CONTROL_ENV: &str = "ATL_BINDER_FD";
/// bionic's default property area path (`__system_properties_init` without `$ATL_PROP_AREA`): the sandbox's `/dev` is the broker's own, so the area is mounted there and no variable is needed.
pub const PROPERTY_AREA: &str = "/dev/__properties__";
/// The sandbox program.
const BWRAP: &str = "bwrap";

/// One bind mount into the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    /// Absolute path inside the sandbox.
    pub at: PathBuf,
    pub writable: bool,
}

/// What to start.
#[derive(Debug)]
pub struct Spawn {
    /// The process's uid and gid inside its user namespace, and its binderd tag.
    pub uid: u32,
    /// May become binder's context manager (servicemanager only).
    pub context_manager: bool,
    pub mounts: Vec<Mount>,
    /// Host device nodes (or directories of them) the process may open, at the same path in the sandbox, whose `/dev` is otherwise a fresh minimal one (the graphics allocator's `/dev/dri`).
    pub devices: Vec<PathBuf>,
    /// The property area file, mounted read-only at [`PROPERTY_AREA`].
    pub property_area: Option<PathBuf>,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    /// The process's fd 0; None = /dev/null. The system services get their [`control`] connection here (an inherited fd would need `unsafe` to adopt; stdin does not).
    pub stdin: Option<OwnedFd>,
    /// stdout and stderr (one file: a log).
    pub log: File,
}

/// A started sandbox.
#[derive(Debug)]
pub struct Process {
    /// bwrap, the broker's child.
    pub child: Child,
    /// The tag binderd reports as the calling pid.
    pub host_pid: i32,
}

/// An entry of the host's `/` and how the sandbox shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootEntry {
    Dir(String),
    Symlink { name: String, target: PathBuf },
}

/// The host's `/` (directories and symlinks; files at the root are not shown).
pub fn host_root() -> io::Result<Vec<RootEntry>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir("/")? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let ft = e.file_type()?;
        if ft.is_symlink() {
            v.push(RootEntry::Symlink {
                target: std::fs::read_link(e.path())?,
                name,
            });
        } else if ft.is_dir() {
            v.push(RootEntry::Dir(name));
        }
    }
    v.sort_by(|a, b| key(a).cmp(key(b)));
    Ok(v)
}

fn key(e: &RootEntry) -> &str {
    match e {
        RootEntry::Dir(n) | RootEntry::Symlink { name: n, .. } => n,
    }
}

/// bwrap's arguments for `spawn` over `root`, info on fd `info_fd`, up to and including `--`.
/// A root entry a mount covers (its first path component) is left out, as are `/dev` and `/proc`, which the sandbox gets fresh.
pub fn bwrap_args(spawn: &Spawn, root: &[RootEntry], info_fd: i32) -> Vec<OsString> {
    let uid = spawn.uid.to_string();
    let mut a: Vec<OsString> = [
        "--unshare-user",
        "--uid",
        &uid,
        "--gid",
        &uid,
        "--unshare-pid",
        "--die-with-parent",
        "--info-fd",
        &info_fd.to_string(),
        "--tmpfs",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let covered = |name: &str| {
        name == "dev"
            || name == "proc"
            || spawn.mounts.iter().any(|m| {
                m.at.components()
                    .nth(1)
                    .is_some_and(|c| c.as_os_str() == name)
            })
    };
    for e in root {
        match e {
            RootEntry::Dir(n) if !covered(n) => {
                let p = format!("/{n}");
                a.extend(["--bind".into(), p.clone().into(), p.into()]);
            }
            RootEntry::Symlink { name, target } if !covered(name) => {
                a.extend([
                    "--symlink".into(),
                    target.clone().into_os_string(),
                    format!("/{name}").into(),
                ]);
            }
            _ => {}
        }
    }
    for m in &spawn.mounts {
        let kind = if m.writable { "--bind" } else { "--ro-bind" };
        a.extend([
            kind.into(),
            m.host.clone().into_os_string(),
            m.at.clone().into_os_string(),
        ]);
    }
    for d in &spawn.devices {
        a.extend([
            "--dev-bind".into(),
            d.clone().into_os_string(),
            d.clone().into_os_string(),
        ]);
    }
    if let Some(area) = &spawn.property_area {
        a.extend([
            "--ro-bind".into(),
            area.clone().into_os_string(),
            PROPERTY_AREA.into(),
        ]);
    }
    a.push("--".into());
    a
}

/// `child-pid` of bwrap's `--info-fd` JSON (`{"child-pid": N, ...}`, bwrap.c).
pub fn child_pid(info: &[u8]) -> Option<i32> {
    let v: serde_json::Value = serde_json::from_slice(info).ok()?;
    i32::try_from(v.get("child-pid")?.as_i64()?).ok()
}

/// Kills the sandbox whose first process is `host_pid` (a [`Process::host_pid`]): that process is pid 1 of the sandbox's pid namespace, so every process in it dies with it.
pub fn kill_sandbox(host_pid: i32) -> io::Result<()> {
    let pid =
        Pid::from_raw(host_pid).ok_or_else(|| other(format!("{host_pid}: not a process id")))?;
    kill_process(pid, Signal::KILL).map_err(io::Error::from)
}

/// The broker's side of binderd: its admin connection. Dropping it closes the connection, which ends binderd.
pub struct Broker {
    admin: OwnedFd,
    /// Held across a spawn: the fds made inheritable for it must not reach another spawn's child.
    spawning: Mutex<()>,
}

fn other(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

impl Broker {
    pub fn new(admin: OwnedFd) -> Self {
        Broker {
            admin,
            spawning: Mutex::new(()),
        }
    }

    /// Starts `spawn` in its sandbox and registers its control connection with binderd once bwrap reports the host pid.
    pub fn spawn(&self, spawn: Spawn) -> io::Result<Process> {
        // a panic elsewhere leaves nothing half-done here: the guard only orders spawns
        let _one_at_a_time = self.spawning.lock().unwrap_or_else(|e| e.into_inner());
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        // std's pipe ends are close-on-exec
        let (info_r, info_w) = io::pipe()?;
        // inherited by this child only: cleared just before the spawn, closed right after it
        fcntl_setfd(&theirs, FdFlags::empty())?;
        fcntl_setfd(&info_w, FdFlags::empty())?;
        let args = bwrap_args(&spawn, &host_root()?, info_w.as_raw_fd());
        let mut cmd = Command::new(BWRAP);
        cmd.args(&args)
            .arg(&spawn.program)
            .args(&spawn.args)
            .envs(
                spawn
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_os_str(), v.as_os_str())),
            )
            .env(CONTROL_ENV, theirs.as_raw_fd().to_string())
            .stdin(spawn.stdin.map_or_else(Stdio::null, Stdio::from))
            .stdout(spawn.log.try_clone()?)
            .stderr(spawn.log);
        let child = cmd.spawn();
        drop(theirs);
        drop(info_w);
        let mut child = child.map_err(|e| other(format!("starting {BWRAP}: {e}")))?;
        let host_pid = match read_info(info_r) {
            Ok(pid) => pid,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        atl_binderd::server::register(
            self.admin.as_fd(),
            ours.as_fd(),
            spawn.uid,
            host_pid,
            spawn.context_manager,
        )
        .map_err(|e| other(format!("registering pid {host_pid} with binderd: {e}")))?;
        Ok(Process { child, host_pid })
    }
}

/// Reads bwrap's info until it parses: bwrap writes it once the sandbox's first process exists; EOF before that = bwrap failed (its stderr is in the log).
fn read_info(mut f: io::PipeReader) -> io::Result<i32> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let n = f.read(&mut chunk)?;
        if n == 0 {
            return Err(other(format!(
                "{BWRAP} ended before reporting its child: {:?}",
                OsStr::from_bytes(&buf)
            )));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pid) = child_pid(&buf) {
            return Ok(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn() -> Spawn {
        Spawn {
            uid: 10_123,
            context_manager: false,
            mounts: vec![
                Mount {
                    host: "/r/system".into(),
                    at: "/system".into(),
                    writable: false,
                },
                Mount {
                    host: "/r/data".into(),
                    at: "/data".into(),
                    writable: true,
                },
            ],
            devices: vec!["/dev/dri".into()],
            property_area: Some("/r/properties".into()),
            program: "/bin/true".into(),
            args: vec![],
            env: vec![],
            stdin: None,
            log: tempfile(),
        }
    }

    fn tempfile() -> File {
        let p = std::env::temp_dir().join(format!("atl-broker-test-{}", std::process::id()));
        File::create(p).unwrap()
    }

    fn s(a: &[OsString]) -> Vec<String> {
        a.iter().map(|x| x.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn sandbox_shape() {
        let root = vec![
            RootEntry::Dir("data".into()),
            RootEntry::Dir("dev".into()),
            RootEntry::Dir("home".into()),
            RootEntry::Symlink {
                name: "lib".into(),
                target: "usr/lib".into(),
            },
            RootEntry::Dir("proc".into()),
            RootEntry::Dir("system".into()),
        ];
        let a = s(&bwrap_args(&spawn(), &root, 9));
        let joined = a.join(" ");
        // the uid map and the namespaces Q4(a) names
        assert!(joined.starts_with(
            "--unshare-user --uid 10123 --gid 10123 --unshare-pid --die-with-parent --info-fd 9 "
        ));
        // the host's dirs bound, its symlinks recreated; what the sandbox replaces is not bound
        assert!(joined.contains("--bind /home /home"));
        assert!(joined.contains("--symlink usr/lib /lib"));
        for replaced in ["/data /data", "/dev /dev", "/proc /proc", "/system /system"] {
            assert!(
                !joined.contains(&format!("--bind {replaced}")),
                "{replaced}"
            );
        }
        // the mounts after the root, then the device nodes, then the area at bionic's path, then the end of options
        assert!(joined.ends_with("--ro-bind /r/system /system --bind /r/data /data --dev-bind /dev/dri /dev/dri --ro-bind /r/properties /dev/__properties__ --"));
    }

    #[test]
    fn info_fd() {
        // bwrap.c writes `{\n    "child-pid": N` and, with --unshare-user / --unshare-pid, namespace ids after it
        assert_eq!(
            child_pid(b"{\n    \"child-pid\": 4242,\n    \"cgroup-namespace\": 1\n}\n"),
            Some(4242)
        );
        assert_eq!(child_pid(b"{\n    \"child-pid\": 42"), None);
        assert_eq!(child_pid(b"{}"), None);
    }
}
