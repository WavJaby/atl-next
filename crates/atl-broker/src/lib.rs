// SPDX-License-Identifier: MIT
//! atl-broker: the one launcher of sandboxed processes (plan Q5), binderd included: [`Broker::start`] runs it in [`Profile::BinderDaemon`] and holds its admin connection; every [`Broker::spawn`] gets its binderd control connection, registered tagged {uid, host pid}.
//! App sandbox (Q5): bubblewrap with a user namespace mapping the process's uid and gid, a pid namespace, a minimal root ([`APP_ROOT_DIRS`] read-only, a private `/tmp`), the caller's mounts over it (`/system`, `/data`, ...),
//! a fresh `/dev` holding the property area at bionic's default path. The stub (atl-sandbox-stub) applies Landlock with the grants bwrap recorded ([`bwrap::Access`]) before the program runs. The app's seccomp filter is [`seccomp`] (bwrap `--seccomp`).
//!
//! Tag pid: the host pid bwrap reports on `--info-fd` (`child-pid`), the namespace's first process, which the broker owns (killing it ends the sandbox: `--unshare-pid` makes it pid 1).
//! Spawns are serialised by a lock in [`Broker`]: the control, info and seccomp fds are made inheritable just before the spawn and closed right after it, and a concurrent spawn would inherit them.
//! [`control`] is how the system services ask atld (which owns the broker) to start and kill processes.
#![forbid(unsafe_code)]

pub mod adb;
pub mod app_env;

pub mod control;
pub mod gpu;
mod seccomp;
pub use seccomp::{NiceTiers, granted_nice};

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
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
/// Android's socket directory in every sandbox; liblog's logd socket lives in it (`/dev/socket/logdw`, logd_writer.cpp).
const SOCKET_DIR: &str = "/dev/socket";
use atl_bwrap::{self as bwrap, Access, PROGRAM as BWRAP};

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
    /// What the app's manifest permissions give beyond its mounts ([`rights_of`]): [`Right::Net`] leaves TCP unrestricted; without it TCP is refused (Landlock ABI >= 4).
    pub rights: Vec<Right>,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    /// The working directory inside the sandbox (bwrap `--chdir`); None = `/`, as an Android app process has: the broker's own cwd is no app's business, and ART's non-zygote native-bridge init makes `./code_cache` (runtime.cc `PreInitializeNativeBridge(".")`), which landed in the caller's checkout.
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

/// Read-only mounts of a runtime tree at its own paths: `root`, and for each symlink under it the file or directory it resolves to. A staged library is often a link into the build tree, which a sandbox does not otherwise show.
/// Symlinks are not followed while walking; a dangling link is skipped (the loader fails on it, as on a missing library).
pub fn tree_mounts(root: &Path) -> io::Result<Vec<Mount>> {
    let mut mounts = vec![Mount {
        host: root.to_path_buf(),
        at: root.to_path_buf(),
        writable: false,
    }];
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                dirs.push(entry.path());
            } else if ft.is_symlink()
                && let Ok(target) = std::fs::canonicalize(entry.path())
            {
                mounts.push(Mount {
                    host: target.clone(),
                    at: target,
                    writable: false,
                });
            }
        }
    }
    Ok(mounts)
}

/// [`tree_mounts`] of each existing directory of `path` (an LD_LIBRARY_PATH value), and the RUNPATH closure of every ELF in them: a library the runtime dlopens from such a dir loads its own dependencies through its RUNPATH.
pub fn library_path_mounts(path: &OsStr) -> io::Result<Vec<Mount>> {
    let mut mounts = Vec::new();
    let mut elfs = Vec::new();
    for dir in std::env::split_paths(path).filter(|d| d.is_dir()) {
        mounts.extend(tree_mounts(&dir)?);
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let p = if entry.file_type()?.is_symlink() {
                std::fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path())
            } else {
                entry.path()
            };
            if p.is_file() && is_elf(&p) {
                elfs.push(p);
            }
        }
    }
    mounts.extend(runpath_mounts_of(&elfs)?);
    Ok(mounts)
}

/// Whether `path` starts with the ELF magic. A `.so` under an app's `lib/` without it is data the app reads itself (Unity's `libil2cpp.usym.so`), not a library anything links.
pub fn is_elf(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && magic == *b"\x7fELF"
}

/// What a sandbox is for: selects its namespaces, its filesystem view and its seccomp program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// An Android process ([`Spawn`]): its uid, the minimal root with the caller's mounts, Landlock-confined by the stub, registered with binderd.
    App,
    /// binderd itself ([`Broker::start`]): every namespace, no capabilities, a root holding only its executable's ELF closure, the [`seccomp::binderd_filter`] allow-list.
    BinderDaemon,
    /// The package installer ([`Broker::spawn_installer`], plan 4b): AOSP's package parse and native-code choice over the caller's package dir and one output dir.
    /// Its own uid ([`INSTALLER_UID`]), no network namespace, not registered with binderd (no binder), the caller's environment alone, the installer allow-list ([`seccomp`]).
    Installer,
    /// AOSP's adbd ([`Broker::spawn_adbd`], plan adbd S2): a user namespace mapped by the setuid helpers ([`adb`]) so its drop_privileges runs unpatched, the host's network (its listener is loopback by property), the app seccomp program, not registered with binderd (S3 gives its shell a binder door).
    Adb,
}

/// The uid and gid binderd runs as inside its user namespace: the overflow id (nobody), which owns nothing binderd could reach.
const BINDERD_ID: u32 = 65534;

/// The installer's appId (plan S10: a dedicated uid, since the package it installs has no uid yet). The first application uid, held back for the installer: package uids start above it.
pub const INSTALLER_UID: u32 = 10_000;

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

/// The host directories an app sandbox shows, read-only at their own paths: `usr` (libraries, binaries, `usr/share`) and `etc` (the loader's cache, locale and config files).
/// Nothing else of the host is shown: no `home` (the user's files), `run`, `var`, `opt`, `mnt`. `/tmp`, `/proc` and `/dev` are the sandbox's own.
/// A row is a grant: add one only with the gate that needs it.
pub const APP_ROOT_DIRS: &[&str] = &["usr", "etc"];

/// bwrap's argv limit: bubblewrap.c's "Exceeded maximum number of arguments", counted over bwrap's own arguments and the program's.
pub const BWRAP_MAX_ARGS: usize = 9000;

/// Where every app sandbox binds the stub that bwrap execs first (the broker's own path, not a host path).
pub const SANDBOX_STUB_AT: &str = "/.atl-sandbox-stub";

/// The host's `/` as an app sandbox shows it: the directories in [`APP_ROOT_DIRS`], and every symlink (`lib` -> `usr/lib`, ...) recreated as it is.
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
        } else if ft.is_dir() && APP_ROOT_DIRS.contains(&name.as_str()) {
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

/// The sandbox's bwrap arguments for `spawn` in `profile` over `root` (up to and including `--`), info on fd `info_fd`, the seccomp program on `seccomp_fd` when given, and the stub `stub` and the program bound read-only to be exec'd.
/// Returns the grants with them: every bind and fresh mount, which `stub_args` turns into the stub's Landlock rules, so no path is visible without one.
/// A root entry a mount covers (its first path component) is left out, as are `/dev` and `/proc`, which the sandbox gets fresh.
pub fn bwrap_args(
    spawn: &Spawn,
    profile: Profile,
    root: &[RootEntry],
    stub: &Path,
    info_fd: i32,
    seccomp_fd: Option<i32>,
) -> (Vec<OsString>, Vec<(Access, PathBuf)>) {
    let mut a = bwrap::Args::new();
    a.unshare_user();
    if profile == Profile::Adb {
        // the maps come from newuidmap/newgidmap (adb.rs), the ids with them; the caller adds --userns-block-fd
        for cap in adb::ADBD_CAPS {
            a.cap_add(cap);
        }
    } else {
        a.ids(spawn.uid);
    }
    a.unshare_pid()
        .die_with_parent()
        .info_fd(info_fd)
        .tmpfs("/")
        .dev("/dev")
        .grant(Access::Rw, "/dev")
        // init's /dev/socket (first_stage_init.cpp `mkdir("/dev/socket", 0755)`): the process's own liblog sink binds `logdw` there (atl-app-launcher logdw.rs)
        .dir(SOCKET_DIR)
        .proc("/proc")
        .grant(Access::Ro, "/proc");
    if profile != Profile::Installer {
        // the app's private /tmp; the installer writes nowhere but its output dir
        a.tmpfs("/tmp").grant(Access::Rw, "/tmp");
    }
    if profile == Profile::Installer {
        // no network: a namespace with loopback only; no controlling terminal
        a.unshare_net().new_session();
    }
    if let Some(fd) = seccomp_fd {
        a.seccomp(fd);
    }
    // a root dir is left out only when a mount or link sits at exactly its path (adb's /bin -> /system/bin); a mount below it is bound over it, after it
    let covered = |name: &str| {
        let at = Path::new("/").join(name);
        name == "dev"
            || name == "proc"
            || spawn.mounts.iter().any(|m| m.at == at)
            || spawn.links.iter().any(|l| l.at == at)
    };
    for e in root {
        match e {
            RootEntry::Dir(n) if !covered(n) => {
                let p = format!("/{n}");
                a.ro_bind(&p, &p);
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
    // last: a directory mounted after these would shadow the files. The stub is exec'd by bwrap before it restricts itself, the program by the stub after: both read-only.
    // The stub sits at SANDBOX_STUB_AT, which no host mount can cover (its host path lies under build trees that other mounts show).
    a.ro_bind(stub, SANDBOX_STUB_AT);
    a.ro_bind(&spawn.program, &spawn.program);
    // the cwd defaults to `/`: the caller's directory is not necessarily shown in the sandbox
    a.chdir(spawn.cwd.as_deref().unwrap_or(Path::new("/")));
    let grants = a.grants().to_vec();
    (a.end(), grants)
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

/// The sandbox stub's argv after its path: `--net` when `net` (TCP unrestricted), each grant as `--ro` / `--rw` / `--device` PATH, then `-- program`. The stub restricts itself with Landlock to exactly those grants and execs `program`, which inherits the domain.
pub fn stub_args(grants: &[(Access, PathBuf)], net: bool, program: &Path) -> Vec<OsString> {
    let mut v = Vec::with_capacity(grants.len() * 2 + 3);
    if net {
        v.push("--net".into());
    }
    for (access, path) in grants {
        v.push(
            match access {
                Access::Ro => "--ro",
                Access::Rw => "--rw",
                Access::Dev => "--device",
            }
            .into(),
        );
        v.push(path.into());
    }
    v.push("--".into());
    v.push(program.into());
    v
}

/// One row per manifest permission: what it gives the app process. Landlock restricts exactly the rights a ruleset handles, so `Net` is the only network row: TCP left unhandled (ABI >= 4), not a port list.
pub const PERMISSION_RIGHTS: &[(&str, Right)] = &[("android.permission.INTERNET", Right::Net)];

/// What a process's permissions give (PERMISSION_RIGHTS rows, in table order).
pub fn rights_of<S: AsRef<str>>(permissions: &[S]) -> Vec<Right> {
    PERMISSION_RIGHTS
        .iter()
        .filter(|(p, _)| permissions.iter().any(|g| g.as_ref() == *p))
        .map(|(_, r)| *r)
        .collect()
}

/// A right a manifest permission gives an app process beyond its mounts (see [`Spawn::rights`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Right {
    /// TCP bind and connect on any port: the Landlock network rights are left unhandled.
    Net,
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

/// `path`'s PT_INTERP and DT_NEEDED names; 32- and 64-bit ELF (an arm32 game's libraries are ELFCLASS32).
pub fn elf_deps(path: &Path) -> io::Result<(Option<PathBuf>, Vec<String>)> {
    let data = std::fs::read(path)?;
    // e_ident[EI_CLASS] (EI_CLASS = 4) is ELFCLASS32 = 1 or ELFCLASS64 = 2 (ELF spec, e_ident layout)
    match data.get(4) {
        Some(1) => elf_deps_of::<object::elf::FileHeader32<object::Endianness>>(path, &data),
        _ => elf_deps_of::<object::elf::FileHeader64<object::Endianness>>(path, &data),
    }
}

fn elf_deps_of<Elf: object::read::elf::FileHeader<Endian = object::Endianness>>(
    path: &Path,
    data: &[u8],
) -> io::Result<(Option<PathBuf>, Vec<String>)> {
    use object::read::elf::{Dyn, ElfFile, ProgramHeader};
    let elf = ElfFile::<Elf>::parse(data).map_err(|e| other(format!("{}: {e}", path.display())))?;
    let endian = elf.endian();
    let bad = |e: object::Error| other(format!("{}: {e}", path.display()));
    let mut interp = None;
    for ph in elf.elf_program_headers() {
        if let Some(bytes) = ph.interpreter(endian, data).map_err(bad)? {
            interp = Some(PathBuf::from(OsStr::from_bytes(bytes)));
        }
    }
    let mut needed = Vec::new();
    let sections = elf.elf_section_table();
    if let Some((dynamic, link)) = sections.dynamic(endian, data).map_err(bad)? {
        let strings = sections.strings(endian, data, link).map_err(bad)?;
        for d in dynamic {
            if d.tag(endian) == object::elf::DT_NEEDED {
                let name = d.string(endian, strings).map_err(bad)?;
                needed.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    Ok((interp, needed))
}

/// `path`'s search entries for its own DT_NEEDED: DT_RUNPATH, or DT_RPATH when it has no RUNPATH.
fn elf_runpath(path: &Path) -> io::Result<Vec<String>> {
    use object::read::elf::{Dyn, ElfFile64};
    let data = std::fs::read(path)?;
    let elf = ElfFile64::<object::Endianness>::parse(&*data)
        .map_err(|e| other(format!("{}: {e}", path.display())))?;
    let endian = elf.endian();
    let bad = |e: object::Error| other(format!("{}: {e}", path.display()));
    let sections = elf.elf_section_table();
    let (mut runpath, mut rpath) = (Vec::new(), Vec::new());
    if let Some((dynamic, link)) = sections.dynamic(endian, &*data).map_err(bad)? {
        let strings = sections.strings(endian, &*data, link).map_err(bad)?;
        for d in dynamic {
            let tag = d.tag(endian);
            if tag == object::elf::DT_RUNPATH || tag == object::elf::DT_RPATH {
                let text =
                    String::from_utf8_lossy(d.string(endian, strings).map_err(bad)?).into_owned();
                let list = if tag == object::elf::DT_RUNPATH {
                    &mut runpath
                } else {
                    &mut rpath
                };
                list.extend(text.split(':').filter(|s| !s.is_empty()).map(str::to_owned));
            }
        }
    }
    Ok(if runpath.is_empty() { rpath } else { runpath })
}

/// The sysfs a process with DRM nodes `devices` (nodes, or dirs of them) reads, and no more: libdrm (drmGetDevice2, xf86drm.c) and the host GL driver's loader (Mesa's loader.c) resolve a node's `/sys/dev/char/<major>:<minor>` link into its device's dir under `/sys/devices` and read its `uevent`, PCI ids and `subsystem` link name there;
/// `/sys/class/drm/<node>` is the same link by name. Returns that device dir read-only at its own path, and the two links recreated as the host has them. Without them the host driver falls back to the render node alone, whose dumb buffers EACCES.
pub fn drm_sysfs(devices: &[PathBuf]) -> io::Result<(Vec<Mount>, Vec<Link>)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut nodes = Vec::new();
    for d in devices {
        if d.is_dir() {
            for e in std::fs::read_dir(d)? {
                nodes.push(e?.path());
            }
        } else {
            nodes.push(d.clone());
        }
    }
    let mut dirs = std::collections::BTreeSet::new();
    let mut links = Vec::new();
    for n in nodes {
        let meta = std::fs::metadata(&n)?;
        if !meta.file_type().is_char_device() {
            continue;
        }
        let rdev = meta.rdev();
        let by_id = PathBuf::from(format!(
            "/sys/dev/char/{}:{}",
            rustix::fs::major(rdev),
            rustix::fs::minor(rdev)
        ));
        let Ok(target) = std::fs::read_link(&by_id) else {
            continue;
        };
        let node_dir = std::fs::canonicalize(&by_id)?;
        // .../<device>/drm/<node>: the device's dir holds what libdrm reads; a node outside a drm/ dir is shown alone
        let dev_dir = match node_dir.parent() {
            Some(p) if p.file_name() == Some(OsStr::new("drm")) => {
                p.parent().unwrap_or(p).to_path_buf()
            }
            _ => node_dir.clone(),
        };
        dirs.insert(dev_dir);
        links.push(Link { target, at: by_id });
        if let Some(name) = node_dir.file_name() {
            let by_name = Path::new("/sys/class/drm").join(name);
            if let Ok(target) = std::fs::read_link(&by_name) {
                links.push(Link {
                    target,
                    at: by_name,
                });
            }
        }
    }
    // + each device's driver module state (`<device>/driver/module` -> /sys/module/<m>/initstate): NVIDIA's EGL declines its GPU without reading it (measured: laptop, nvidia 615.78, Wayland offload falls back to Mesa);
    // the file, not the module dir, whose parameters are none of a process's business
    let states: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|d| std::fs::canonicalize(d.join("driver/module")).ok())
        .map(|m| m.join("initstate"))
        .filter(|f| f.is_file())
        .collect();
    let mounts = dirs
        .into_iter()
        .chain(states)
        .map(|d| Mount {
            host: d.clone(),
            at: d,
            writable: false,
        })
        .collect();
    Ok((mounts, links))
}

/// Read-only mounts of what `program`'s loader finds through RUNPATH: each object's own RUNPATH dirs ([`tree_mounts`]) and the DT_NEEDED libraries in them, followed in turn (a RUNPATH serves only the object that has it).
/// `$ORIGIN` is the object's directory. LD_LIBRARY_PATH's dirs are the caller's environment (its Spawn env); the default search path is the interpreter's directory ([`elf_closure`]).
pub fn runpath_mounts(program: &Path) -> io::Result<Vec<Mount>> {
    runpath_mounts_of(&[program.to_path_buf()])
}

/// [`runpath_mounts`] of each of `roots`, one set: what each loads through RUNPATH, transitively.
pub fn runpath_mounts_of(roots: &[PathBuf]) -> io::Result<Vec<Mount>> {
    let mut mounts = Vec::new();
    let mut dirs = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut queue = roots.to_vec();
    while let Some(obj) = queue.pop() {
        if !seen.insert(obj.clone()) {
            continue;
        }
        let origin = obj
            .parent()
            .unwrap_or(Path::new("/"))
            .to_string_lossy()
            .into_owned();
        // not a 64-bit ELF: not loadable by this sandbox's loader path, so nothing to follow
        let Ok((_, needed)) = elf_deps(&obj) else {
            continue;
        };
        for entry in elf_runpath(&obj)? {
            let dir = PathBuf::from(entry.replace("$ORIGIN", &origin));
            if !dir.is_dir() {
                continue;
            }
            if dirs.insert(dir.clone()) {
                mounts.extend(tree_mounts(&dir)?);
            }
            queue.extend(
                needed
                    .iter()
                    .map(|n| dir.join(n))
                    .filter(|lib| lib.is_file()),
            );
        }
    }
    Ok(mounts)
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

/// Poll step of [`terminate_sandbox`]'s wait: short against any grace worth configuring, long against a busy loop.
const END_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Stops the sandbox whose first process is `host_pid` the Linux way: SIGTERM to its program (the init's one child; the init, pid 1 of its namespace, ignores a signal it has no handler for, and
/// ends when its child does), then [`kill_sandbox`] once `grace` passes without the sandbox ending. Returns whether it ended within `grace` (false: killed).
pub fn terminate_sandbox(host_pid: i32, grace: std::time::Duration) -> io::Result<bool> {
    if !running(host_pid) {
        return Ok(true);
    }
    let child = program_host_pid(host_pid)?;
    let pid = Pid::from_raw(child).ok_or_else(|| other(format!("{child}: not a process id")))?;
    kill_process(pid, Signal::TERM)?;
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if !running(host_pid) {
            return Ok(true);
        }
        std::thread::sleep(END_POLL);
    }
    kill_sandbox(host_pid)?;
    Ok(false)
}

/// Whether `pid` still runs: its `/proc` entry exists and is not a zombie (stat field 3, proc(5); the comm before it is parenthesised and may hold spaces).
fn running(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit_once(") ")
            .and_then(|(_, rest)| rest.chars().next())
            .is_some_and(|state| state != 'Z' && state != 'X')
    })
}

/// The host pid of the program of the sandbox whose first process is `host_pid`: bwrap's init is that first process (pid 1; no `--as-pid-1`) and forks the program as its one child, which the stub
/// execs into. Read from the kernel: the init's children (`/proc/<pid>/task/<pid>/children`).
fn program_host_pid(host_pid: i32) -> io::Result<i32> {
    let children = fs::read_to_string(format!("/proc/{host_pid}/task/{host_pid}/children"))?;
    match children.split_whitespace().collect::<Vec<_>>()[..] {
        [one] => one
            .parse()
            .map_err(|_| other(format!("sandbox {host_pid}: child {one:?} is not a pid"))),
        ref all => Err(other(format!(
            "sandbox {host_pid}: init has children {all:?}, want one"
        ))),
    }
}

/// The pid the program of the sandbox whose first process is `host_pid` has in its own pid namespace (its getpid(), Process.myPid()): [`program_host_pid`]'s process, its `NSpid` (innermost last).
pub fn program_pid(host_pid: i32) -> io::Result<i32> {
    let child = program_host_pid(host_pid)?;
    let status = fs::read_to_string(format!("/proc/{child}/status"))?;
    innermost_pid(&status).ok_or_else(|| other(format!("sandbox {host_pid}: no NSpid for {child}")))
}

/// The last pid of a `/proc/<pid>/status` `NSpid:` line: the pid in the process's own namespace (proc(5)).
fn innermost_pid(status: &str) -> Option<i32> {
    let line = status.lines().find_map(|l| l.strip_prefix("NSpid:"))?;
    line.split_whitespace().last()?.parse().ok()
}

/// The broker's side of binderd: its admin connection. Dropping it closes the connection, which ends binderd.
pub struct Broker {
    admin: OwnedFd,
    /// Held across a spawn: the fds made inheritable for it must not reach another spawn's child.
    spawning: Mutex<()>,
    /// Whether apps get the app seccomp filter (seccomp.rs); off only for the `--no-app-seccomp` check.
    app_seccomp: bool,
    /// The sandbox stub every app process runs under (bwrap execs it, it restricts itself and execs the program).
    stub: PathBuf,
}

fn other(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

/// One bwrap run: its arguments up to `--`, then the program; `env` its whole environment (bwrap adds PWD); `inherit` = the fds the child alone gets (cleared close-on-exec just before the spawn, closed right after it). Returns bwrap and the host pid it reports on `info_r`.
struct Launch<'a> {
    bwrap_args: Vec<OsString>,
    program: &'a Path,
    args: &'a [OsString],
    env: Vec<(OsString, OsString)>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
}

fn launch(l: Launch<'_>, info_r: io::PipeReader, inherit: Vec<OwnedFd>) -> io::Result<Process> {
    for fd in &inherit {
        fcntl_setfd(fd, FdFlags::empty())?;
    }
    // bwrap refuses an argv longer than this and then reports the program's exec as failed: say so here
    let argc = l.bwrap_args.len() + 1 + l.args.len();
    if argc > BWRAP_MAX_ARGS {
        return Err(other(format!(
            "{argc} arguments for bwrap, which accepts {BWRAP_MAX_ARGS} (bubblewrap.c): the sandbox shows too many paths"
        )));
    }
    #[expect(
        clippy::disallowed_types,
        reason = "the one launcher of the runtime: every process atld starts is a bwrap run built here"
    )]
    // bwrap found through the broker's PATH: std resolves a bare name through the child's PATH when the child's environment sets one (adbd's is init's /system/bin:...)
    let mut cmd = std::process::Command::new(host_program(BWRAP)?);
    cmd.env_clear()
        .args(&l.bwrap_args)
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

/// `name`'s path in the broker's own PATH (the first directory holding it).
fn host_program(name: &str) -> io::Result<PathBuf> {
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .ok_or_else(|| other(format!("{name} is not in the broker's PATH")))
}

impl Broker {
    /// Starts binderd (`program`) in [`Profile::BinderDaemon`] with its admin connection on stdin and its stderr (counters) to `log`, and the broker over the other end of that connection.
    /// bwrap runs `stub` (atl-sandbox-stub), which applies Landlock (read + execute on binderd's ELF closure only) and execs binderd; both closures are bound, so the stub's own files are visible but not in the domain.
    /// Order: bwrap installs seccomp, then execs the stub, so the seccomp filter sees the stub's calls (landlock_*, prctl) as well as binderd's.
    /// Dropping the Broker closes admin, which ends binderd; the returned process is bwrap, which exits with binderd's status.
    pub fn start(program: &Path, stub: &Path, log: File) -> io::Result<(Broker, Process)> {
        let (admin, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )?;
        let (info_r, info_w) = io::pipe()?;
        let seccomp = seccomp_memfd(Profile::BinderDaemon)?;
        let allow = elf_closure(program)?;
        let mut files = allow.clone();
        for f in elf_closure(stub)? {
            if !files.contains(&f) {
                files.push(f);
            }
        }
        let bwrap_args = binderd_args(&files, info_w.as_raw_fd(), seccomp.as_raw_fd());
        let grants: Vec<(Access, PathBuf)> =
            allow.iter().map(|f| (Access::Ro, f.clone())).collect();
        let args = stub_args(&grants, false, program);
        let process = launch(
            Launch {
                bwrap_args,
                program: stub,
                args: &args,
                // binderd reads no variable
                env: vec![],
                stdin: theirs.into(),
                stdout: Stdio::null(),
                stderr: log.into(),
            },
            info_r,
            vec![info_w.into(), seccomp],
        )
        .map_err(|e| other(format!("starting binderd {}: {e}", program.display())))?;
        Ok((Broker::new(admin, stub), process))
    }

    /// Ends binderd the way its admin contract says (atl-binderd main: it exits on a closed admin connection and prints its counters), while threads still hold this Broker: shuts the
    /// connection down for both directions. Spawns after it fail.
    pub fn close_admin(&self) -> io::Result<()> {
        rustix::net::shutdown(&self.admin, rustix::net::Shutdown::Both).map_err(io::Error::from)
    }

    fn new(admin: OwnedFd, stub: &Path) -> Self {
        Broker {
            admin,
            spawning: Mutex::new(()),
            app_seccomp: true,
            stub: stub.to_path_buf(),
        }
    }

    /// Apps start without the seccomp filter: the deliberate-failure check that shows what the filter answers (atld `--no-app-seccomp`, test only).
    pub fn without_app_seccomp(mut self) -> Self {
        self.app_seccomp = false;
        self
    }

    /// Starts `spawn` in its sandbox and registers its control connection with binderd once bwrap reports the host pid.
    pub fn spawn(&self, mut spawn: Spawn) -> io::Result<Process> {
        // a panic elsewhere leaves nothing half-done here: the guard only orders spawns
        let _one_at_a_time = self.spawning.lock().unwrap_or_else(|e| e.into_inner());
        complete_mounts(&mut spawn)?;
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
        let (bwrap_args, grants) = bwrap_args(
            &spawn,
            Profile::App,
            &host_root()?,
            &self.stub,
            info_w.as_raw_fd(),
            seccomp.as_ref().map(AsRawFd::as_raw_fd),
        );
        // the stub applies the grants, then execs the program with the spawn's arguments
        let mut args = stub_args(&grants, spawn.rights.contains(&Right::Net), &spawn.program);
        // the app's thread-priority tiers (seccomp.rs NiceTiers): the stub raises the soft RLIMIT_NICE to the hard one and logs the tier line once per process
        if self.app_seccomp {
            let tiers = seccomp::NiceTiers::for_limit(seccomp::granted_nice());
            args.splice(0..0, [OsString::from("--nice"), tiers.describe().into()]);
        }
        args.extend(spawn.args.iter().cloned());
        // the desktop's variables the sandbox reads (app_env::APP_ENV), then the spawn's own (later winning on a clash); nothing else of atld's environment
        let mut env = app_env::forwarded(|k| std::env::var_os(k));
        env.extend(spawn.env);
        env.push((CONTROL_ENV.into(), theirs.as_raw_fd().to_string().into()));
        let mut inherit: Vec<OwnedFd> = vec![theirs, info_w.into()];
        inherit.extend(seccomp);
        let Process { child, host_pid } = launch(
            Launch {
                bwrap_args,
                program: Path::new(SANDBOX_STUB_AT),
                args: &args,
                env,
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

impl Broker {
    /// Starts the installer (plan 4b) in [`Profile::Installer`]: `spawn` runs as [`INSTALLER_UID`] and asks for nothing the profile does not give ([`installer_refusal`]).
    /// Its environment is `spawn.env` alone (no desktop variables, no app env), and it is not registered with binderd: an installer has no binder.
    pub fn spawn_installer(&self, mut spawn: Spawn) -> io::Result<Process> {
        if let Some(why) = installer_refusal(&spawn) {
            return Err(other(format!("installer spawn refused: {why}")));
        }
        let _one_at_a_time = self.spawning.lock().unwrap_or_else(|e| e.into_inner());
        complete_mounts(&mut spawn)?;
        let (info_r, info_w) = io::pipe()?;
        let seccomp = seccomp_memfd(Profile::Installer)?;
        let (bwrap_args, grants) = bwrap_args(
            &spawn,
            Profile::Installer,
            &host_root()?,
            &self.stub,
            info_w.as_raw_fd(),
            Some(seccomp.as_raw_fd()),
        );
        // no rights: TCP is left handled (refused) by the stub, and the namespace has no network anyway
        let mut args = stub_args(&grants, false, &spawn.program);
        args.extend(spawn.args.iter().cloned());
        let Process { child, host_pid } = launch(
            Launch {
                bwrap_args,
                program: Path::new(SANDBOX_STUB_AT),
                args: &args,
                env: spawn.env,
                stdin: spawn.stdin.map_or_else(Stdio::null, Stdio::from),
                stdout: spawn.stdout,
                stderr: spawn.stderr,
            },
            info_r,
            vec![info_w.into(), seccomp],
        )?;
        Ok(Process { child, host_pid })
    }
}

impl Broker {
    /// Starts adbd in [`Profile::Adb`]: bwrap blocks its first process until `maps` are written into the new user namespace ([`adb::write_maps`] on the reported pid), then runs the stub (TCP left open: adbd listens) and adbd as namespace [`adb::AID_SHELL`] with [`adb::ADBD_CAPS`].
    /// `spawn.uid` must be AID_SHELL (the id the maps give the caller); its environment is `spawn.env` alone (init's, as adbd's on a device); no binderd registration. Maps that fail to write end the sandbox.
    pub fn spawn_adbd(&self, mut spawn: Spawn, maps: &adb::IdMaps) -> io::Result<Process> {
        if spawn.uid != adb::AID_SHELL || spawn.context_manager || !spawn.devices.is_empty() {
            return Err(other(
                "adbd spawn refused: uid must be AID_SHELL, no context manager, no devices",
            ));
        }
        let _one_at_a_time = self.spawning.lock().unwrap_or_else(|e| e.into_inner());
        complete_mounts(&mut spawn)?;
        let (info_r, info_w) = io::pipe()?;
        let (block_r, mut block_w) = io::pipe()?;
        let seccomp = seccomp_memfd(Profile::Adb)?;
        let (mut bwrap_args, grants) = bwrap_args(
            &spawn,
            Profile::Adb,
            &host_root()?,
            &self.stub,
            info_w.as_raw_fd(),
            Some(seccomp.as_raw_fd()),
        );
        let mut block = bwrap::Args::new();
        block.userns_block_fd(block_r.as_raw_fd());
        bwrap_args.splice(0..0, block.options());
        let mut args = stub_args(&grants, true, &spawn.program);
        args.extend(spawn.args.iter().cloned());
        let process = launch(
            Launch {
                bwrap_args,
                program: Path::new(SANDBOX_STUB_AT),
                args: &args,
                env: spawn.env,
                stdin: spawn.stdin.map_or_else(Stdio::null, Stdio::from),
                stdout: spawn.stdout,
                stderr: spawn.stderr,
            },
            info_r,
            vec![info_w.into(), block_r.into(), seccomp],
        )?;
        if let Err(e) = adb::write_maps(process.host_pid, maps) {
            let _ = kill_sandbox(process.host_pid);
            return Err(e);
        }
        // readable = maps written: bwrap's first process goes on
        block_w.write_all(b"1")?;
        Ok(process)
    }
}

/// Why [`Broker::spawn_installer`] refuses `spawn`: what the installer profile does not give (its uid, devices, rights, links, the context-manager role), else None.
fn installer_refusal(spawn: &Spawn) -> Option<&'static str> {
    if spawn.uid != INSTALLER_UID {
        Some("uid is not INSTALLER_UID")
    } else if !spawn.devices.is_empty() {
        Some("devices: the installer has no device nodes")
    } else if !spawn.rights.is_empty() {
        Some("rights: the installer has no manifest right, network included")
    } else if !spawn.links.is_empty() {
        Some("links: the installer's view is mounts only")
    } else if spawn.context_manager {
        Some("context manager: the installer has no binder")
    } else {
        None
    }
}

/// The CPU topology dir, shown read-only to every sandbox: Android lets every domain read it (sepolicy domain.te `r_dir_file(domain, sysfs_devices_system_cpu)`, isolated apps included).
/// bionic's sysconf(_SC_NPROCESSORS_CONF/ONLN) reads its `possible` / `online` and answers 1 without them (sysinfo.cpp), so an engine sizing its worker pool by them ran one worker.
pub const CPU_SYSFS: &str = "/sys/devices/system/cpu";

/// What every sandbox shows beyond its caller's mounts: the libraries its loader finds through RUNPATH, the CPU topology ([`CPU_SYSFS`]), the sysfs of its DRM nodes, the trees of its LD_LIBRARY_PATH.
/// A path the root already shows read-only at the same place, or one already mounted, is dropped.
fn complete_mounts(spawn: &mut Spawn) -> io::Result<()> {
    // the libraries its loader finds through RUNPATH are part of the program's view (runpath_mounts)
    spawn.mounts.extend(runpath_mounts(&spawn.program)?);
    // the CPU topology every Android domain reads (CPU_SYSFS)
    spawn.mounts.push(Mount {
        host: PathBuf::from(CPU_SYSFS),
        at: PathBuf::from(CPU_SYSFS),
        writable: false,
    });
    // the sysfs of its DRM nodes only (drm_sysfs)
    let (sys_mounts, sys_links) = drm_sysfs(&spawn.devices)?;
    spawn.mounts.extend(sys_mounts);
    spawn.links.extend(sys_links);
    // the trees of the LD_LIBRARY_PATH the process starts with: a library the loader finds there loads its own RUNPATH dirs too (library_path_mounts)
    for (k, v) in &spawn.env {
        if k == "LD_LIBRARY_PATH" {
            spawn.mounts.extend(library_path_mounts(v)?);
        }
    }
    // INTERNET: the host's resolver configuration, where /etc links it out of the shown root (resolver_view)
    if spawn.rights.contains(&Right::Net) {
        let (mounts, links) = resolver_view(Path::new(RESOLV_CONF))?;
        spawn.mounts.extend(mounts);
        spawn.links.extend(links);
    }
    let mut seen = std::collections::BTreeSet::new();
    spawn.mounts.retain(|m| {
        let in_root = m.host == m.at
            && APP_ROOT_DIRS
                .iter()
                .any(|d| m.at.starts_with(Path::new("/").join(d)));
        !in_root && seen.insert((m.host.clone(), m.at.clone(), m.writable))
    });
    Ok(())
}

/// The resolver configuration glibc's stub resolver reads (resolv.h `_PATH_RESCONF`), which the guest's and ART's getaddrinfo reach through the host libc.
pub const RESOLV_CONF: &str = "/etc/resolv.conf";

/// What makes `path` resolve inside the sandbox as on the host when it is a symlink chain leaving the shown root (systemd-resolved: `-> /run/systemd/resolve/stub-resolv.conf`; WSL: `-> /mnt/wsl/resolv.conf`):
/// every hop outside [`APP_ROOT_DIRS`] that is a link becomes the same link, the final file a read-only mount at its own path. Each hop's parent is canonicalised, so a linked directory (`/var/run -> /run`) is followed too.
/// Nothing for a path the root already shows whole, and nothing when the chain dangles on the host or loops (more than 40 hops, the kernel's MAXSYMLINKS): the host has no resolver file then, and neither does the app.
fn resolver_view(path: &Path) -> io::Result<(Vec<Mount>, Vec<Link>)> {
    let shown = |p: &Path| {
        APP_ROOT_DIRS
            .iter()
            .any(|d| p.starts_with(Path::new("/").join(d)))
    };
    let mut mounts = Vec::new();
    let mut links = Vec::new();
    let mut hop = path.to_path_buf();
    for _ in 0..40 {
        let (Some(parent), Some(name)) = (hop.parent(), hop.file_name()) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let here = match fs::canonicalize(parent) {
            Ok(dir) => dir.join(name),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
            Err(e) => return Err(e),
        };
        let meta = match fs::symlink_metadata(&here) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
            Err(e) => return Err(e),
        };
        if !meta.file_type().is_symlink() {
            if !shown(&here) {
                mounts.push(Mount {
                    host: here.clone(),
                    at: here,
                    writable: false,
                });
            }
            return Ok((mounts, links));
        }
        let target = fs::read_link(&here)?;
        if !shown(&here) {
            links.push(Link {
                target: target.clone(),
                at: here.clone(),
            });
        }
        hop = here.parent().unwrap_or(Path::new("/")).join(target);
    }
    Ok((Vec::new(), Vec::new()))
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
    use atl_android_paths::{DATA, PROC_CPUINFO, SDCARD, SHARED_STORAGE, SYSTEM};

    /// A file mount over a /proc entry (atld's guest cpuinfo for a translated app) lands after the fresh `--proc /proc`, so it covers the kernel's file instead of being covered by the procfs; and it is granted read-only.
    #[test]
    fn proc_file_mount_follows_the_fresh_proc() {
        let mut sp = spawn();
        sp.mounts.push(Mount {
            host: "/r/cpuinfo.arm64".into(),
            at: PROC_CPUINFO.into(),
            writable: false,
        });
        let (args, grants) = bwrap_args(&sp, Profile::App, &[], Path::new("/b/stub"), 9, None);
        let args = s(&args);
        // the fresh procfs is the only argument naming /proc itself; the bind names its source
        let proc_at = args.iter().position(|a| a == "/proc");
        let bind_at = args
            .windows(2)
            .position(|w| w == ["/r/cpuinfo.arm64", PROC_CPUINFO]);
        assert!(
            matches!((proc_at, bind_at), (Some(p), Some(b)) if p < b),
            "{args:?}"
        );
        assert!(grants.contains(&(Access::Ro, PathBuf::from(PROC_CPUINFO))));
    }

    /// Spawn.links follow the mounts as bwrap `--symlink TARGET AT`: the app's /sdcard -> the shared storage root.
    #[test]
    fn links_follow_the_mounts() {
        let mut sp = spawn();
        sp.links.push(Link {
            target: SHARED_STORAGE.into(),
            at: SDCARD.into(),
        });
        let joined =
            s(&bwrap_args(&sp, Profile::App, &[], Path::new("/b/stub"), 9, None).0).join(" ");
        assert!(joined.ends_with(
            "--ro-bind /r/system /system --bind /r/data /data --symlink /storage/emulated/0 /sdcard --dev-bind /dev/dri /dev/dri --ro-bind /r/properties /dev/__properties__ --ro-bind /b/stub /.atl-sandbox-stub --ro-bind /bin/true /bin/true --chdir / --"
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
            rights: vec![],
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
        // host_root's rows for this test: etc and usr (APP_ROOT_DIRS), lib -> usr/lib, sorted by name
        let root = vec![
            RootEntry::Dir("etc".into()),
            RootEntry::Symlink {
                name: "lib".into(),
                target: "usr/lib".into(),
            },
            RootEntry::Dir("usr".into()),
        ];
        // Golden argv, written from the profile: uid map + pid namespace, gone with the broker; info fd; empty root + fresh /dev (with init's /dev/socket) and /proc, a private /tmp; then the seccomp program when given;
        // the root's dirs read-only and its symlinks recreated; the mounts, the device nodes, the area at bionic's path; the stub and the program read-only; cwd `/`; the end of options.
        let head = "--unshare-user --uid 10123 --gid 10123 --unshare-pid --die-with-parent --info-fd 9 --tmpfs / --dev /dev --dir /dev/socket --proc /proc --tmpfs /tmp";
        let tail = "--ro-bind /etc /etc --symlink usr/lib /lib --ro-bind /usr /usr --ro-bind /r/system /system --bind /r/data /data --dev-bind /dev/dri /dev/dri --ro-bind /r/properties /dev/__properties__ --ro-bind /b/stub /.atl-sandbox-stub --ro-bind /bin/true /bin/true --chdir / --";
        let stub = Path::new("/b/stub");
        assert_eq!(
            s(&bwrap_args(&spawn(), Profile::App, &root, stub, 9, None).0).join(" "),
            format!("{head} {tail}")
        );
        assert_eq!(
            s(&bwrap_args(&spawn(), Profile::App, &root, stub, 9, Some(8)).0).join(" "),
            format!("{head} --seccomp 8 {tail}")
        );
        // the grants, in call order: the fresh /dev, /proc, /tmp; each bind at its sandbox path, the device and the area
        let (_, grants) = bwrap_args(&spawn(), Profile::App, &root, stub, 9, None);
        let want: Vec<(Access, &str)> = vec![
            (Access::Rw, "/dev"),
            (Access::Ro, "/proc"),
            (Access::Rw, "/tmp"),
            (Access::Ro, "/etc"),
            (Access::Ro, "/usr"),
            (Access::Ro, SYSTEM),
            (Access::Rw, DATA),
            (Access::Dev, "/dev/dri"),
            (Access::Ro, PROPERTY_AREA),
            (Access::Ro, "/.atl-sandbox-stub"),
            (Access::Ro, "/bin/true"),
        ];
        let got: Vec<(Access, &Path)> = grants.iter().map(|(a, p)| (*a, p.as_path())).collect();
        assert_eq!(
            got,
            want.iter()
                .map(|(a, p)| (*a, Path::new(p)))
                .collect::<Vec<_>>()
        );
    }

    /// A mount below a root dir is bound over it, after it: the root dir stays bound (the covered check is exact-path only).
    #[test]
    fn nested_mount_keeps_the_root_dir() {
        let mut sp = spawn();
        sp.mounts.push(Mount {
            host: "/r/fonts".into(),
            at: "/usr/share/fonts".into(),
            writable: false,
        });
        let (argv, _) = bwrap_args(
            &sp,
            Profile::App,
            &[RootEntry::Dir("usr".into())],
            Path::new("/b/stub"),
            9,
            None,
        );
        let joined = s(&argv).join(" ");
        let usr = joined
            .find("--ro-bind /usr /usr ")
            .expect("the root dir is bound");
        let fonts = joined
            .find("--ro-bind /r/fonts /usr/share/fonts")
            .expect("the nested mount is bound");
        assert!(usr < fonts, "{joined}");
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

    /// Golden argv of Profile::Installer, written from its spec: the installer's uid, every namespace and a network namespace of its own (loopback only), its own session, the seccomp program;
    /// then the root, the caller's mounts, the stub and the program as the other profiles give them. No devices, no property area here: the spawn gives none.
    /// A chain leaving the root: each link outside it recreated as written, the file mounted read-only at its own path; a dangling chain and a file the root shows give nothing.
    #[test]
    fn resolver_view_follows_the_chain() {
        let dir = std::env::temp_dir().join(format!("atl-broker-resolv-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).expect("test dir");
        let dir = fs::canonicalize(&dir).expect("canonical test dir");
        fs::write(dir.join("sub/c"), "nameserver 192.0.2.1\n").expect("file");
        std::os::unix::fs::symlink("b", dir.join("a")).expect("link a");
        std::os::unix::fs::symlink("sub/c", dir.join("b")).expect("link b");
        std::os::unix::fs::symlink("gone", dir.join("dangling")).expect("link");
        let (mounts, links) = resolver_view(&dir.join("a")).expect("view");
        assert_eq!(
            links,
            [
                Link {
                    target: "b".into(),
                    at: dir.join("a")
                },
                Link {
                    target: "sub/c".into(),
                    at: dir.join("b")
                }
            ]
        );
        assert_eq!(
            mounts,
            [Mount {
                host: dir.join("sub/c"),
                at: dir.join("sub/c"),
                writable: false
            }]
        );
        assert_eq!(
            resolver_view(&dir.join("dangling")).expect("view"),
            (vec![], vec![])
        );
        assert_eq!(
            resolver_view(Path::new("/etc/passwd")).expect("view"),
            (vec![], vec![])
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn installer_shape() {
        let mut sp = spawn();
        sp.uid = INSTALLER_UID;
        sp.devices.clear();
        sp.property_area = None;
        let root = [RootEntry::Dir("usr".into())];
        assert_eq!(
            s(&bwrap_args(
                &sp,
                Profile::Installer,
                &root,
                Path::new("/b/stub"),
                9,
                Some(8)
            )
            .0)
            .join(" "),
            "--unshare-user --uid 10000 --gid 10000 --unshare-pid --die-with-parent --info-fd 9 --tmpfs / --dev /dev --dir /dev/socket --proc /proc --unshare-net --new-session --seccomp 8 --ro-bind /usr /usr --ro-bind /r/system /system --bind /r/data /data --ro-bind /b/stub /.atl-sandbox-stub --ro-bind /bin/true /bin/true --chdir / --"
        );
    }

    /// The installer profile refuses what it does not give: another uid, a device, a manifest right, a link, the context-manager role. The spawn it accepts has none of them.
    #[test]
    fn installer_refuses_what_it_does_not_give() {
        let mut sp = spawn();
        assert!(
            installer_refusal(&sp).is_some(),
            "uid 10123 is not the installer's"
        );
        sp.uid = INSTALLER_UID;
        sp.devices.clear();
        sp.property_area = None;
        assert_eq!(installer_refusal(&sp), None);
        sp.devices.push("/dev/dri".into());
        assert!(installer_refusal(&sp).is_some());
        sp.devices.clear();
        sp.rights.push(Right::Net);
        assert!(installer_refusal(&sp).is_some());
        sp.rights.clear();
        sp.links.push(Link {
            target: "/x".into(),
            at: "/y".into(),
        });
        assert!(installer_refusal(&sp).is_some());
        sp.links.clear();
        sp.context_manager = true;
        assert!(installer_refusal(&sp).is_some());
    }

    /// The stub's argv: each grant as its flag and path, then `--` and the program (atl-sandbox-stub's usage line).
    #[test]
    fn stub_argv() {
        let grants: Vec<(Access, PathBuf)> = vec![
            (Access::Ro, "/b/atl-binderd".into()),
            (Access::Rw, "/rw/x".into()),
            (Access::Dev, "/dev/dri".into()),
        ];
        assert_eq!(
            s(&stub_args(&grants, false, Path::new("/b/atl-binderd"))).join(" "),
            "--ro /b/atl-binderd --rw /rw/x --device /dev/dri -- /b/atl-binderd"
        );
        assert_eq!(
            s(&stub_args(&grants, true, Path::new("/b/atl-binderd"))).join(" "),
            "--net --ro /b/atl-binderd --rw /rw/x --device /dev/dri -- /b/atl-binderd"
        );
        // INTERNET is one table row; no permission gives no right
        assert_eq!(
            rights_of(&["android.permission.INTERNET"]),
            vec![Right::Net]
        );
        assert!(rights_of::<&str>(&[]).is_empty());
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
    fn nspid() {
        // proc(5): host pid first, the process's own namespace last
        assert_eq!(
            innermost_pid("Name:\tapp_process\nPid:\t1113415\nNSpid:\t1113415\t2\nNSpgid:\t1\n"),
            Some(2)
        );
        assert_eq!(innermost_pid("NSpid:\t77\n"), Some(77));
        assert_eq!(innermost_pid("Pid:\t77\n"), None);
    }

    /// A shell standing in for bwrap's init, its one child for the program: with no pid namespace of their own, the child's NSpid is the pid the shell printed (`$!`, after the fork).
    #[test]
    #[expect(
        clippy::disallowed_types,
        reason = "a test: an unsandboxed process tree stands in for a sandbox's"
    )]
    fn program_pid_of_a_live_tree() {
        let mut init = std::process::Command::new("sh")
            .args(["-c", "sleep 5 & echo $!; wait"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh");
        let mut line = String::new();
        io::BufRead::read_line(
            &mut io::BufReader::new(init.stdout.take().expect("stdout")),
            &mut line,
        )
        .expect("read");
        let got = program_pid(i32::try_from(init.id()).expect("pid"));
        let _ = init.kill();
        let _ = init.wait();
        assert_eq!(
            got.expect("program_pid"),
            line.trim().parse::<i32>().expect("$!")
        );
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
