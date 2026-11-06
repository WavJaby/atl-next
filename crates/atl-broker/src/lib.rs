// SPDX-License-Identifier: MIT
//! atl-broker: the one launcher of sandboxed processes (plan Q5), binderd included: [`Broker::start`] runs it in [`Profile::BinderDaemon`] and holds its admin connection; every [`Broker::spawn`] gets its binderd control connection, registered tagged {uid, host pid}.
//! M2 depth (owner Q4(a), 2026-10-31): bubblewrap with a user namespace mapping the process's uid and gid, a pid namespace, the host's root (Landlock narrows it in a later Q5 stage)
//! with the caller's mounts over it (`/system`, `/data`, ...), a fresh `/dev` holding the property area at bionic's default path; Landlock comes with the Q5 stages. The app's seccomp filter is [`seccomp`] (bwrap `--seccomp`).
//!
//! Tag pid: the host pid bwrap reports on `--info-fd` (`child-pid`), the namespace's first process, which the broker owns (killing it ends the sandbox: `--unshare-pid` makes it pid 1).
//! Spawns are serialised by a lock in [`Broker`]: the control, info and seccomp fds are made inheritable just before the spawn and closed right after it, and a concurrent spawn would inherit them.
//! [`control`] is how the system services ask atld (which owns the broker) to start and kill processes.
#![forbid(unsafe_code)]

pub mod bwrap;
pub mod control;
mod seccomp;

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use atl_android_paths::PROPERTY_AREA;
use std::process::{Child, Stdio};
use std::sync::Mutex;

use rustix::fs::{MemfdFlags, memfd_create};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use rustix::process::{Pid, Signal, kill_process};

/// libbinder's driver-fd variable (atl-binder-driver `CONTROL_ENV`).
pub const CONTROL_ENV: &str = "ATL_BINDER_FD";
use bwrap::PROGRAM as BWRAP;

/// One bind mount into the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    /// Absolute path inside the sandbox.
    pub at: PathBuf,
    pub writable: bool,
}

/// A symbolic link in the sandbox (bwrap `--symlink TARGET AT`): `at` links to `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub target: PathBuf,
    /// Absolute path inside the sandbox.
    pub at: PathBuf,
}

/// What to start.
#[derive(Debug)]
pub struct Spawn {
    /// The process's uid and gid inside its user namespace, and its binderd tag.
    pub uid: u32,
    /// May become binder's context manager (servicemanager only).
    pub context_manager: bool,
    pub mounts: Vec<Mount>,
    /// Symbolic links made after the mounts (bwrap `--symlink`): the app's `/sdcard`.
    pub links: Vec<Link>,
    /// Host device nodes (or directories of them) the process may open, at the same path in the sandbox, whose `/dev` is otherwise a fresh minimal one (the graphics allocator's `/dev/dri`).
    pub devices: Vec<PathBuf>,
    /// The property area file, mounted read-only at [`PROPERTY_AREA`].
    pub property_area: Option<PathBuf>,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    /// The working directory inside the sandbox (bwrap `--chdir`); None = bwrap's default (the caller's directory when the sandbox shows it).
    pub cwd: Option<PathBuf>,
    /// The process's fd 0; None = /dev/null. The system services get their [`control`] connection here (an inherited fd would need `unsafe` to adopt; stdin does not).
    pub stdin: Option<OwnedFd>,
    pub stdout: Stdio,
    pub stderr: Stdio,
}

/// stdout and stderr both to `log` (one file: a process's log).
pub fn to_log(log: File) -> io::Result<(Stdio, Stdio)> {
    Ok((log.try_clone()?.into(), log.into()))
}

/// What a sandbox is for: selects its namespaces, its filesystem view and its seccomp program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// An Android process ([`Spawn`]): its uid, the host's root with the caller's mounts, registered with binderd.
    App,
    /// binderd itself ([`Broker::start`]): every namespace, no capabilities, a root holding only its executable's ELF closure, the [`seccomp::binderd_filter`] allow-list.
    BinderDaemon,
}

/// The uid and gid binderd runs as inside its user namespace: the overflow id (nobody), which owns nothing binderd could reach.
const BINDERD_ID: u32 = 65534;

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

/// bwrap's arguments for `spawn` over `root`, info on fd `info_fd`, the seccomp program on `seccomp_fd` when given, up to and including `--`.
/// A root entry a mount covers (its first path component) is left out, as are `/dev` and `/proc`, which the sandbox gets fresh.
pub fn bwrap_args(
    spawn: &Spawn,
    root: &[RootEntry],
    info_fd: i32,
    seccomp_fd: Option<i32>,
) -> Vec<OsString> {
    let mut a = bwrap::Args::new();
    a.unshare_user()
        .ids(spawn.uid)
        .unshare_pid()
        .die_with_parent()
        .info_fd(info_fd)
        .tmpfs("/")
        .dev("/dev")
        .proc("/proc");
    if let Some(fd) = seccomp_fd {
        a.seccomp(fd);
    }
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
                a.bind(&p, &p);
            }
            RootEntry::Symlink { name, target } if !covered(name) => {
                a.symlink(target, format!("/{name}"));
            }
            _ => {}
        }
    }
    for m in &spawn.mounts {
        a.mount(&m.host, &m.at, m.writable);
    }
    for l in &spawn.links {
        a.symlink(&l.target, &l.at);
    }
    for d in &spawn.devices {
        a.dev_bind(d, d);
    }
    if let Some(area) = &spawn.property_area {
        a.ro_bind(area, PROPERTY_AREA);
    }
    if let Some(dir) = &spawn.cwd {
        a.chdir(dir);
    }
    a.end()
}

/// bwrap's arguments for binderd ([`Profile::BinderDaemon`]): `files` (its executable's ELF closure, [`elf_closure`]) read-only at their own paths on an empty root, info on `info_fd`, the seccomp program on `seccomp_fd`, up to and including `--`.
/// No /dev, no /proc: binderd's only fds are its admin socket (stdin), stderr, and the sockets it creates or receives.
pub fn binderd_args(files: &[PathBuf], info_fd: i32, seccomp_fd: i32) -> Vec<OsString> {
    let mut a = bwrap::Args::new();
    a.unshare_all()
        .unshare_user()
        .ids(BINDERD_ID)
        .die_with_parent()
        .new_session()
        .cap_drop_all()
        .info_fd(info_fd)
        .seccomp(seccomp_fd)
        .tmpfs("/");
    for f in files {
        a.ro_bind(f, f);
    }
    a.end()
}

/// The files `program` needs to start: itself, its interpreter (PT_INTERP, at the path it names), and every DT_NEEDED library, transitively, found in the interpreter's canonical directory (the loader's default search path holds it; no other is consulted).
/// A library that is not there fails: a sandbox missing it would end binderd at start with only the loader's message.
pub fn elf_closure(program: &Path) -> io::Result<Vec<PathBuf>> {
    let (interp, mut needed) = elf_deps(program)?;
    let interp = interp.ok_or_else(|| other(format!("{}: no PT_INTERP", program.display())))?;
    let dir = std::fs::canonicalize(&interp)?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| other(format!("{}: no directory", interp.display())))?;
    let mut files = vec![program.to_path_buf(), interp];
    while let Some(name) = needed.pop() {
        let lib = dir.join(&name);
        if files.contains(&lib) {
            continue;
        }
        if !lib.is_file() {
            return Err(other(format!(
                "{}: {name} is not in {}",
                program.display(),
                dir.display()
            )));
        }
        needed.extend(elf_deps(&lib)?.1);
        files.push(lib);
    }
    Ok(files)
}

/// `path`'s PT_INTERP and DT_NEEDED names.
fn elf_deps(path: &Path) -> io::Result<(Option<PathBuf>, Vec<String>)> {
    use object::read::elf::{Dyn, ElfFile64, ProgramHeader};
    let data = std::fs::read(path)?;
    let elf = ElfFile64::<object::Endianness>::parse(&*data)
        .map_err(|e| other(format!("{}: {e}", path.display())))?;
    let endian = elf.endian();
    let bad = |e: object::Error| other(format!("{}: {e}", path.display()));
    let mut interp = None;
    for ph in elf.elf_program_headers() {
        if let Some(bytes) = ph.interpreter(endian, &*data).map_err(bad)? {
            interp = Some(PathBuf::from(OsStr::from_bytes(bytes)));
        }
    }
    let mut needed = Vec::new();
    let sections = elf.elf_section_table();
    if let Some((dynamic, link)) = sections.dynamic(endian, &*data).map_err(bad)? {
        let strings = sections.strings(endian, &*data, link).map_err(bad)?;
        for d in dynamic {
            if d.tag(endian) == object::elf::DT_NEEDED {
                let name = d.string(endian, strings).map_err(bad)?;
                needed.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    Ok((interp, needed))
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
    /// Whether apps get the app seccomp filter (seccomp.rs); off only for the `--no-app-seccomp` check.
    app_seccomp: bool,
}

fn other(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

/// One bwrap run: its arguments up to `--`, then the program; `inherit` = the fds the child alone gets (cleared close-on-exec just before the spawn, closed right after it). Returns bwrap and the host pid it reports on `info_r`.
struct Launch<'a> {
    bwrap_args: Vec<OsString>,
    program: &'a Path,
    args: &'a [OsString],
    env: Vec<(OsString, OsString)>,
    clear_env: bool,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
}

fn launch(l: Launch<'_>, info_r: io::PipeReader, inherit: Vec<OwnedFd>) -> io::Result<Process> {
    for fd in &inherit {
        fcntl_setfd(fd, FdFlags::empty())?;
    }
    #[expect(
        clippy::disallowed_types,
        reason = "the one launcher of the runtime: every process atld starts is a bwrap run built here"
    )]
    let mut cmd = std::process::Command::new(BWRAP);
    if l.clear_env {
        cmd.env_clear();
    }
    cmd.args(&l.bwrap_args)
        .arg(l.program)
        .args(l.args)
        .envs(l.env)
        .stdin(l.stdin)
        .stdout(l.stdout)
        .stderr(l.stderr);
    let child = cmd.spawn();
    drop(inherit);
    let mut child = child.map_err(|e| other(format!("starting {BWRAP}: {e}")))?;
    match read_info(info_r) {
        Ok(host_pid) => Ok(Process { child, host_pid }),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

impl Broker {
    /// Starts binderd (`program`) in [`Profile::BinderDaemon`] with its admin connection on stdin and its stderr (counters) to `log`, and the broker over the other end of that connection.
    /// Dropping the Broker closes admin, which ends binderd; the returned process is bwrap, which exits with binderd's status.
    pub fn start(program: &Path, log: File) -> io::Result<(Broker, Process)> {
        let (admin, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        let (info_r, info_w) = io::pipe()?;
        let seccomp = seccomp_memfd(Profile::BinderDaemon)?;
        let files = elf_closure(program)?;
        let bwrap_args = binderd_args(&files, info_w.as_raw_fd(), seccomp.as_raw_fd());
        let process = launch(
            Launch {
                bwrap_args,
                program,
                args: &[],
                env: vec![],
                // binderd reads no variable; the host's would only widen what it sees
                clear_env: true,
                stdin: theirs.into(),
                stdout: Stdio::null(),
                stderr: log.into(),
            },
            info_r,
            vec![info_w.into(), seccomp],
        )
        .map_err(|e| other(format!("starting binderd {}: {e}", program.display())))?;
        Ok((Broker::new(admin), process))
    }

    fn new(admin: OwnedFd) -> Self {
        Broker {
            admin,
            spawning: Mutex::new(()),
            app_seccomp: true,
        }
    }

    /// Apps start without the seccomp filter: the deliberate-failure check that shows what the filter answers (atld `--no-app-seccomp`, test only).
    pub fn without_app_seccomp(mut self) -> Self {
        self.app_seccomp = false;
        self
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
        // the app's seccomp program, read by bwrap from this fd before the program runs
        let seccomp = self
            .app_seccomp
            .then(|| seccomp_memfd(Profile::App))
            .transpose()?;
        let bwrap_args = bwrap_args(
            &spawn,
            &host_root()?,
            info_w.as_raw_fd(),
            seccomp.as_ref().map(AsRawFd::as_raw_fd),
        );
        let mut env = spawn.env;
        env.push((CONTROL_ENV.into(), theirs.as_raw_fd().to_string().into()));
        let mut inherit: Vec<OwnedFd> = vec![theirs, info_w.into()];
        inherit.extend(seccomp);
        let Process { child, host_pid } = launch(
            Launch {
                bwrap_args,
                program: &spawn.program,
                args: &spawn.args,
                env,
                clear_env: false,
                stdin: spawn.stdin.map_or_else(Stdio::null, Stdio::from),
                stdout: spawn.stdout,
                stderr: spawn.stderr,
            },
            info_r,
            inherit,
        )?;
        atl_binderd::server::register(
            self.admin.as_fd(),
            ours.as_fd(),
            spawn.uid,
            host_pid,
            spawn.context_manager,
        )
        .map_err(|e| registration_error(host_pid, e))?;
        Ok(Process { child, host_pid })
    }
}

/// A failed registration of `host_pid`: a closed admin connection (EPIPE / ECONNRESET on the SEQPACKET socket) means binderd has ended -- killed by its seccomp filter, crashed, or exited -- and its log says which.
fn registration_error(host_pid: i32, e: impl Into<io::Error>) -> io::Error {
    let e: io::Error = e.into();
    match e.kind() {
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => other(format!(
            "binderd ended (its admin connection is closed) before pid {host_pid} could register: see binderd's log; a SIGSYS end is its seccomp allow-list"
        )),
        _ => other(format!("registering pid {host_pid} with binderd: {e}")),
    }
}

/// `profile`'s seccomp program in a memfd, rewound: bwrap reads it from `--seccomp`.
fn seccomp_memfd(profile: Profile) -> io::Result<OwnedFd> {
    let mut file = File::from(memfd_create("atl-seccomp", MemfdFlags::CLOEXEC)?);
    file.write_all(&seccomp::filter(profile)?)?;
    file.rewind()?;
    Ok(file.into())
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
    use atl_android_paths::{DATA, SDCARD, SHARED_STORAGE, SYSTEM};

    /// Spawn.links follow the mounts as bwrap `--symlink TARGET AT`: the app's /sdcard -> the shared storage root.
    #[test]
    fn links_follow_the_mounts() {
        let mut sp = spawn();
        sp.links.push(Link {
            target: SHARED_STORAGE.into(),
            at: SDCARD.into(),
        });
        let joined = s(&bwrap_args(&sp, &[], 9, None)).join(" ");
        assert!(joined.ends_with(
            "--ro-bind /r/system /system --bind /r/data /data --symlink /storage/emulated/0 /sdcard --dev-bind /dev/dri /dev/dri --ro-bind /r/properties /dev/__properties__ --"
        ));
    }

    fn spawn() -> Spawn {
        Spawn {
            uid: 10_123,
            context_manager: false,
            mounts: vec![
                Mount {
                    host: "/r/system".into(),
                    at: SYSTEM.into(),
                    writable: false,
                },
                Mount {
                    host: "/r/data".into(),
                    at: DATA.into(),
                    writable: true,
                },
            ],
            links: vec![],
            devices: vec!["/dev/dri".into()],
            property_area: Some("/r/properties".into()),
            program: "/bin/true".into(),
            args: vec![],
            env: vec![],
            cwd: None,
            stdin: None,
            stdout: Stdio::null(),
            stderr: Stdio::null(),
        }
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
        // Golden argv, written from the profile (Q4(a)): uid map + pid namespace, gone with the broker; info fd; empty root + fresh /dev and /proc; then the seccomp program when given;
        // the host's dirs bound and symlinks recreated, except what the sandbox replaces (dev, proc) or a mount covers (data, system); the mounts, the device nodes, the area at bionic's path; the end of options.
        let head = "--unshare-user --uid 10123 --gid 10123 --unshare-pid --die-with-parent --info-fd 9 --tmpfs / --dev /dev --proc /proc";
        let tail = "--bind /home /home --symlink usr/lib /lib --ro-bind /r/system /system --bind /r/data /data --dev-bind /dev/dri /dev/dri --ro-bind /r/properties /dev/__properties__ --";
        assert_eq!(
            s(&bwrap_args(&spawn(), &root, 9, None)).join(" "),
            format!("{head} {tail}")
        );
        assert_eq!(
            s(&bwrap_args(&spawn(), &root, 9, Some(8))).join(" "),
            format!("{head} --seccomp 8 {tail}")
        );
    }

    /// Golden argv of Profile::BinderDaemon, written from its spec (plan F1.2 B.3): every namespace incl. net, its own user namespace as nobody (65534), gone with the broker, its own session (no terminal ioctls), no capabilities;
    /// info fd; the seccomp program (always: no unfiltered binderd); an empty root holding only the given files read-only at their paths; no /dev, no /proc.
    #[test]
    fn binderd_shape() {
        let files: Vec<PathBuf> = vec!["/b/atl-binderd".into(), "/lib64/ld.so".into()];
        assert_eq!(
            s(&binderd_args(&files, 9, 8)).join(" "),
            "--unshare-all --unshare-user --uid 65534 --gid 65534 --die-with-parent --new-session --cap-drop ALL --info-fd 9 --seccomp 8 --tmpfs / --ro-bind /b/atl-binderd /b/atl-binderd --ro-bind /lib64/ld.so /lib64/ld.so --"
        );
    }

    /// The closure of this test binary (dynamically linked like binderd): itself first, then the interpreter its PT_INTERP names, and glibc's libc.so.6 (every glibc program needs it) from the interpreter's directory.
    #[test]
    fn closure_holds_loader_and_libc() {
        let exe = std::env::current_exe().unwrap();
        let files = elf_closure(&exe).unwrap();
        assert_eq!(files[0], exe);
        let interp = &files[1];
        assert!(interp.is_absolute() && interp.is_file(), "{interp:?}");
        let dir = std::fs::canonicalize(interp).unwrap();
        let libc = dir.parent().unwrap().join("libc.so.6");
        assert!(files.contains(&libc), "{files:?}");
        // a library missing from that directory fails instead of yielding a sandbox binderd cannot start in
        assert!(elf_closure(Path::new("/nonexistent")).is_err());
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
