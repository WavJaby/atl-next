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

/// An `Error` reply's frame around its text: the envelope at its widest id, and the message key.
const ERROR_FRAME: usize = r#"{"id":18446744073709551615,"status":"error","message":""}"#.len();
/// The longest JSON escape of one text byte: a control character is `\u00XX`.
const ESCAPE_MAX: usize = r#"\u0000"#.len();
/// The most text an `Error` reply carries ([`keep_tail`]): at the longest escape per byte it still fits a packet.
pub const MAX_ERROR_TEXT: usize = (MAX_MESSAGE - ERROR_FRAME) / ESCAPE_MAX;

/// `text` if it fits `room` bytes, else its end that does, behind a marker naming the bytes cut. The end is kept: an installer's stderr ends with its failure.
pub fn keep_tail(text: &str, room: usize) -> String {
    if text.len() <= room {
        return text.to_owned();
    }
    // the widest marker this text can need (a cut of all of it) comes off the room first
    let reserve = format!("[{} bytes cut] ", text.len()).len();
    let mut start = text.len() - room.saturating_sub(reserve);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[{start} bytes cut] {}", &text[start..])
}

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
    /// installd's file work (staging, the installer run, renames, data dirs): atld answers these on its installer lane, one at a time in arrival order ([`serve_lanes`]).
    /// One lane is the bound: AOSP commits install sessions on one installer thread (PackageInstallerService.mInstallThread); boot scans are not measured for width (P7 decides).
    /// Exhaustive on purpose: a new request chooses its lane here.
    pub fn is_installer_op(&self) -> bool {
        match self {
            Request::Install { .. }
            | Request::Reinstate { .. }
            | Request::Scan { .. }
            | Request::Commit { .. }
            | Request::ScanCommit { .. } => true,
            Request::Start(_)
            | Request::Kill { .. }
            | Request::ProgramPid { .. }
            | Request::Notify { .. }
            | Request::Withdraw { .. }
            | Request::LastAppEnded { .. }
            | Request::Network => false,
        }
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
    send_packet(fd, &encode(m)?)
}

/// One packet, whole or not at all (SEQPACKET).
fn send_packet(fd: impl AsFd, bytes: &[u8]) -> io::Result<()> {
    let sent = send(fd, bytes, SendFlags::empty())?;
    if sent != bytes.len() {
        return Err(io::Error::other("short write on a SEQPACKET socket"));
    }
    Ok(())
}

/// Sends the reply to call `id`. A reply the packet cannot carry becomes an `Error` that says so: its caller fails at once instead of waiting. A failed send is the connection's end.
fn reply(fd: impl AsFd, id: u64, body: &Response) -> io::Result<()> {
    let bytes = serde_json::to_vec(&Envelope { id, body }).map_err(io::Error::other)?;
    if bytes.len() <= MAX_MESSAGE {
        return send_packet(fd, &bytes);
    }
    let note = Response::Error {
        message: format!(
            "reply of {} bytes over the {MAX_MESSAGE}-byte control limit: not sent",
            bytes.len()
        ),
    };
    write(fd, &Envelope { id, body: &note })
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
/// When the connection ends, or a reply matches no call, the connection is shut down and every waiter fails with the reason. A later call fails at once: `UnexpectedEof`, or the write's error (BrokenPipe) when it raced the shutdown. Nothing waits on a dead connection.
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
        // the reader answers every waiter before it ends ([`ReaderEnd`]): no slot is dropped unanswered
        answer.recv().expect("the reader answers every waiter")
    }
}

impl Drop for Client {
    // the reader's read ends and the server sees the end: both threads go
    fn drop(&mut self) {
        let _ = shutdown(&*self.fd, Shutdown::Both);
    }
}

/// The reader's exit: shuts the connection and fails every waiter with the reason. A drop guard, so a panic in the reader fails them too.
struct ReaderEnd<'a> {
    fd: &'a OwnedFd,
    waiting: &'a Mutex<Waiting>,
    why: String,
}

impl Drop for ReaderEnd<'_> {
    fn drop(&mut self) {
        let _ = shutdown(self.fd, Shutdown::Both);
        let mut w = lock(self.waiting);
        for slot in w.calls.drain().map(|(_, slot)| slot) {
            let _ = slot.send(Err(ended(&self.why)));
        }
        w.closed = Some(std::mem::take(&mut self.why));
    }
}

/// Routes each reply to the call with its id until the connection ends; then every waiter fails ([`ReaderEnd`]).
fn route_replies(fd: &OwnedFd, waiting: &Mutex<Waiting>) {
    let mut end = ReaderEnd {
        fd,
        waiting,
        why: "the reader panicked".to_owned(),
    };
    end.why = loop {
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
}

/// Answers the connection until the client ends it. An installer op ([`Request::is_installer_op`]) goes to `installer`, one request at a time on one lane thread, in arrival order.
/// Every other request goes to `inline`, on this thread, in arrival order. Each answer carries its request's id, so answers may overtake one another.
/// The connection ends with this function, on any exit (an error or a panic on either thread included), before the lane is joined: every waiter fails at once, not after the installer's current item.
/// The lane stops at its first failed reply, so the queue is dropped once the connection has ended: nobody reads those replies. Returns the first error: the lane's (a panic or a failed send), else the serving side's.
/// The lane's queue is unbounded: each queued request is one a services thread is blocked on ([`Client::call`] waits for its reply), so the queue holds at most the services' concurrent callers.
pub fn serve_lanes(
    fd: OwnedFd,
    mut inline: impl FnMut(Request) -> Response,
    mut installer: impl FnMut(Request) -> Response + Send,
) -> io::Result<()> {
    let fd = Arc::new(fd);
    let (lane_tx, lane_rx) = mpsc::channel::<(u64, Request)>();
    // scoped: the lane borrows what the installer closure borrows, and the same closure may serve both lanes
    thread::scope(|s| {
        let lane = {
            let fd = Arc::clone(&fd);
            thread::Builder::new()
                .name("control-installer".into())
                .spawn_scoped(s, move || -> io::Result<()> {
                    let _end = EndConnection(Arc::clone(&fd));
                    for (id, request) in lane_rx {
                        reply(&*fd, id, &installer(request))?;
                    }
                    Ok(())
                })?
        };
        let result = {
            let _end = EndConnection(Arc::clone(&fd));
            loop {
                let (id, request) = match read::<Envelope<Request>>(&*fd) {
                    Ok(Some(Envelope { id, body })) => (id, body),
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e),
                };
                if request.is_installer_op() {
                    if lane_tx.send((id, request)).is_err() {
                        break Err(io::Error::other("installer lane stopped"));
                    }
                } else if let Err(e) = reply(&*fd, id, &inline(request)) {
                    break Err(e);
                }
            }
        };
        drop(lane_tx);
        let lane = lane
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("installer lane panicked")));
        lane.and(result)
    })
}

/// Shuts the connection down when dropped: the client sees the end and fails its waiters.
struct EndConnection(Arc<OwnedFd>);

impl Drop for EndConnection {
    fn drop(&mut self) {
        let _ = shutdown(&*self.0, Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
    use std::sync::mpsc::channel;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);
    /// How long a Start or a lane item holds for a request that could overtake it: long enough for a request routed to another thread to run first, so the order tests can see it.
    const OVERTAKE: Duration = Duration::from_millis(200);
    /// Ids start at 1 ([`Client::new`]): 0 is never a call's id.
    const NEVER_ISSUED: u64 = 0;

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
        let seen = Arc::new(Mutex::new(Vec::new()));
        let inline_seen = Arc::clone(&seen);
        let thread = thread::spawn(move || {
            serve_lanes(
                server,
                move |r| {
                    inline_seen.lock().expect("seen").push(r.clone());
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
                },
                |_| Response::Noted,
            )
            .expect("serve_lanes");
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
        thread.join().expect("server thread");
        assert_eq!(
            *seen.lock().expect("seen"),
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

    /// A call on its own thread, its answer read later through the returned receiver.
    fn call_async(client: &Arc<Client>, request: Request) -> mpsc::Receiver<io::Result<Response>> {
        let (done, answer) = channel();
        let client = Arc::clone(client);
        let _ = thread::spawn(move || done.send(client.call(&request)).ok());
        answer
    }

    /// The server ends the connection with two calls in flight: both fail, and so does one made after. None waits for an answer that will not come.
    #[test]
    fn a_connection_end_fails_every_waiter() {
        let (client, server) = pair();
        let server = thread::spawn(move || {
            read::<Envelope<Request>>(&server)
                .expect("read")
                .expect("the first call");
            read::<Envelope<Request>>(&server)
                .expect("read")
                .expect("the second call");
            drop(server);
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let first = call_async(&client, Request::Kill { pid: 1 });
        let second = call_async(&client, Request::Kill { pid: 2 });
        server.join().expect("server");
        for answer in [first, second] {
            let answer = answer.recv_timeout(WAIT).expect("the call returned");
            assert!(answer.is_err(), "{answer:?}");
        }
        let later = call_within(&client, Request::Kill { pid: 3 });
        assert!(later.is_err(), "{later:?}");
    }

    /// A reply that matches no call ends the connection: the call waiting fails, and so does a later one. The server keeps its end open, so the client must end the connection.
    #[test]
    fn an_unknown_reply_id_ends_the_connection() {
        let (client, server) = pair();
        let (done, held) = channel::<()>();
        let server = thread::spawn(move || {
            read::<Envelope<Request>>(&server)
                .expect("read")
                .expect("the call");
            write(
                &server,
                &Envelope {
                    id: NEVER_ISSUED,
                    body: &Response::Noted,
                },
            )
            .expect("reply");
            held.recv().ok();
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let answer = call_within(&client, Request::Kill { pid: 1 });
        assert!(answer.is_err(), "{answer:?}");
        let later = call_within(&client, Request::Kill { pid: 2 });
        assert!(later.is_err(), "{later:?}");
        done.send(()).expect("done");
        server.join().expect("server");
    }

    /// A second reply for a call already answered matches no call: the connection ends. The first call keeps its answer; a later call fails.
    #[test]
    fn a_duplicate_reply_id_ends_the_connection() {
        let (client, server) = pair();
        let (done, held) = channel::<()>();
        let server = thread::spawn(move || {
            let call = read::<Envelope<Request>>(&server)
                .expect("read")
                .expect("the call");
            let reply = Envelope {
                id: call.id,
                body: &Response::Noted,
            };
            write(&server, &reply).expect("reply");
            write(&server, &reply).expect("the same reply again");
            held.recv().ok();
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let first = call_within(&client, Request::Kill { pid: 1 });
        assert_eq!(first.expect("the first call"), Response::Noted);
        let later = call_within(&client, Request::Kill { pid: 2 });
        assert!(later.is_err(), "{later:?}");
        done.send(()).expect("done");
        server.join().expect("server");
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
        let ended = server.join().expect("server thread");
        let err = ended.expect_err("the lane's panic is the server's error, not a clean end");
        assert!(err.to_string().contains("panicked"), "{err}");
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
                    // the first install holds the lane, so the requests behind it are queued when it returns
                    let first = matches!(r, Request::Install { session: 1, .. });
                    lane_seen
                        .send(("lane", thread::current().id(), r))
                        .expect("record");
                    if first {
                        thread::sleep(OVERTAKE);
                    }
                    Response::Noted
                },
            )
            .expect("serve_lanes");
        });
        let install = |session| Request::Install {
            session,
            uid: 10_001,
        };
        let commit = Request::Commit {
            session: 3,
            package: "com.example".into(),
            uid: 10_001,
            names: vec![],
        };
        // four requests in flight before any answer; the lane keeps the order of its three (a LIFO lane reverses them)
        let requests = [install(1), commit.clone(), install(2), Request::Network];
        for (id, request) in (1_u64..).zip(&requests) {
            write(&client, &Envelope { id, body: request }).expect("send");
        }
        for _ in &requests {
            let reply = read::<Envelope<Response>>(&client)
                .expect("read")
                .expect("a reply");
            assert_eq!(reply.body, Response::Noted);
        }
        drop(client);
        server.join().expect("server");
        let all: Vec<_> = record.try_iter().collect();
        let lane: Vec<_> = all.iter().filter(|s| s.0 == "lane").collect();
        let serving: Vec<_> = all.iter().filter(|s| s.0 == "serving").collect();
        assert_eq!(
            lane.iter().map(|s| s.2.clone()).collect::<Vec<_>>(),
            vec![install(1), commit, install(2)]
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

    /// Every installer op runs on the lane and none on the serving thread: each is sent alone and the thread that answers it is recorded.
    #[test]
    fn every_installer_op_runs_on_the_lane() {
        let (client, server) = pair();
        let ran = Arc::new(Mutex::new(Vec::new()));
        let (inline_ran, lane_ran) = (Arc::clone(&ran), Arc::clone(&ran));
        let server = thread::spawn(move || {
            serve_lanes(
                server,
                move |r| {
                    inline_ran.lock().expect("ran").push((r, "serving"));
                    Response::Noted
                },
                move |r| {
                    lane_ran.lock().expect("ran").push((r, "lane"));
                    Response::Noted
                },
            )
            .expect("serve_lanes");
        });
        let client = Client::new(client).expect("client");
        let calls = vec![
            Request::Install {
                session: 1,
                uid: 10_001,
            },
            Request::Reinstate {
                session: 1,
                package: "com.example".into(),
                uid: 10_001,
                version: 2,
                installed: 3,
            },
            Request::Scan {
                dir: "/product/app/Fixture".into(),
            },
            Request::Commit {
                session: 1,
                package: "com.example".into(),
                uid: 10_001,
                names: vec![],
            },
            Request::ScanCommit {
                dir: "/product/app/Fixture".into(),
                package: "com.example".into(),
                uid: 10_001,
            },
        ];
        for call in &calls {
            assert_eq!(client.call(call).expect("call"), Response::Noted);
        }
        drop(client);
        server.join().expect("server");
        let expected: Vec<_> = calls.into_iter().map(|c| (c, "lane")).collect();
        assert_eq!(*ran.lock().expect("ran"), expected);
    }

    /// Installer replies are answered with the error reply that fits when the reply itself cannot be sent (over MAX_MESSAGE): the caller returns, it does not wait.
    #[test]
    fn an_oversized_lane_reply_fails_its_caller() {
        let (client, server) = pair();
        let server = thread::spawn(move || {
            serve_lanes(
                server,
                |_| Response::Noted,
                |_| Response::Error {
                    message: "x".repeat(MAX_MESSAGE),
                },
            )
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let answer = call_within(
            &client,
            Request::Install {
                session: 1,
                uid: 10_001,
            },
        );
        assert!(matches!(answer, Ok(Response::Error { .. })), "{answer:?}");
        drop(client);
        server.join().expect("server thread").expect("serve_lanes");
    }

    /// The same for an inline reply: the caller gets an error reply that fits, and the connection goes on.
    #[test]
    fn an_oversized_inline_reply_fails_its_caller() {
        let (client, server) = pair();
        let server = thread::spawn(move || {
            serve_lanes(
                server,
                |_| Response::Error {
                    message: "x".repeat(MAX_MESSAGE),
                },
                |_| Response::Noted,
            )
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let answer = call_within(&client, Request::Network);
        assert!(matches!(answer, Ok(Response::Error { .. })), "{answer:?}");
        let later = call_within(&client, Request::Kill { pid: 1 });
        assert!(matches!(later, Ok(Response::Error { .. })), "{later:?}");
        drop(client);
        server.join().expect("server thread").expect("serve_lanes");
    }

    /// The serving side fails on a packet it cannot read while an installer runs: the call in flight and a later call fail at once, not when the installer returns.
    #[test]
    fn a_serving_read_failure_fails_the_waiters_while_the_lane_runs() {
        let (client, server) = pair();
        let raw = rustix::io::dup(&client).expect("dup");
        let (entered, running) = channel();
        let (release, held) = channel::<()>();
        let server = thread::spawn(move || {
            serve_lanes(
                server,
                |_| Response::Noted,
                move |_| {
                    entered.send(()).expect("entered");
                    held.recv().expect("released");
                    Response::Noted
                },
            )
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let in_flight = call_async(
            &client,
            Request::Install {
                session: 1,
                uid: 10_001,
            },
        );
        running.recv_timeout(WAIT).expect("the installer runs");
        send(&raw, b"not json", SendFlags::empty()).expect("send");
        let failed = in_flight
            .recv_timeout(WAIT)
            .expect("the in-flight call returned while the installer runs");
        assert!(failed.is_err(), "{failed:?}");
        let later = call_within(&client, Request::Network);
        assert!(later.is_err(), "{later:?}");
        release.send(()).expect("release");
        let ended = server.join().expect("server thread");
        assert!(ended.is_err(), "the read failure is the server's error: {ended:?}");
    }

    /// A panic on the serving thread ends the connection at once as well: the call in flight fails while the installer still runs.
    #[test]
    fn a_serving_panic_fails_the_waiters_while_the_lane_runs() {
        let (client, server) = pair();
        let (entered, running) = channel();
        let (release, held) = channel::<()>();
        let server = thread::spawn(move || {
            serve_lanes(
                server,
                |_| panic!("inline bug"),
                move |_| {
                    entered.send(()).expect("entered");
                    held.recv().expect("released");
                    Response::Noted
                },
            )
        });
        let client = Arc::new(Client::new(client).expect("client"));
        let in_flight = call_async(
            &client,
            Request::Install {
                session: 1,
                uid: 10_001,
            },
        );
        running.recv_timeout(WAIT).expect("the installer runs");
        let failed = call_within(&client, Request::Network);
        assert!(failed.is_err(), "{failed:?}");
        let in_flight = in_flight
            .recv_timeout(WAIT)
            .expect("the in-flight call returned while the installer runs");
        assert!(in_flight.is_err(), "{in_flight:?}");
        release.send(()).expect("release");
        assert!(server.join().is_err(), "the serving thread panicked");
    }

    /// Start, Kill and Network share the serving thread, so they are answered in arrival order while the lane holds an install: each is recorded where it is answered.
    /// A Kill routed to the lane waits behind the install (OVERTAKE) and is recorded after the Network that follows it; a Start routed there is recorded after the Kill.
    #[test]
    fn inline_requests_answer_in_order_while_the_lane_runs() {
        let (client, server) = pair();
        let inline_order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let lane_order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let (inline_seen, lane_seen) = (Arc::clone(&inline_order), Arc::clone(&lane_order));
        let server = thread::spawn(move || {
            serve_lanes(
                server,
                move |r| match r {
                    Request::Start(_) => {
                        inline_seen.lock().expect("order").push("start");
                        Response::Started { pid: 77 }
                    }
                    Request::Kill { .. } => {
                        inline_seen.lock().expect("order").push("kill");
                        Response::Killed
                    }
                    Request::Network => {
                        inline_seen.lock().expect("order").push("network");
                        Response::Network { network: None }
                    }
                    other => panic!("unexpected {other:?}"),
                },
                move |_| {
                    lane_seen.lock().expect("order").push("install");
                    thread::sleep(OVERTAKE);
                    Response::Noted
                },
            )
            .expect("serve_lanes");
        });
        let install = Request::Install {
            session: 1,
            uid: 10_001,
        };
        let requests = [
            install,
            Request::Start(start()),
            Request::Kill { pid: 77 },
            Request::Network,
        ];
        for (id, request) in (1_u64..).zip(&requests) {
            write(&client, &Envelope { id, body: request }).expect("send");
        }
        let mut replies: Vec<_> = requests
            .iter()
            .map(|_| {
                read::<Envelope<Response>>(&client)
                    .expect("read")
                    .expect("a reply")
            })
            .collect();
        replies.sort_by_key(|e| e.id);
        assert_eq!(
            replies.into_iter().map(|e| e.body).collect::<Vec<_>>(),
            vec![
                Response::Noted,
                Response::Started { pid: 77 },
                Response::Killed,
                Response::Network { network: None },
            ]
        );
        drop(client);
        server.join().expect("server");
        assert_eq!(
            *inline_order.lock().expect("order"),
            vec!["start", "kill", "network"]
        );
        assert_eq!(*lane_order.lock().expect("order"), vec!["install"]);
    }

    /// A reader that panics fails every waiter: the drop guard drains them with the reason, so none waits on a reader that is gone.
    #[test]
    fn a_reader_panic_fails_every_waiter() {
        let (fd, _peer) = pair();
        let waiting = Mutex::new(Waiting::default());
        let (slot, answer) = channel();
        lock(&waiting).calls.insert(1, slot);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _end = ReaderEnd {
                fd: &fd,
                waiting: &waiting,
                why: "reader bug".into(),
            };
            panic!("reader bug");
        }));
        let failed = answer.recv_timeout(WAIT).expect("the waiter is answered");
        assert!(failed.is_err(), "{failed:?}");
        assert!(lock(&waiting).closed.is_some());
    }

    /// Text within the bound is returned whole; a longer one is cut to its end, and the reply that carries the cut fits a packet even when every byte is escaped.
    #[test]
    fn an_error_text_over_the_bound_is_cut_and_fits() {
        assert_eq!(keep_tail("short", 10), "short");
        let text = "\u{1}".repeat(2 * MAX_ERROR_TEXT);
        let cut = keep_tail(&text, MAX_ERROR_TEXT);
        assert!(cut.contains(" bytes cut] "), "{:.60}", cut);
        assert!(cut.len() <= MAX_ERROR_TEXT, "{} bytes", cut.len());
        let packet = serde_json::to_vec(&Envelope {
            id: u64::MAX,
            body: &Response::Error { message: cut },
        })
        .expect("encode");
        assert!(packet.len() <= MAX_MESSAGE, "{} bytes", packet.len());
    }
}
