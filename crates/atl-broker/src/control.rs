// SPDX-License-Identifier: MIT
//! The process-control protocol (maintainer D1, 2026-11-03): the system services start and kill app processes through the host (atld, which owns the broker) over one SEQPACKET connection, one JSON message per packet.
//! Every message is an [`Envelope`] carrying the id of its call, so calls may overlap and replies may overtake each other: [`Client`] matches replies to calls, [`serve_lanes`] answers them.
//! `Start` is where ProcessList.startProcessLocked ends (Process.start's arguments reduced to what the broker's sandbox needs); `Kill` is Process.killProcess of a process it started. The server refuses to kill pids it did not start.
//! `Notify` / `Withdraw` carry the notification service's display requests: the services have no session bus, so atld shows them on the desktop.
//! Only the services hold the client end (the broker passes it as their stdin, [`crate::Spawn::stdin`]): an app has no way to ask for a start or a kill.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;

use rustix::net::{RecvFlags, SendFlags, Shutdown, recv, send, shutdown};
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

/// One message on the wire: a request or a reply, with the id of the call it belongs to. The body's fields sit beside `id`: `{"id":5,"op":"kill","pid":5}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub id: u64,
    #[serde(flatten)]
    pub body: T,
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
    /// The services refused staged session `session` (staged for `uid`) as INSTALL_FAILED_VERSION_DOWNGRADE (`version` below the installed `installed`): its staging dir goes and the installer
    /// parses `package`'s installed `/data/app/<package>` in place (as an unchanged session, `--installed`) under the same session, so the session's `Commit` registers the installed package as it
    /// is (PMS refuses that install only; the installed package stays and runs). atld says so in one line naming both versionCodes. Answered [`Response::Staged`]; a failed run answers its stderr.
    Reinstate {
        session: u32,
        package: String,
        uid: u32,
        version: i64,
        installed: i64,
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
    /// Show `package`'s notification `key` (the StatusBarNotification key) on the desktop, replacing the key's earlier one (notifications plan N2: the sandboxed services have no session bus,
    /// atld posts on the app's behalf). Answered [`Response::Notified`].
    Notify {
        package: String,
        key: String,
        content: NotificationContent,
    },
    /// Remove notification `key` from the desktop. Answered [`Response::Withdrawn`].
    Withdraw {
        key: String,
    },
    /// The death of app process `pid` left no activity in any Task, and the process had a Task (a launched app; system packages' processes have none). `finished`: it held no activity
    /// that was not finishing (its Task was removed or it finished its last activity); false: it died with a live one (a crash, or its own exit). The host decides whether the run ends
    /// (atld `--exit-with-last-app`). Answered [`Response::Noted`].
    LastAppEnded {
        pid: i32,
        finished: bool,
    },
    /// The host's default network now, read outside the sandbox (GIO's verdict needs the host's main context). Answered [`Response::Network`]. The services ask once at start, after their watch starts, and again on each watch burst.
    Network,
}

impl Request {
    /// installd's file work (staging, the installer run, renames, data dirs): atld answers these on its installer lane, one at a time in arrival order ([`serve_lanes`]). AOSP commits install sessions on one installer thread (PackageInstallerService), so one lane is the same bound.
    pub fn is_installer_op(&self) -> bool {
        matches!(
            self,
            Request::Install { .. }
                | Request::Reinstate { .. }
                | Request::Scan { .. }
                | Request::Commit { .. }
                | Request::ScanCommit { .. }
        )
    }
}

/// A notification's desktop form, mapped from Android's by the services (notifications plan N4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationContent {
    pub summary: String,
    pub body: String,
    pub urgency: NotificationUrgency,
    /// stays until withdrawn (ongoing / no-clear)
    pub resident: bool,
    /// no sound for this post
    pub silent: bool,
}

/// The urgencies Android's importances map to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationUrgency {
    Low,
    Normal,
}

/// What became of a [`Request::Notify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyOutcome {
    /// a new notification is on the desktop
    Shown,
    /// the key's earlier one was replaced
    Updated,
    /// no notification service: nothing was shown
    NoService,
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
    /// The installer refused the package: its parse verdict (installer_end.rs). A scan skips it, as PMS skips a package that fails to parse; any other failed end is `Error`.
    Refused {
        message: String,
    },
    /// The package is in place.
    Committed,
    /// The notification's fate.
    Notified {
        outcome: NotifyOutcome,
    },
    /// The notification is gone; `shown`: one of it was on the desktop.
    Withdrawn {
        shown: bool,
    },
    /// A report the host took (no answer of its own).
    Noted,
    /// The host's default network as `DefaultNetwork::to_arg` writes it (one JSON object); None = no default route.
    Network {
        network: Option<String>,
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

/// The client's waiters: each unanswered call's reply slot, and why the connection ended once it has.
#[derive(Default)]
struct Waiting {
    calls: HashMap<u64, mpsc::Sender<io::Result<Response>>>,
    closed: Option<String>,
}

/// Locks the waiter table; a poisoned lock still holds valid state (a panic in another caller).
fn lock(m: &Mutex<Waiting>) -> MutexGuard<'_, Waiting> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn ended(why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("control connection ended: {why}"),
    )
}

/// The client end, shared by threads. A call takes an id and waits for the reply with that id; a reader thread routes replies by id, so a slow call never holds up another, and replies may overtake each other.
/// When the connection ends, or a reply matches no call, the connection is shut down and every waiter and every later call fails with `UnexpectedEof`: nothing waits on a dead connection.
pub struct Client {
    fd: Arc<OwnedFd>,
    waiting: Arc<Mutex<Waiting>>,
    next_id: AtomicU64,
}

impl Client {
    /// Takes the connection and starts its reader thread.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let fd = Arc::new(fd);
        let waiting = Arc::new(Mutex::new(Waiting::default()));
        let (reader_fd, reader_waiting) = (Arc::clone(&fd), Arc::clone(&waiting));
        thread::Builder::new()
            .name("control-replies".into())
            .spawn(move || route_replies(&reader_fd, &reader_waiting))?;
        Ok(Client {
            fd,
            waiting,
            next_id: AtomicU64::new(1),
        })
    }

    /// Sends `request` and waits for its reply. Fails when the connection has ended or the request could not be sent.
    pub fn call(&self, request: &Request) -> io::Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (slot, answer) = mpsc::channel();
        {
            let mut w = lock(&self.waiting);
            if let Some(why) = &w.closed {
                return Err(ended(why));
            }
            w.calls.insert(id, slot);
        }
        // SEQPACKET sends are whole messages: concurrent callers need no lock around the write
        if let Err(e) = write(&*self.fd, &Envelope { id, body: request }) {
            lock(&self.waiting).calls.remove(&id);
            return Err(e);
        }
        // the reader answers every waiter before it ends, so a closed slot means the reader is gone
        answer
            .recv()
            .unwrap_or_else(|_| Err(ended("reader stopped")))
    }
}

impl Drop for Client {
    // the reader's read ends and the server sees the end: both threads go
    fn drop(&mut self) {
        let _ = shutdown(&*self.fd, Shutdown::Both);
    }
}

/// Routes each reply to the call with its id until the connection ends; then every waiter fails.
fn route_replies(fd: &OwnedFd, waiting: &Mutex<Waiting>) {
    let why = loop {
        match read::<Envelope<Response>>(fd) {
            Ok(Some(Envelope { id, body })) => {
                let slot = lock(waiting).calls.remove(&id);
                match slot {
                    // a caller that went away takes its reply with it
                    Some(slot) => {
                        let _ = slot.send(Ok(body));
                    }
                    None => break format!("reply {id} matches no call"),
                }
            }
            Ok(None) => break "end of connection".to_owned(),
            Err(e) => break e.to_string(),
        }
    };
    let _ = shutdown(fd, Shutdown::Both);
    let mut w = lock(waiting);
    for slot in w.calls.drain().map(|(_, slot)| slot) {
        let _ = slot.send(Err(ended(&why)));
    }
    w.closed = Some(why);
}

/// The server's side: answers each request with `handle`, in arrival order, until the client ends the connection.
pub fn serve(fd: impl AsFd, mut handle: impl FnMut(Request) -> Response) -> io::Result<()> {
    while let Some(Envelope { id, body }) = read::<Envelope<Request>>(fd.as_fd())? {
        write(
            fd.as_fd(),
            &Envelope {
                id,
                body: &handle(body),
            },
        )?;
    }
    Ok(())
}

/// Answers the connection until the client ends it. An installer op ([`Request::is_installer_op`]) goes to `installer`, which runs on one lane thread: one request at a time, in arrival order.
/// Every other request goes to `inline`, on this thread, in arrival order. Each answer carries its request's id, so answers may overtake one another. Returns once the lane has answered what it holds.
/// A panic on either thread shuts the connection down: the client's waiters fail at once instead of waiting for answers that will not come.
pub fn serve_lanes(
    fd: OwnedFd,
    mut inline: impl FnMut(Request) -> Response,
    mut installer: impl FnMut(Request) -> Response + Send + 'static,
) -> io::Result<()> {
    let fd = Arc::new(fd);
    let _end = EndOnPanic(Arc::clone(&fd));
    let (lane_tx, lane_rx) = mpsc::channel::<(u64, Request)>();
    let lane = {
        let fd = Arc::clone(&fd);
        thread::Builder::new()
            .name("control-installer".into())
            .spawn(move || {
                let _end = EndOnPanic(Arc::clone(&fd));
                for (id, request) in lane_rx {
                    // a client that went away takes its waiter with it
                    let _ = write(
                        &*fd,
                        &Envelope {
                            id,
                            body: &installer(request),
                        },
                    );
                }
            })?
    };
    let result = loop {
        let (id, request) = match read::<Envelope<Request>>(&*fd) {
            Ok(Some(Envelope { id, body })) => (id, body),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        if request.is_installer_op() {
            if lane_tx.send((id, request)).is_err() {
                break Err(io::Error::other("installer lane stopped"));
            }
        } else if let Err(e) = write(
            &*fd,
            &Envelope {
                id,
                body: &inline(request),
            },
        ) {
            break Err(e);
        }
    };
    drop(lane_tx);
    let _ = lane.join();
    result
}

/// Shuts the connection down when its thread panics: the client sees the end and fails its waiters.
struct EndOnPanic(Arc<OwnedFd>);

impl Drop for EndOnPanic {
    fn drop(&mut self) {
        if thread::panicking() {
            let _ = shutdown(&*self.0, Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    use std::sync::mpsc::channel;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);

    fn pair() -> (OwnedFd, OwnedFd) {
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

    /// A call on its own thread: a call that never returns fails the test instead of hanging it.
    fn call_within(client: &Arc<Client>, request: Request) -> io::Result<Response> {
        let (done, answer) = channel();
        let client = Arc::clone(client);
        let _ = thread::spawn(move || done.send(client.call(&request)).ok());
        answer.recv_timeout(WAIT).expect("the call returned")
    }

    /// A request and its reply cross the socket intact, in order; the server returns when the client closes its end.
    #[test]
    fn request_reply_roundtrip() {
        let (client, server) = pair();
        let thread = thread::spawn(move || {
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
        let client = Client::new(client).expect("client");
        assert_eq!(
            client.call(&Request::Start(start())).expect("start"),
            Response::Started { pid: 4242 }
        );
        assert_eq!(
            client.call(&Request::Kill { pid: 4242 }).expect("kill"),
            Response::Killed
        );
        assert_eq!(
            client.call(&Request::Kill { pid: 7 }).expect("kill"),
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

    /// The id sits beside the body's fields: one object per message, request and reply alike.
    #[test]
    fn envelope_wire_form() {
        let json = serde_json::to_string(&Envelope {
            id: 5,
            body: &Request::Kill { pid: 5 },
        })
        .expect("encode");
        assert_eq!(json, r#"{"id":5,"op":"kill","pid":5}"#);
        let back: Envelope<Response> =
            serde_json::from_str(r#"{"id":9,"status":"started","pid":9}"#).expect("decode");
        assert_eq!(
            back,
            Envelope {
                id: 9,
                body: Response::Started { pid: 9 }
            }
        );
    }

    /// A server that is gone fails a call: an error, not a hang.
    #[test]
    fn server_gone() {
        let (client, server) = pair();
        drop(server);
        let client = Arc::new(Client::new(client).expect("client"));
        assert!(call_within(&client, Request::Kill { pid: 1 }).is_err());
    }

    /// Two calls in flight; the server answers the second first. Each caller gets the reply with its own id: a reply matched by arrival order would hand the first caller the other's.
    #[test]
    fn replies_are_matched_by_id() {
        let (client, server) = pair();
        let (first_seen, first_read) = channel();
        let server = thread::spawn(move || {
            let first = read::<Envelope<Request>>(&server)
                .expect("read")
                .expect("first");
            first_seen.send(()).expect("signal");
            let second = read::<Envelope<Request>>(&server)
                .expect("read")
                .expect("second");
            for call in [second, first] {
                let pid = match call.body {
                    Request::Kill { pid } => pid,
                    other => panic!("unexpected {other:?}"),
                };
                let reply = Response::Error {
                    message: format!("pid {pid}"),
                };
                write(
                    &server,
                    &Envelope {
                        id: call.id,
                        body: &reply,
                    },
                )
                .expect("reply");
            }
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let (done, answers) = channel();
        for pid in [1, 2] {
            let (client, done) = (Arc::clone(&client), done.clone());
            let _ =
                thread::spawn(move || done.send((pid, client.call(&Request::Kill { pid }))).ok());
            if pid == 1 {
                first_read
                    .recv_timeout(WAIT)
                    .expect("the first call reached the server");
            }
        }
        let mut answers: Vec<_> = (0..2)
            .map(|_| answers.recv_timeout(WAIT).expect("a call returned"))
            .collect();
        answers.sort_by_key(|(pid, _)| *pid);
        for (pid, answer) in answers {
            assert_eq!(
                answer.expect("call"),
                Response::Error {
                    message: format!("pid {pid}")
                }
            );
        }
        drop(client);
        server.join().expect("server");
    }

    /// The server ends the connection with a call in flight: that call fails, and so does one made after. Neither waits for an answer that will not come.
    #[test]
    fn a_connection_end_fails_every_waiter() {
        let (client, server) = pair();
        let server = thread::spawn(move || {
            read::<Envelope<Request>>(&server)
                .expect("read")
                .expect("the call");
            drop(server);
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let in_flight = call_within(&client, Request::Kill { pid: 1 });
        assert!(in_flight.is_err(), "{in_flight:?}");
        server.join().expect("server");
        let later = call_within(&client, Request::Kill { pid: 2 });
        assert!(later.is_err(), "{later:?}");
    }

    /// An installer that panics ends the connection: its call and the calls after it fail, and the server returns.
    #[test]
    fn installer_panic_ends_connection() {
        let (client, server) = pair();
        let server = thread::spawn(move || {
            serve_lanes(server, |_| Response::Noted, |_| panic!("installer bug"))
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let install = call_within(
            &client,
            Request::Install {
                session: 1,
                uid: 10_001,
            },
        );
        assert!(install.is_err(), "{install:?}");
        let later = call_within(&client, Request::Network);
        assert!(later.is_err(), "{later:?}");
        server
            .join()
            .expect("server thread")
            .expect("serve_lanes returns once the connection ends");
    }

    /// Installer requests run on one lane thread, in arrival order; the other requests run on the serving thread.
    #[test]
    fn installer_requests_share_one_lane_in_order() {
        let (client, server) = pair();
        let (seen, record) = channel::<(&'static str, thread::ThreadId, Request)>();
        let lane_seen = seen.clone();
        let server = thread::spawn(move || {
            serve_lanes(
                server,
                move |r| {
                    seen.send(("serving", thread::current().id(), r))
                        .expect("record");
                    Response::Noted
                },
                move |r| {
                    lane_seen
                        .send(("lane", thread::current().id(), r))
                        .expect("record");
                    Response::Noted
                },
            )
            .expect("serve_lanes");
        });
        let client = Client::new(client).expect("client");
        let install = |session| Request::Install {
            session,
            uid: 10_001,
        };
        assert_eq!(client.call(&install(1)).expect("install"), Response::Noted);
        assert_eq!(
            client.call(&Request::Network).expect("network"),
            Response::Noted
        );
        assert_eq!(client.call(&install(2)).expect("install"), Response::Noted);
        drop(client);
        server.join().expect("server");
        let all: Vec<_> = record.try_iter().collect();
        let lane: Vec<_> = all.iter().filter(|s| s.0 == "lane").collect();
        let serving: Vec<_> = all.iter().filter(|s| s.0 == "serving").collect();
        assert_eq!(
            lane.iter().map(|s| s.2.clone()).collect::<Vec<_>>(),
            vec![install(1), install(2)]
        );
        assert_eq!(
            serving.iter().map(|s| s.2.clone()).collect::<Vec<_>>(),
            vec![Request::Network]
        );
        assert!(
            lane.windows(2).all(|w| w[0].1 == w[1].1),
            "one lane thread: {lane:?}"
        );
        assert_ne!(
            lane[0].1, serving[0].1,
            "the lane is not the serving thread"
        );
    }
}
