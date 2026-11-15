// SPDX-License-Identifier: MIT
//! flatpak-spawn as an app sandbox shows it (atl_broker::webspawn): the subset of Flatpak's `flatpak-spawn` CLI WebKitGTK uses (FlatpakLauncher.cpp, ProcessLauncherGLib.cpp), turned into one request
//! on the app's web-process channel. Nothing runs here: the broker starts the process, or refuses. This program is no security boundary (it runs with the app's rights); the broker's check is.
//!
//! Accepted: `--forward-fd=N` (once), `--expose-pids`, `--watch-bus`, `--sandbox`, `--directory=D`, `--no-network`, `--sandbox-flag=F`, `--sandbox-expose-path[-ro][-try]=P`, `--env=K=V`, then
//! PROGRAM ARGS. `--expose-pids` and `--watch-bus` hold by construction: the process lives in a child pid namespace of the app's and dies with it. Any other option: exit 1.
//! A line without `--forward-fd` is WebKit's usability probe (`--sandbox ... echo`, run without inherited fds, so the channel is unreachable): exit 0 when the broker gave this sandbox a channel
//! ([`FD_ENV`] set) and `--sandbox` is asked, else 1; it starts nothing. Exit 0 after a start, 1 with the broker's reason on stderr otherwise.

use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd};
use std::process::ExitCode;

use atl_broker::webspawn::{Expose, FD_ENV, Reply, Request, read_reply, send_request};
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

/// A parsed line: the request, or None for a probe line (no `--forward-fd`).
fn parse(args: &[String]) -> Result<(Option<i32>, Request), String> {
    let mut req = Request {
        program: String::new(),
        args: vec![],
        forward_fd: -1,
        env: vec![],
        directory: "/".into(),
        sandbox: false,
        network: true,
        flags: vec![],
        expose: vec![],
    };
    let mut forward = None;
    let mut rest = args.iter();
    for a in rest.by_ref() {
        let Some(opt) = a.strip_prefix("--") else {
            req.program = a.clone();
            break;
        };
        let (name, value) = match opt.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (opt, None),
        };
        match (name, value) {
            ("forward-fd", Some(v)) if forward.is_none() => {
                forward = Some(v.parse().map_err(|_| format!("--forward-fd={v}"))?)
            }
            ("expose-pids" | "watch-bus", None) => {}
            ("sandbox", None) => req.sandbox = true,
            ("no-network", None) => req.network = false,
            ("directory", Some(v)) => req.directory = v.into(),
            ("sandbox-flag", Some(v)) => req.flags.push(v.into()),
            ("env", Some(v)) => {
                let (k, val) = v.split_once('=').ok_or_else(|| format!("--env={v}"))?;
                req.env.push((k.into(), val.into()));
            }
            (
                "sandbox-expose-path"
                | "sandbox-expose-path-ro"
                | "sandbox-expose-path-try"
                | "sandbox-expose-path-ro-try",
                Some(v),
            ) => req.expose.push(Expose {
                path: v.into(),
                writable: !name.contains("-ro"),
                must_exist: !name.ends_with("-try"),
            }),
            _ => return Err(format!("unsupported option {a}")),
        }
    }
    if req.program.is_empty() {
        return Err("no program".into());
    }
    req.args = rest.cloned().collect();
    if let Some(fd) = forward {
        req.forward_fd = fd;
    }
    Ok((forward, req))
}

/// Inherited fd `n`, if open.
fn inherited(n: i32) -> Option<BorrowedFd<'static>> {
    // SAFETY: F_GETFD only reads the fd's flags; -1 = not open
    let open = n >= 0 && unsafe { libc::fcntl(n, libc::F_GETFD) } >= 0;
    // SAFETY: `n` is open (checked above), inherited by this process and never closed by it, so it stays valid for the process's life
    open.then(|| unsafe { BorrowedFd::borrow_raw(n) })
}

fn channel() -> Option<BorrowedFd<'static>> {
    inherited(std::env::var(FD_ENV).ok()?.parse().ok()?)
}

fn spawn(forward: i32, req: &Request) -> Result<(), String> {
    let chan = channel().ok_or_else(|| format!("no web-process channel ({FD_ENV})"))?;
    let socket = inherited(forward).ok_or_else(|| format!("--forward-fd={forward}: not open"))?;
    let (ours, theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| format!("reply socket: {e}"))?;
    let (out, err) = (std::io::stdout(), std::io::stderr());
    send_request(
        chan,
        req,
        [socket, theirs.as_fd(), out.as_fd(), err.as_fd()],
    )
    .map_err(|e| format!("sending the request: {e}"))?;
    drop(theirs);
    match read_reply(&ours).map_err(|e| format!("reading the reply: {e}"))? {
        Reply::Started { .. } => Ok(()),
        Reply::Refused { what, why } => Err(format!("refused ({what}): {why}")),
        Reply::Failed { message } => Err(format!("failed: {message}")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = parse(&args).and_then(|(forward, req)| match forward {
        Some(fd) => spawn(fd, &req),
        None if req.sandbox && std::env::var_os(FD_ENV).is_some() => Ok(()),
        None => Err(format!("cannot sandbox: no web-process channel ({FD_ENV})")),
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "flatpak-spawn: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    /// The web-process line WebKit 2.54 builds (FlatpakLauncher.cpp; flags as W0 logged them).
    #[test]
    fn parses_webkits_line() {
        let (fwd, req) = parse(&line(
            "--forward-fd=15 --expose-pids --watch-bus --sandbox --directory=/ --no-network --sandbox-flag=share-gpu --sandbox-flag=allow-dbus \
             --sandbox-expose-path-ro=/data/x --sandbox-expose-path=/data/y --env=A=b=c /usr/lib/webkitgtk-6.0/WebKitWebProcess 7 15",
        ))
        .expect("parse");
        assert_eq!(fwd, Some(15));
        assert_eq!(req.forward_fd, 15);
        assert!(req.sandbox && !req.network);
        assert_eq!(req.flags, vec!["share-gpu", "allow-dbus"]);
        assert_eq!(
            req.expose,
            vec![
                Expose {
                    path: "/data/x".into(),
                    writable: false,
                    must_exist: true
                },
                Expose {
                    path: "/data/y".into(),
                    writable: true,
                    must_exist: true
                }
            ]
        );
        assert_eq!(req.env, vec![("A".into(), "b=c".into())]);
        assert_eq!(req.program, "/usr/lib/webkitgtk-6.0/WebKitWebProcess");
        assert_eq!(req.args, vec!["7", "15"]);
    }

    /// WebKit's probe (ProcessLauncherGLib.cpp isFlatpakSpawnUsable): no fd to forward.
    #[test]
    fn probe_line_has_no_forward_fd() {
        let (fwd, req) = parse(&line(
            "--sandbox --directory=/ --sandbox-expose-path-ro-try=/this_path_doesnt_exist echo",
        ))
        .expect("parse");
        assert_eq!(fwd, None);
        assert!(req.sandbox);
        assert!(!req.expose[0].must_exist && !req.expose[0].writable);
    }

    #[test]
    fn unknown_options_fail() {
        assert!(parse(&line("--host echo")).is_err());
        assert!(parse(&line("--sandbox-a11y-own-name=x echo")).is_err());
        assert!(parse(&line("--forward-fd=3 --forward-fd=4 echo")).is_err());
        assert!(parse(&line("--sandbox")).is_err());
    }
}
