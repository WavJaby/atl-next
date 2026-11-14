// SPDX-License-Identifier: MIT
//! bubblewrap's command line, spelled once: every bwrap flag any of our code passes is a method here (F1.1). Users: the broker's profiles and xtask's build-tree namespace (`tree::namespace_args`), a different concept that shares only the flag spelling.
//! Flag meanings: bubblewrap's bwrap.c `usage()` / bwrap(1). Order matters to bwrap (mounts apply in argv order), so the builder keeps call order.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The sandbox program.
pub const PROGRAM: &str = "bwrap";

/// What the sandbox may do at a path: the Landlock grant the stub applies (`atl-sandbox-stub --ro/--rw/--dev`). Each bind method records the grant its kind implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// read, list, execute
    Ro,
    /// Ro plus write, create, remove, rename
    Rw,
    /// Rw plus the device ioctls: a device node
    Dev,
}

/// bwrap's options, in call order ([`Args::end`] closes them with `--`), and the grants of the paths they show: the mounts and the Landlock rules come from one list, so they cannot disagree.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Args {
    argv: Vec<OsString>,
    grants: Vec<(Access, PathBuf)>,
}

impl Args {
    pub fn new() -> Self {
        Self::default()
    }

    fn word(&mut self, w: impl Into<OsString>) -> &mut Self {
        self.argv.push(w.into());
        self
    }

    /// Records `access` at `at` (a sandbox path) for a fresh mount bwrap makes itself (tmpfs, dev, proc): no bind method records those.
    pub fn grant(&mut self, access: Access, at: impl AsRef<Path>) -> &mut Self {
        self.grants.push((access, at.as_ref().to_path_buf()));
        self
    }

    /// The grants recorded so far, in call order.
    pub fn grants(&self) -> &[(Access, PathBuf)] {
        &self.grants
    }

    fn pair(&mut self, flag: &str, a: impl Into<OsString>, b: impl Into<OsString>) -> &mut Self {
        self.word(flag).word(a).word(b)
    }

    /// `--unshare-user`: a new user namespace.
    pub fn unshare_user(&mut self) -> &mut Self {
        self.word("--unshare-user")
    }

    /// `--unshare-pid`: a new pid namespace; the program's sandbox ends when its pid 1 does.
    pub fn unshare_pid(&mut self) -> &mut Self {
        self.word("--unshare-pid")
    }

    /// `--unshare-net`: a new network namespace with only loopback.
    pub fn unshare_net(&mut self) -> &mut Self {
        self.word("--unshare-net")
    }

    /// `--unshare-all`: user (try), ipc, pid, net, uts, cgroup (try) namespaces.
    pub fn unshare_all(&mut self) -> &mut Self {
        self.word("--unshare-all")
    }

    /// `--uid U --gid U`: the uid and gid inside the user namespace (mapped to the caller's).
    pub fn ids(&mut self, id: u32) -> &mut Self {
        self.word("--uid")
            .word(id.to_string())
            .word("--gid")
            .word(id.to_string())
    }

    /// `--userns-block-fd FD`: bwrap writes no id map; the sandbox's first process waits until FD becomes readable, by which time the caller has written the maps (newuidmap/newgidmap on the `--info-fd` child pid). Its ids are then the caller's ids under those maps.
    pub fn userns_block_fd(&mut self, fd: i32) -> &mut Self {
        self.word("--userns-block-fd").word(fd.to_string())
    }

    /// `--cap-add CAP`: keep CAP (e.g. `CAP_SETUID`) inside the user namespace for the program.
    pub fn cap_add(&mut self, cap: &str) -> &mut Self {
        self.word("--cap-add").word(cap)
    }

    /// `--die-with-parent`: the sandbox is killed when bwrap's parent dies.
    pub fn die_with_parent(&mut self) -> &mut Self {
        self.word("--die-with-parent")
    }

    /// `--new-session`: setsid, so the program cannot reach the caller's terminal (TIOCSTI).
    pub fn new_session(&mut self) -> &mut Self {
        self.word("--new-session")
    }

    /// `--cap-drop ALL`: no capabilities inside the namespace either.
    pub fn cap_drop_all(&mut self) -> &mut Self {
        self.word("--cap-drop").word("ALL")
    }

    /// `--info-fd FD`: bwrap writes `{"child-pid": N, ...}` there once the sandbox's first process exists.
    pub fn info_fd(&mut self, fd: i32) -> &mut Self {
        self.word("--info-fd").word(fd.to_string())
    }

    /// `--seccomp FD`: a `struct sock_filter` array installed before the program runs.
    pub fn seccomp(&mut self, fd: i32) -> &mut Self {
        self.word("--seccomp").word(fd.to_string())
    }

    /// `--tmpfs AT`.
    pub fn tmpfs(&mut self, at: impl AsRef<Path>) -> &mut Self {
        self.word("--tmpfs").word(at.as_ref())
    }

    /// `--dev AT`: a fresh minimal /dev (null, zero, full, random, urandom, tty, pts, shm).
    pub fn dev(&mut self, at: impl AsRef<Path>) -> &mut Self {
        self.word("--dev").word(at.as_ref())
    }

    /// `--dir AT`: an empty directory, owned and writable by the sandbox's uid.
    pub fn dir(&mut self, at: impl AsRef<Path>) -> &mut Self {
        self.word("--dir").word(at.as_ref())
    }

    /// `--proc AT`: procfs of the sandbox's pid namespace.
    pub fn proc(&mut self, at: impl AsRef<Path>) -> &mut Self {
        self.word("--proc").word(at.as_ref())
    }

    /// `--bind HOST AT` (writable): grant Rw at AT.
    pub fn bind(&mut self, host: impl AsRef<Path>, at: impl AsRef<Path>) -> &mut Self {
        self.grant(Access::Rw, at.as_ref());
        self.pair("--bind", host.as_ref(), at.as_ref())
    }

    /// `--ro-bind HOST AT`: grant Ro at AT.
    pub fn ro_bind(&mut self, host: impl AsRef<Path>, at: impl AsRef<Path>) -> &mut Self {
        self.grant(Access::Ro, at.as_ref());
        self.pair("--ro-bind", host.as_ref(), at.as_ref())
    }

    /// `--bind` or `--ro-bind` by `writable`.
    pub fn mount(
        &mut self,
        host: impl AsRef<Path>,
        at: impl AsRef<Path>,
        writable: bool,
    ) -> &mut Self {
        if writable {
            self.bind(host, at)
        } else {
            self.ro_bind(host, at)
        }
    }

    /// `--dev-bind HOST AT`: a bind that keeps device nodes usable: grant Dev at AT.
    pub fn dev_bind(&mut self, host: impl AsRef<Path>, at: impl AsRef<Path>) -> &mut Self {
        self.grant(Access::Dev, at.as_ref());
        self.pair("--dev-bind", host.as_ref(), at.as_ref())
    }

    /// `--symlink TARGET AT`: `at` links to `target`.
    pub fn symlink(&mut self, target: impl AsRef<Path>, at: impl AsRef<Path>) -> &mut Self {
        self.pair("--symlink", target.as_ref(), at.as_ref())
    }

    /// `--chdir DIR`: the program's working directory.
    pub fn chdir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.word("--chdir").word(dir.as_ref())
    }

    /// The options alone, without `--`: spliced before another argv's options (the adb profile's block fd, known only to its spawn).
    pub fn options(self) -> Vec<OsString> {
        self.argv
    }

    /// The options and the closing `--`; the program and its arguments follow.
    pub fn end(mut self) -> Vec<OsString> {
        self.argv.push("--".into());
        self.argv
    }
}
