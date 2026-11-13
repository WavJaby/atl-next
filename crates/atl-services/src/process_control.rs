// SPDX-License-Identifier: MIT
//! ProcessControl (owner D1, 2026-11-03): how the system services start and kill app processes. The seam stands where AOSP's ProcessList meets the zygote (`Process.start`) and `Process.killProcess`;
//! the one implementation is a control socket to atld, which spawns through the broker's sandbox and kills the sandbox ([`atl_broker::control`] is the protocol). The trait is shaped by that implementation:
//! a start names the process and returns the pid binderd will report for it (what `attachApplication` sees as the calling pid), a kill takes that pid.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

use atl_broker::control::{self, Request, Response, StartRequest};

pub trait ProcessControl: Send + Sync {
    /// Starts the process; its pid (the binderd tag).
    fn start(&self, request: &StartRequest) -> io::Result<i32>;
    /// Kills a process this control started, with everything in its sandbox.
    fn kill(&self, pid: i32) -> io::Result<()>;
}

/// The control socket's install half (installd's role): staging + the installer run, then the commit (the services' boot-time install, system_services.rs).
impl ControlSocket {
    /// Stages session `session` for `uid` and runs the installer: the frames file's host path.
    pub fn install(&self, session: u32, uid: u32) -> io::Result<String> {
        match self.call(&Request::Install { session, uid })? {
            Response::Staged { frames } => Ok(frames),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("install answered {other:?}"))),
        }
    }

    /// Moves session `session` to `/data/app/<package>` and creates its data dirs: whether a library needs ES1.
    pub fn commit(&self, session: u32, package: &str, uid: u32) -> io::Result<bool> {
        match self.call(&Request::Commit {
            session,
            package: package.to_owned(),
            uid,
        })? {
            Response::Committed { needs_es1 } => Ok(needs_es1),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("commit answered {other:?}"))),
        }
    }
}

/// The control socket to atld. One request is in flight at a time: the protocol is request then reply on one connection.
pub struct ControlSocket {
    fd: Mutex<OwnedFd>,
}

impl ControlSocket {
    /// The connection atld passes as the services' stdin ([`atl_broker::Spawn::stdin`]).
    pub fn from_stdin() -> io::Result<Self> {
        Ok(ControlSocket {
            fd: Mutex::new(rustix::stdio::stdin().try_clone_to_owned()?),
        })
    }

    #[cfg(test)]
    fn new(fd: OwnedFd) -> Self {
        ControlSocket { fd: Mutex::new(fd) }
    }

    fn call(&self, request: &Request) -> io::Result<Response> {
        // a panic mid-call leaves the socket usable: the next request starts a fresh exchange
        let fd = self.fd.lock().unwrap_or_else(|e| e.into_inner());
        control::call(&*fd, request)
    }
}

impl ProcessControl for ControlSocket {
    fn start(&self, request: &StartRequest) -> io::Result<i32> {
        match self.call(&Request::Start(request.clone()))? {
            Response::Started { pid } => Ok(pid),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("start answered {other:?}"))),
        }
    }

    fn kill(&self, pid: i32) -> io::Result<()> {
        match self.call(&Request::Kill { pid })? {
            Response::Killed => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("kill answered {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    fn request() -> StartRequest {
        StartRequest {
            uid: 10_123,
            process_name: "p".into(),
            package: "p".into(),
            target_sdk: 36,
            entry_point: "android.app.ActivityThread".into(),
            args: vec!["seq=3".into()],
            instruction_set: "x86_64".into(),
            rights: vec![],
        }
    }

    /// The trait over the socket against a server that starts one process (pid 77) and refuses any other kill.
    #[test]
    fn start_and_kill_over_the_socket() {
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair");
        let server = std::thread::spawn(move || {
            control::serve(&theirs, |r| match r {
                Request::Start(s) if s == request() => Response::Started { pid: 77 },
                Request::Start(_) => Response::Error {
                    message: "unexpected start".into(),
                },
                Request::Kill { pid: 77 } => Response::Killed,
                Request::Kill { pid } => Response::Error {
                    message: format!("not started: {pid}"),
                },
                other => Response::Error {
                    message: format!("unexpected {other:?}"),
                },
            })
            .expect("serve");
        });
        let pc = ControlSocket::new(ours);
        assert_eq!(pc.start(&request()).expect("start"), 77);
        pc.kill(77).expect("kill");
        let err = pc.kill(5).expect_err("a pid nobody started");
        assert!(err.to_string().contains("not started: 5"), "{err}");
        drop(pc);
        server.join().expect("server");
    }
}
