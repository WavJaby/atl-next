// SPDX-License-Identifier: MIT
//! atl-sandbox-join: the broker's stage that starts a sandbox *inside* an app sandbox's pid namespace (a WebKit web process: WebKit takes its child's pid from SCM credentials and
//! asserts it non-zero, which holds only for a process its own pid namespace can see).
//!
//! `atl-sandbox-join --pidns FD --mntns FD -- PROGRAM ARGS...`: FD = the app sandbox's pid and mount namespaces (the broker opens `/proc/<sandbox pid 1>/ns/{pid,mnt}` and lets this
//! process inherit them). Joins, in this order: the user namespace that owns the pid namespace, the pid namespace, the mount namespace; then runs PROGRAM (bwrap) as its child and exits with its status.
//! - owner, not `/proc/<pid>/ns/user`: bwrap nests a second user namespace for `--uid` when it mounts devpts (`--dev`), so the app's processes sit one below the owner; setns(pid) needs
//!   CAP_SYS_ADMIN over the owner (kernel pidns_install), which joining it gives the broker's uid (its creator).
//! - user before pid: pidns_install also checks the caller's own user namespace; `bwrap --userns --pidns` joins in the other order and fails.
//! - a child, not exec: setns(pid) moves only future children (pid_for_children), and that process may not create a pid namespace itself (copy_pid_ns: EINVAL), which bwrap does.
//! - the mount namespace: bwrap reads `/proc/<its pid>`, which only the app's procfs numbers right; bwrap then builds the new sandbox from the app's view, so it can show nothing the app cannot see.
//! Single-threaded on purpose: setns(CLONE_NEWUSER) refuses a multithreaded caller, the reason this is a process and not code in atld.
//! Rights held here (full capabilities over the app's namespaces, as uid 0 of the owner) end at PROGRAM's exec: bwrap drops them for its sandbox.

use std::ffi::OsString;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::process::{Command, ExitCode};

use rustix::io::{FdFlags, fcntl_setfd};
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};

/// linux/nsfs.h `NS_GET_USERNS` = `_IO(0xb7, 0x1)`: a new fd of the user namespace owning a namespace.
const NS_GET_USERNS: libc::c_ulong = 0xb701;

struct Join {
    pidns: i32,
    mntns: i32,
    program: OsString,
    args: Vec<OsString>,
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Join, String> {
    let (mut pidns, mut mntns) = (None, None);
    let fd = |v: Option<OsString>, flag: &str| -> Result<i32, String> {
        v.and_then(|v| v.to_str()?.parse().ok())
            .filter(|n| *n > 2)
            .ok_or_else(|| format!("{flag} takes an inherited fd number above 2"))
    };
    loop {
        match args.next().as_ref().and_then(|a| a.to_str()) {
            Some("--pidns") => pidns = Some(fd(args.next(), "--pidns")?),
            Some("--mntns") => mntns = Some(fd(args.next(), "--mntns")?),
            Some("--") => break,
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let program = args.next().ok_or("no program after --")?;
    Ok(Join {
        pidns: pidns.ok_or("no --pidns")?,
        mntns: mntns.ok_or("no --mntns")?,
        program,
        args: args.collect(),
    })
}

/// Takes ownership of inherited fd `n` and makes it close-on-exec: the namespaces must not reach PROGRAM.
fn adopt(n: i32) -> Result<OwnedFd, String> {
    // SAFETY: `n` is an fd the broker passed this process for exactly this use (its argv names it); nothing else in this single-threaded process owns or closes it.
    let fd = unsafe { OwnedFd::from_raw_fd(n) };
    fcntl_setfd(&fd, FdFlags::CLOEXEC).map_err(|e| format!("fd {n}: {e}"))?;
    Ok(fd)
}

fn owner_userns(ns: &OwnedFd) -> Result<OwnedFd, String> {
    use std::os::fd::AsRawFd;
    // SAFETY: NS_GET_USERNS takes no argument and returns a new fd or -1; `ns` is a valid open namespace fd for the call's duration.
    let r = unsafe { libc::ioctl(ns.as_raw_fd(), NS_GET_USERNS) };
    if r < 0 {
        return Err(format!(
            "NS_GET_USERNS: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: a successful NS_GET_USERNS returns a new fd this process now owns.
    Ok(unsafe { OwnedFd::from_raw_fd(r) })
}

fn run(j: Join) -> Result<ExitCode, String> {
    let pidns = adopt(j.pidns)?;
    let mntns = adopt(j.mntns)?;
    let owner = owner_userns(&pidns)?;
    move_into_link_name_space(owner.as_fd(), Some(LinkNameSpaceType::User))
        .map_err(|e| format!("joining the pid namespace's owner: {e}"))?;
    move_into_link_name_space(pidns.as_fd(), Some(LinkNameSpaceType::ProcessID))
        .map_err(|e| format!("joining the pid namespace: {e}"))?;
    move_into_link_name_space(mntns.as_fd(), Some(LinkNameSpaceType::Mount))
        .map_err(|e| format!("joining the mount namespace: {e}"))?;
    drop((owner, pidns, mntns));
    #[expect(
        clippy::disallowed_types,
        reason = "the broker's own stage: runs the bwrap command the broker built, in the namespaces just joined"
    )]
    let status = Command::new(&j.program)
        .args(&j.args)
        .status()
        .map_err(|e| format!("starting {}: {e}", j.program.to_string_lossy()))?;
    // bwrap's status, or 128 + signal as a shell reports it
    Ok(ExitCode::from(
        status
            .code()
            .or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                status.signal().map(|s| 128 + s)
            })
            .and_then(|c| u8::try_from(c).ok())
            .unwrap_or(1),
    ))
}

fn main() -> ExitCode {
    match parse(std::env::args_os().skip(1)).and_then(run) {
        Ok(code) => code,
        Err(e) => {
            // stderr is the log of the app that asked (the broker passes it)
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                format!("atl-sandbox-join: {e}\n").as_bytes(),
            );
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> impl Iterator<Item = OsString> {
        s.iter().map(OsString::from).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_fds_and_program() {
        let j = parse(args(&["--pidns", "5", "--mntns", "6", "--", "/usr/bin/bwrap", "--x"]))
            .expect("parse");
        assert_eq!((j.pidns, j.mntns), (5, 6));
        assert_eq!(j.program, "/usr/bin/bwrap");
        assert_eq!(j.args, vec![OsString::from("--x")]);
    }

    /// fds 0-2 are stdio, never a namespace; a missing flag or program is an error, not a default.
    #[test]
    fn refuses_bad_lines() {
        assert!(parse(args(&["--pidns", "1", "--mntns", "6", "--", "p"])).is_err());
        assert!(parse(args(&["--mntns", "6", "--", "p"])).is_err());
        assert!(parse(args(&["--pidns", "5", "--mntns", "6", "--"])).is_err());
        assert!(parse(args(&["--other", "--", "p"])).is_err());
    }
}
