// SPDX-License-Identifier: MIT
//! ProcessControl (maintainer D1, 2026-11-03): how the system services start and kill app processes. The seam stands where AOSP's ProcessList meets the zygote (`Process.start`) and `Process.killProcess`;
//! the one implementation is a control socket to atld, which spawns through the broker's sandbox and kills the sandbox ([`atl_broker::control`] is the protocol). The trait is shaped by that implementation:
//! a start names the process and returns the pid binderd will report for it (what `attachApplication` sees as the calling pid), a kill takes that pid, and so does the lookup of the pid the process has
//! inside its own sandbox (getRunningAppProcesses answers an app's own pid in its numbering).

use std::io;
use std::os::fd::OwnedFd;

use atl_broker::control::{self, Request, Response, StagedName, StartRequest};

pub trait ProcessControl: Send + Sync {
    /// Starts the process; its pid (the binderd tag).
    fn start(&self, request: &StartRequest) -> io::Result<i32>;
    /// Kills a process this control started, with everything in its sandbox.
    fn kill(&self, pid: i32) -> io::Result<()>;
    /// The pid a process this control started has in its own sandbox: what it reads as Process.myPid().
    fn program_pid(&self, pid: i32) -> io::Result<i32>;
    /// The death of `pid` ended the last app (control `LastAppEnded`); the host decides whether the run ends.
    fn last_app_ended(&self, pid: i32, finished: bool) -> io::Result<()>;
}

/// A scan's answer: the frames file's host path, or the installer's parse verdict on the package (`Refused`: PMS skips it). Any other end is an error.
pub enum ScanReply {
    Staged(String),
    Refused(String),
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

    /// The host's default network now, as `DefaultNetwork::to_arg` writes it (atld reads it outside the sandbox); None = no default route.
    pub fn network(&self) -> io::Result<Option<String>> {
        match self.call(&Request::Network)? {
            Response::Network { network } => Ok(network),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("network answered {other:?}"))),
        }
    }

    /// Renames session `session`'s files to `names`, moves it to `/data/app/<package>` and creates its data dirs.
    pub fn commit(
        &self,
        session: u32,
        package: &str,
        uid: u32,
        names: Vec<StagedName>,
    ) -> io::Result<()> {
        match self.call(&Request::Commit {
            session,
            package: package.to_owned(),
            uid,
            names,
        })? {
            Response::Committed => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("commit answered {other:?}"))),
        }
    }

    /// Session `session` was refused as a downgrade (`version` below `installed`): the installer parses `package`'s installed code in place under it instead; the frames file's host path.
    pub fn reinstate(
        &self,
        session: u32,
        package: &str,
        uid: u32,
        version: i64,
        installed: i64,
    ) -> io::Result<String> {
        match self.call(&Request::Reinstate {
            session,
            package: package.to_owned(),
            uid,
            version,
            installed,
        })? {
            Response::Staged { frames } => Ok(frames),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("reinstate answered {other:?}"))),
        }
    }

    /// Runs the installer over the preinstalled package dir `dir` (an Android path): the frames file's host path, or its parse verdict.
    pub fn scan(&self, dir: &str) -> io::Result<ScanReply> {
        match self.call(&Request::Scan {
            dir: dir.to_owned(),
        })? {
            Response::Staged { frames } => Ok(ScanReply::Staged(frames)),
            Response::Refused { message } => Ok(ScanReply::Refused(message)),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("scan answered {other:?}"))),
        }
    }

    /// Accepts scan `dir` as `package` of `uid`: its data dirs are created.
    pub fn scan_commit(&self, dir: &str, package: &str, uid: u32) -> io::Result<()> {
        match self.call(&Request::ScanCommit {
            dir: dir.to_owned(),
            package: package.to_owned(),
            uid,
        })? {
            Response::Committed => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("scan commit answered {other:?}"))),
        }
    }
}

/// The control socket's display half: the notification service's requests, shown by atld (notifications plan N2).
#[cfg(feature = "converter")]
impl crate::notification::DesktopNotifications for ControlSocket {
    fn notify(
        &self,
        package: &str,
        key: &str,
        content: control::NotificationContent,
    ) -> io::Result<control::NotifyOutcome> {
        match self.call(&Request::Notify {
            package: package.to_owned(),
            key: key.to_owned(),
            content,
        })? {
            Response::Notified { outcome } => Ok(outcome),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("notify answered {other:?}"))),
        }
    }

    fn withdraw(&self, key: &str) -> io::Result<bool> {
        match self.call(&Request::Withdraw {
            key: key.to_owned(),
        })? {
            Response::Withdrawn { shown } => Ok(shown),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("withdraw answered {other:?}"))),
        }
    }
}

/// The control socket to atld. Calls overlap: `control::Client` routes each reply to the call that asked for it, so a slow install does not hold up a start.
pub struct ControlSocket {
    client: control::Client,
}

impl ControlSocket {
    /// The connection atld passes as the services' stdin ([`atl_broker::Spawn::stdin`]).
    pub fn from_stdin() -> io::Result<Self> {
        Self::over(rustix::stdio::stdin().try_clone_to_owned()?)
    }

    fn over(fd: OwnedFd) -> io::Result<Self> {
        Ok(ControlSocket {
            client: control::Client::new(fd)?,
        })
    }

    /// A socket to a test's fake atld.
    #[cfg(test)]
    pub(crate) fn new(fd: OwnedFd) -> Self {
        Self::over(fd).expect("control reader thread")
    }

    fn call(&self, request: &Request) -> io::Result<Response> {
        self.client.call(request)
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

    fn program_pid(&self, pid: i32) -> io::Result<i32> {
        match self.call(&Request::ProgramPid { pid })? {
            Response::ProgramPid { pid } => Ok(pid),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!("program pid answered {other:?}"))),
        }
    }

    fn last_app_ended(&self, pid: i32, finished: bool) -> io::Result<()> {
        match self.call(&Request::LastAppEnded { pid, finished })? {
            Response::Noted => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(io::Error::other(format!(
                "last app ended answered {other:?}"
            ))),
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

    /// The trait over the socket against a server that starts one process (pid 77, pid 2 in its sandbox) and refuses any other kill.
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
                Request::ProgramPid { pid: 77 } => Response::ProgramPid { pid: 2 },
                other => Response::Error {
                    message: format!("unexpected {other:?}"),
                },
            })
            .expect("serve");
        });
        let pc = ControlSocket::new(ours);
        assert_eq!(pc.start(&request()).expect("start"), 77);
        assert_eq!(pc.program_pid(77).expect("program pid"), 2);
        pc.kill(77).expect("kill");
        let err = pc.kill(5).expect_err("a pid nobody started");
        assert!(err.to_string().contains("not started: 5"), "{err}");
        drop(pc);
        server.join().expect("server");
    }

    /// The fake atld of the next test: `Install` reports its arrival, then waits for a `Start`; the `Start` releases it. Both lanes run this; only the routing decides the thread.
    struct FakeAtld {
        install_seen: std::sync::Mutex<std::sync::mpsc::Sender<()>>,
        start_tx: std::sync::Mutex<std::sync::mpsc::Sender<()>>,
        start_rx: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }

    fn fake_atld(fake: &FakeAtld, r: Request) -> Response {
        match r {
            Request::Start(s) if s == request() => {
                fake.start_tx
                    .lock()
                    .expect("start")
                    .send(())
                    .expect("release");
                Response::Started { pid: 77 }
            }
            Request::Install {
                session: 1,
                uid: 10_123,
            } => {
                fake.install_seen
                    .lock()
                    .expect("seen")
                    .send(())
                    .expect("seen");
                let released = fake
                    .start_rx
                    .lock()
                    .expect("wait")
                    .recv_timeout(std::time::Duration::from_secs(5));
                match released {
                    Ok(()) => Response::Staged {
                        frames: "frames".into(),
                    },
                    Err(_) => Response::Error {
                        message: "no Start while the Install waited".into(),
                    },
                }
            }
            other => Response::Error {
                message: format!("unexpected {other:?}"),
            },
        }
    }

    /// `Install` in flight does not hold up a `Start` sent after it: atld answers the Start on its serving thread while the installer lane waits for it, and both calls finish.
    #[test]
    fn start_is_not_held_behind_an_install() {
        use std::sync::{Arc, Mutex, mpsc};
        use std::time::Duration;
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair");
        let (install_seen_tx, install_seen_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let fake = Arc::new(FakeAtld {
            install_seen: Mutex::new(install_seen_tx),
            start_tx: Mutex::new(start_tx),
            start_rx: Mutex::new(start_rx),
        });
        let inline_fake = Arc::clone(&fake);
        let atld = std::thread::spawn(move || {
            control::serve_lanes(
                theirs,
                |r| fake_atld(&inline_fake, r),
                move |r| fake_atld(&fake, r),
            )
            .expect("serve_lanes");
        });
        let pc = Arc::new(ControlSocket::new(ours));
        let (install_done, install_result) = mpsc::channel();
        let installing = Arc::clone(&pc);
        std::thread::spawn(move || {
            install_done.send(installing.install(1, 10_123)).ok();
        });
        install_seen_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the Install reached atld");
        let (start_done, start_result) = mpsc::channel();
        let starting = Arc::clone(&pc);
        std::thread::spawn(move || {
            start_done.send(starting.start(&request())).ok();
        });
        let started = start_result
            .recv_timeout(Duration::from_secs(5))
            .expect("the Start returned while the Install was in flight");
        assert_eq!(started.expect("start"), 77);
        let installed = install_result
            .recv_timeout(Duration::from_secs(5))
            .expect("the Install returned");
        assert_eq!(installed.expect("install"), "frames");
        drop(pc);
        atld.join().expect("atld");
    }
}
