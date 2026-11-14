// SPDX-License-Identifier: MIT
//! The process-control protocol (owner D1, 2026-11-03): the system services start and kill app processes through the host (atld, which owns the broker) over one SEQPACKET connection, one JSON message per packet, strictly request then reply.
//! `Start` is where ProcessList.startProcessLocked ends (Process.start's arguments reduced to what the broker's sandbox needs); `Kill` is Process.killProcess of a process it started. The server refuses to kill pids it did not start.
//! Only the services hold the client end (the broker passes it as their stdin, [`crate::Spawn::stdin`]): an app has no way to ask for a start or a kill.

use std::io;
use std::os::fd::AsFd;

use rustix::net::{RecvFlags, SendFlags, recv, send};
use serde::{Deserialize, Serialize};

/// The largest message either side accepts. A start request is a few hundred bytes (names and a short argument list); `recv` reports a longer packet as an error rather than truncating it.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// What to start: the arguments Process.start passes the zygote for an app (ZygoteProcess.startViaZygote), reduced to what the sandbox needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    /// The process's uid (and gid, as in the sandbox's user namespace map).
    pub uid: u32,
    /// ProcessRecord.processName; names the process's log.
    pub process_name: String,
    /// The package the process belongs to (its data directory).
    pub package: String,
    /// The app's target SDK (zygote's `--target-sdk-version`).
    pub target_sdk: i32,
    /// The class run as `main` (zygote's `entryPoint`: android.app.ActivityThread).
    pub entry_point: String,
    /// Its arguments (`seq=<startSeq>`).
    pub args: Vec<String>,
    /// The package's instruction set (arches.toml row key), from ApplicationInfo.primaryCpuAbi: zygote's `--instruction-set`, as ProcessList passes it.
    pub instruction_set: String,
    /// What the package's install-time grants give the sandbox ([`crate::rights_of`]): ProcessList's gids, which PackageManager derives from the grants.
    pub rights: Vec<crate::Right>,
}

/// `Install` / `Commit` are the installd half of an install (PackageInstallerSession staging + installd): the services own the package list and ask the host for file work only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Start(StartRequest),
    Kill {
        pid: i32,
    },
    /// The pid a started process (`pid`, its tag) has in its own sandbox, as it numbers itself ([`crate::program_pid`]): the services answer an app's own pid in the app's numbering. Answered
    /// [`Response::ProgramPid`].
    ProgramPid {
        pid: i32,
    },
    /// Stage session `session`'s files under their session names (the source file names, as `pm install-multiple` writes them: PackageManagerShellCommand.doWriteSplits) in `/data/app/vmdl<session>.tmp`
    /// and run the installer over it (parse + native-library copy). `uid` (the package's future uid) must be above the installer's own ([`crate::INSTALLER_UID`]). Answered [`Response::Staged`]; a failed
    /// installer run removes the staging dir and answers its stderr.
    Install {
        session: u32,
        uid: u32,
    },
    /// Rename each staged file to the name its parsed split gives (`names`: PackageInstallerSession's uniform naming, ApkLiteParseUtils.splitNameToFileName), then the staged dir to
    /// `/data/app/<package>`, and create the package's data dirs (installd createAppData): the services decoded the frames and accepted them. `names` covers every staged file once.
    Commit {
        session: u32,
        package: String,
        uid: u32,
        names: Vec<StagedName>,
    },
    /// PMS's system scan of one preinstalled package dir (`dir`: its Android path, one of atl-services-types `system_packages::scan`'s dirs, e.g. `/product/app/Fixture`): the installer parses it
    /// where it lies (read-only partition, no staging, no native-library copy). No uid yet: PMS assigns the appId only to a package the scan accepts. Answered [`Response::Staged`]; a failed run
    /// answers its stderr.
    Scan {
        dir: String,
    },
    /// The services accepted scan `dir` as `package`: create its data dirs (installd createAppData, as for any package) and let start requests name it. Answered [`Response::Committed`].
    ScanCommit {
        dir: String,
        package: String,
        uid: u32,
    },
}

/// One staged file's rename at commit: `staged` (its session name) -> `name` (`base.apk` or `split_<split name>.apk`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedName {
    pub staged: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// The process runs; `pid` is the tag binderd reports as its calling pid (what attachApplication sees).
    Started {
        pid: i32,
    },
    /// The sandbox was killed.
    Killed,
    /// The process's pid in its own sandbox.
    ProgramPid {
        pid: i32,
    },
    /// The installer ran: its frames (atl-services-types installer_output) at `frames`, a host path the services read.
    Staged {
        frames: String,
    },
    /// The package is in place; `needs_es1`: a native library of it DT_NEEDs libGLESv1_CM (the ES1 rule's file fact; the services choose the driver).
    Committed {
        needs_es1: bool,
    },
    Error {
        message: String,
    },
}

fn encode<T: Serialize>(m: &T) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(m).map_err(io::Error::other)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::other(format!(
            "control message of {} bytes (limit {MAX_MESSAGE})",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// The next message, `None` at end of connection.
fn read<T: for<'de> Deserialize<'de>>(fd: impl AsFd) -> io::Result<Option<T>> {
    // one byte more than the limit tells a longer packet from one of exactly the limit
    let mut buf = vec![0u8; MAX_MESSAGE + 1];
    let n = recv(fd, &mut buf, RecvFlags::empty())?.0;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_MESSAGE {
        return Err(io::Error::other(format!(
            "control message over {MAX_MESSAGE} bytes"
        )));
    }
    serde_json::from_slice(&buf[..n])
        .map(Some)
        .map_err(io::Error::other)
}

fn write<T: Serialize>(fd: impl AsFd, m: &T) -> io::Result<()> {
    let bytes = encode(m)?;
    let sent = send(fd, &bytes, SendFlags::empty())?;
    if sent != bytes.len() {
        return Err(io::Error::other("short write on a SEQPACKET socket"));
    }
    Ok(())
}

/// The client's side: sends `request` and waits for the reply. The server ending the connection is an error (`UnexpectedEof`).
pub fn call(fd: impl AsFd, request: &Request) -> io::Result<Response> {
    write(fd.as_fd(), request)?;
    read(fd)?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
}

/// The server's side: answers each request with `handle` until the client ends the connection.
pub fn serve(fd: impl AsFd, mut handle: impl FnMut(Request) -> Response) -> io::Result<()> {
    while let Some(request) = read::<Request>(fd.as_fd())? {
        write(fd.as_fd(), &handle(request))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    fn pair() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair")
    }

    fn start() -> StartRequest {
        StartRequest {
            uid: 10_123,
            process_name: "com.example".into(),
            package: "com.example".into(),
            target_sdk: 36,
            entry_point: "android.app.ActivityThread".into(),
            args: vec!["seq=1".into()],
            instruction_set: "x86_64".into(),
            rights: vec![crate::Right::Net],
        }
    }

    /// A request and its reply cross the socket intact, in order; the server returns when the client closes its end.
    #[test]
    fn request_reply_roundtrip() {
        let (client, server) = pair();
        let thread = std::thread::spawn(move || {
            let mut seen = Vec::new();
            serve(&server, |r| {
                seen.push(r.clone());
                match r {
                    Request::Start(_) => Response::Started { pid: 4242 },
                    Request::Kill { pid: 4242 } => Response::Killed,
                    Request::Kill { pid } => Response::Error {
                        message: format!("unknown pid {pid}"),
                    },
                    other => Response::Error {
                        message: format!("unexpected {other:?}"),
                    },
                }
            })
            .expect("serve");
            seen
        });
        assert_eq!(
            call(&client, &Request::Start(start())).expect("start"),
            Response::Started { pid: 4242 }
        );
        assert_eq!(
            call(&client, &Request::Kill { pid: 4242 }).expect("kill"),
            Response::Killed
        );
        assert_eq!(
            call(&client, &Request::Kill { pid: 7 }).expect("kill"),
            Response::Error {
                message: "unknown pid 7".into()
            }
        );
        drop(client);
        assert_eq!(
            thread.join().expect("server thread"),
            vec![
                Request::Start(start()),
                Request::Kill { pid: 4242 },
                Request::Kill { pid: 7 }
            ]
        );
    }

    /// The wire form is the documented JSON: a tagged object per message.
    #[test]
    fn wire_form() {
        let json = serde_json::to_string(&Request::Kill { pid: 5 }).expect("encode");
        assert_eq!(json, r#"{"op":"kill","pid":5}"#);
        let back: Response =
            serde_json::from_str(r#"{"status":"started","pid":9}"#).expect("decode");
        assert_eq!(back, Response::Started { pid: 9 });
    }

    /// A client that goes away mid-call is an error, not a hang.
    #[test]
    fn server_gone() {
        let (client, server) = pair();
        drop(server);
        assert!(call(&client, &Request::Kill { pid: 1 }).is_err());
    }
}
