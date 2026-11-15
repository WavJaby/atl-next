// SPDX-License-Identifier: MIT
//! The web-process channel (owner D2, 2026-11-15): an app process asks the broker for exactly one thing, "start a WebKit web process for me", and gets a process weaker than itself.
//! WebKit (host WebKitGTK in the app process) sandboxes its web process through Flatpak's route when `/.flatpak-info` exists: it runs `flatpak-spawn --sandbox ... WebKitWebProcess`, found in PATH.
//! Our `flatpak-spawn` (crate atl-flatpak-spawn, bound at [`SHIM_AT`]) turns that line into one [`Request`] on this channel; the broker checks it ([`admit`]) and starts the web process through
//! atl-sandbox-join in [`crate::Profile::WebProcess`]: a child pid namespace of the app's (WebKit takes the pid from SCM credentials and asserts it non-zero), built from the app's own mount view.
//!
//! Channel: one SEQPACKET socketpair per app sandbox, the app's end at fd [`FD_ENV`]; identity is the socket itself, so a request names no app. One JSON request per packet with [`FDS`] fds
//! (SCM_RIGHTS): WebKit's socket for the child, a reply socket (one per request: concurrent shims share the channel), stdout, stderr. The reply ([`Reply`]) goes to the reply socket.
//! Not here, on purpose: any other program, an unsandboxed start, a kill (the web process ends with its IPC socket, or with the app's pid namespace), anything outside the app's writable dirs.

use std::ffi::OsString;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recv, recvmsg, send, sendmsg,
};
use serde::{Deserialize, Serialize};

use crate::Mount;

/// The variable naming the app's end of the channel (an inherited fd).
pub const FD_ENV: &str = "ATL_WEBSPAWN_FD";
/// Where every app sandbox shows the shim: a directory of its own on the app's PATH ([`APP_PATH`]), since WebKit runs `flatpak-spawn` by name.
pub const SHIM_DIR: &str = "/.atl-bin";
pub const SHIM_AT: &str = "/.atl-bin/flatpak-spawn";
/// The app's PATH: GLib's search path when PATH is unset (`/bin:/usr/bin:.`, gspawn.c) without the cwd, then [`SHIM_DIR`]; every name found before keeps resolving as before.
pub const APP_PATH: &str = "/bin:/usr/bin:/.atl-bin";
/// The file whose existence switches WebKit to the Flatpak route (wtf/glib Sandbox.cpp `isInsideFlatpak`); empty: nothing reads its content on this route.
pub const FLATPAK_INFO: &str = "/.flatpak-info";
/// Fds per request, in order: WebKit's socket for the child, the reply socket, stdout, stderr.
pub const FDS: usize = 4;
/// The largest request accepted: WebKit forwards its whole environment (`--env=` per variable, FlatpakLauncher.cpp).
pub const MAX_MESSAGE: usize = 256 * 1024;
/// The programs a request may name: (directory name, file name) of WebKit's web process; a path qualifies only under `/usr` (host-installed, read-only in every sandbox).
/// The network process is absent: WebKit spawns it directly, with the app's rights (ProcessLauncherGLib: only `--sandbox` processes go through flatpak-spawn).
pub const PROGRAMS: &[(&str, &str)] = &[("webkitgtk-6.0", "WebKitWebProcess")];
/// `--sandbox-flag` values WebKit passes (FlatpakLauncher.cpp); `share-gpu` gives the app's DRM nodes, the rest give nothing (no session bus, no display or sound socket is shown).
pub const SANDBOX_FLAGS: &[&str] = &[
    "share-gpu",
    "share-display",
    "share-sound",
    "allow-a11y",
    "allow-dbus",
];
/// atl_diag interface of the refusal counters (`refused WebProcessSpawn.<what>`).
pub const INTERFACE: &str = "WebProcessSpawn";

/// A flatpak-spawn line, parsed by the shim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub program: String,
    pub args: Vec<String>,
    /// `--forward-fd=N`: the fd number WebKit gave the child's socket (the web process's argv names it).
    pub forward_fd: i32,
    /// `--env=K=V`, in order.
    pub env: Vec<(String, String)>,
    /// `--directory=`; absent = `/`.
    pub directory: String,
    /// `--sandbox`.
    pub sandbox: bool,
    /// Not `--no-network`. The web process gets no network either way; recorded so a request for one is refused, not silently narrowed.
    pub network: bool,
    /// `--sandbox-flag=` values.
    pub flags: Vec<String>,
    /// `--sandbox-expose-path[-ro][-try]=`.
    pub expose: Vec<Expose>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expose {
    pub path: String,
    pub writable: bool,
    /// Not a `-try` form: a missing path refuses the request.
    pub must_exist: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    /// Started; `pid` in the app's pid namespace (the sandbox's first process).
    Started { pid: i32 },
    /// The request asks for something the channel does not give; counted under `what`.
    Refused { what: String, why: String },
    /// Admitted, but the start failed.
    Failed { message: String },
}

/// What a request may reach: the calling app's sandbox.
#[derive(Debug, Clone)]
pub struct AppView {
    pub uid: u32,
    /// The app's device nodes (its DRM nodes), shown to the web process under `share-gpu`.
    pub devices: Vec<PathBuf>,
    /// The app's writable mounts: the only dirs an expose path may lie in.
    pub writable: Vec<Mount>,
}

/// An admitted request: what the broker starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub program: PathBuf,
    /// WebKit's process identifier (argv[1]); argv[2], the socket's fd, is the broker's to fill.
    pub identifier: String,
    pub env: Vec<(OsString, OsString)>,
    /// Expose paths, each at its own path in the app's view (`host` = `at`: the web sandbox is built inside the app's mount namespace).
    pub expose: Vec<Mount>,
    pub gpu: bool,
}

/// Why a request is refused: `what` keys the counter (a fixed word), `why` names the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub what: &'static str,
    pub why: String,
}

fn refuse(what: &'static str, why: impl Into<String>) -> Refusal {
    Refusal {
        what,
        why: why.into(),
    }
}

/// `p` is absolute and plain: only the root and normal components (no `.`, `..`, empty or relative parts), so its text is its place.
fn plain(p: &Path) -> bool {
    p.is_absolute()
        && p.components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// Whether `p` names a [`PROGRAMS`] row under `/usr` (by text; [`admit`] also requires it be the canonical path of a regular file).
pub fn program_allowed(p: &Path) -> bool {
    fn name(q: Option<&Path>) -> Option<&str> {
        q.and_then(Path::file_name).and_then(|n| n.to_str())
    }
    plain(p)
        && p.starts_with("/usr")
        && PROGRAMS
            .iter()
            .any(|(dir, file)| name(Some(p)) == Some(*file) && name(p.parent()) == Some(*dir))
}

/// Variables the web process does not get: the loader's (the paths they name are not in its view) and the broker's own fds.
fn dropped(name: &str) -> bool {
    name.starts_with("LD_") || name == FD_ENV || name == crate::CONTROL_ENV
}

/// Checks `req` against the app's view. `canonical`: a path's canonical form on the host (None = absent); `exists`: whether a host path exists. Refusals are not counted here ([`admit`] counts).
pub fn check(
    req: &Request,
    app: &AppView,
    canonical: impl Fn(&Path) -> Option<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Result<Plan, Refusal> {
    if !req.sandbox {
        return Err(refuse("unsandboxed", "no --sandbox"));
    }
    if req.network {
        return Err(refuse("network", "no --no-network"));
    }
    if req.directory != "/" {
        return Err(refuse("directory", req.directory.clone()));
    }
    let program = PathBuf::from(&req.program);
    if !program_allowed(&program) || canonical(&program).as_deref() != Some(program.as_path()) {
        return Err(refuse("program", req.program.clone()));
    }
    // WebKit's argv: <identifier> <socket fd> (ProcessLauncherGLib); nothing else
    let identifier = match &req.args[..] {
        [id, fd] if id.parse::<u64>().is_ok() && *fd == req.forward_fd.to_string() => id.clone(),
        _ => return Err(refuse("args", format!("{:?}", req.args))),
    };
    if let Some(f) = req
        .flags
        .iter()
        .find(|f| !SANDBOX_FLAGS.contains(&f.as_str()))
    {
        return Err(refuse("flag", f.clone()));
    }
    let mut expose = Vec::new();
    for e in &req.expose {
        let at = PathBuf::from(&e.path);
        let host = plain(&at)
            .then(|| {
                app.writable
                    .iter()
                    .find_map(|m| Some(m.host.join(at.strip_prefix(&m.at).ok()?)))
            })
            .flatten();
        let Some(host) = host else {
            return Err(refuse("expose", e.path.clone()));
        };
        if !exists(&host) {
            if e.must_exist {
                return Err(refuse("expose-missing", e.path.clone()));
            }
            continue;
        }
        expose.push(Mount {
            host: at.clone(),
            at,
            writable: e.writable,
        });
    }
    Ok(Plan {
        program,
        identifier,
        env: req
            .env
            .iter()
            .filter(|(k, _)| !dropped(k))
            .map(|(k, v)| (k.into(), v.into()))
            .collect(),
        expose,
        gpu: req.flags.iter().any(|f| f == "share-gpu"),
    })
}

/// [`check`] on the host's filesystem, a refusal counted (`refused WebProcessSpawn.<what>`) and answered.
pub fn admit(req: &Request, app: &AppView) -> Result<Plan, Reply> {
    check(
        req,
        app,
        |p| std::fs::canonicalize(p).ok().filter(|c| c.is_file()),
        |p| p.exists(),
    )
    .map_err(|r| {
        atl_diag::refused(INTERFACE, r.what);
        Reply::Refused {
            what: r.what.to_owned(),
            why: r.why,
        }
    })
}

/// The shim's side: sends `req` with `fds` ([`FDS`], in their order) on `channel`.
pub fn send_request(
    channel: impl AsFd,
    req: &Request,
    fds: [BorrowedFd<'_>; FDS],
) -> io::Result<()> {
    let bytes = serde_json::to_vec(req).map_err(io::Error::other)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::other(format!(
            "request of {} bytes (limit {MAX_MESSAGE})",
            bytes.len()
        )));
    }
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    control.push(SendAncillaryMessage::ScmRights(&fds));
    let sent = sendmsg(
        channel,
        &[IoSlice::new(&bytes)],
        &mut control,
        SendFlags::empty(),
    )?;
    if sent != bytes.len() {
        return Err(io::Error::other("short write on a SEQPACKET socket"));
    }
    Ok(())
}

/// The shim's side: the broker's answer on the reply socket.
pub fn read_reply(reply: impl AsFd) -> io::Result<Reply> {
    let mut buf = vec![0u8; 64 * 1024];
    let n = recv(reply, &mut buf, RecvFlags::empty())?.0;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    serde_json::from_slice(&buf[..n.min(buf.len())]).map_err(io::Error::other)
}

fn write_reply(reply: impl AsFd, r: &Reply) -> io::Result<()> {
    let bytes = serde_json::to_vec(r).map_err(io::Error::other)?;
    send(reply, &bytes, SendFlags::empty())?;
    Ok(())
}

/// One received packet: the request (or why it does not decode) and its fds.
type Packet = (Result<Request, Refusal>, Vec<OwnedFd>);

/// The next packet, None at end of channel. A packet over [`MAX_MESSAGE`], with truncated fds, or not JSON decodes as a refusal.
fn next(channel: BorrowedFd<'_>) -> io::Result<Option<Packet>> {
    let mut buf = vec![0u8; MAX_MESSAGE + 1];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let got = recvmsg(
        channel,
        &mut [IoSliceMut::new(&mut buf)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    let mut fds = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(r) = m {
            fds.extend(r);
        }
    }
    if got.bytes == 0 && fds.is_empty() {
        return Ok(None);
    }
    let req = if got.flags.contains(ReturnFlags::CTRUNC) || fds.len() != FDS {
        Err(refuse("fds", format!("{} fds", fds.len())))
    } else if got.bytes > MAX_MESSAGE || got.flags.contains(ReturnFlags::TRUNC) {
        Err(refuse("size", format!("{} bytes", got.bytes)))
    } else {
        serde_json::from_slice(&buf[..got.bytes]).map_err(|e| refuse("decode", e.to_string()))
    };
    Ok(Some((req, fds)))
}

/// The broker's side: answers each request on `channel` until the app's end closes. `start` gets an admitted plan and the request's fds (WebKit's socket, stdout, stderr: the reply
/// socket is kept here). A packet without a reply socket is counted and dropped (nothing to answer on).
pub fn serve(
    channel: impl AsFd,
    app: &AppView,
    mut start: impl FnMut(&Plan, [OwnedFd; 3]) -> Reply,
) -> io::Result<()> {
    while let Some((req, fds)) = next(channel.as_fd())? {
        let req = req.map_err(|r| {
            atl_diag::refused(INTERFACE, r.what);
            Reply::Refused {
                what: r.what.to_owned(),
                why: r.why,
            }
        });
        let Ok([socket, reply, stdout, stderr]) = <[OwnedFd; FDS]>::try_from(fds) else {
            continue;
        };
        let answer = match req.and_then(|r| admit(&r, app)) {
            Ok(plan) => start(&plan, [socket, stdout, stderr]),
            Err(refusal) => refusal,
        };
        // a shim that went away is no error of the channel's
        let _ = write_reply(&reply, &answer);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    const WEB: &str = "/usr/lib/x86_64-linux-gnu/webkitgtk-6.0/WebKitWebProcess";

    fn app() -> AppView {
        AppView {
            uid: 10_123,
            devices: vec![PathBuf::from("/dev/dri")],
            writable: vec![Mount {
                host: PathBuf::from("/host/appdata/com.example"),
                at: PathBuf::from("/var/appdata/com.example"),
                writable: true,
            }],
        }
    }

    /// The line WebKit 2.54 builds for a web process (FlatpakLauncher.cpp flatpakSpawn), fd 15.
    fn web() -> Request {
        Request {
            program: WEB.into(),
            args: vec!["7".into(), "15".into()],
            forward_fd: 15,
            env: vec![
                ("HOME".into(), "/tmp".into()),
                ("LD_LIBRARY_PATH".into(), "/opt/lib64".into()),
                (FD_ENV.into(), "9".into()),
            ],
            directory: "/".into(),
            sandbox: true,
            network: false,
            flags: SANDBOX_FLAGS.iter().map(|f| (*f).to_owned()).collect(),
            expose: vec![],
        }
    }

    /// Host filesystem stand-ins: every plain path is its own canonical form and exists, except names holding `missing` / `link`.
    fn fs_check(req: &Request) -> Result<Plan, Refusal> {
        check(
            req,
            &app(),
            |p| (!p.to_string_lossy().contains("link")).then(|| p.to_path_buf()),
            |p| !p.to_string_lossy().contains("missing"),
        )
    }

    #[test]
    fn webkit_line_is_admitted() {
        let plan = fs_check(&web()).expect("admitted");
        assert_eq!(plan.program, PathBuf::from(WEB));
        assert_eq!(plan.identifier, "7");
        assert!(plan.gpu);
        // the loader's variables and the broker's fds are dropped, the rest kept in order
        assert_eq!(plan.env, vec![("HOME".into(), "/tmp".into())]);
    }

    /// Only WebKit's web process under /usr is spawnable: another binary, WebKit's other helpers, a copy outside /usr, a path that is not plain or not canonical are refused as `program`.
    #[test]
    fn only_the_web_process_is_spawnable() {
        for p in [
            "/usr/bin/sh",
            "/usr/lib/x86_64-linux-gnu/webkitgtk-6.0/WebKitNetworkProcess",
            "/usr/lib/x86_64-linux-gnu/webkitgtk-6.0/WebKitGPUProcess",
            "/var/appdata/com.example/webkitgtk-6.0/WebKitWebProcess",
            "/tmp/webkitgtk-6.0/WebKitWebProcess",
            "/usr/lib/../../data/data/com.example/webkitgtk-6.0/WebKitWebProcess",
            "usr/lib/webkitgtk-6.0/WebKitWebProcess",
            "/usr/lib/link/webkitgtk-6.0/WebKitWebProcess",
            "/usr/lib/webkitgtk-4.1/WebKitWebProcess",
        ] {
            let req = Request {
                program: p.into(),
                ..web()
            };
            assert_eq!(
                fs_check(&req).map_err(|r| r.what),
                Err("program"),
                "{p} must be refused"
            );
        }
    }

    #[test]
    fn other_lines_are_refused() {
        let cases: Vec<(Request, &str)> = vec![
            (
                Request {
                    sandbox: false,
                    ..web()
                },
                "unsandboxed",
            ),
            (
                Request {
                    network: true,
                    ..web()
                },
                "network",
            ),
            (
                Request {
                    directory: "/var".into(),
                    ..web()
                },
                "directory",
            ),
            (
                Request {
                    args: vec!["7".into(), "16".into()],
                    ..web()
                },
                "args",
            ),
            (
                Request {
                    args: vec![
                        "7".into(),
                        "15".into(),
                        "--configure-jsc-for-testing".into(),
                    ],
                    ..web()
                },
                "args",
            ),
            (
                Request {
                    args: vec!["x".into(), "15".into()],
                    ..web()
                },
                "args",
            ),
            (
                Request {
                    flags: vec!["share-everything".into()],
                    ..web()
                },
                "flag",
            ),
        ];
        for (req, what) in cases {
            assert_eq!(fs_check(&req).map_err(|r| r.what), Err(what), "{req:?}");
        }
    }

    /// Expose paths lie in the app's writable dirs, plain; a missing `-try` path is skipped, a missing required one refused.
    #[test]
    fn expose_paths_stay_in_the_app_data() {
        let with = |path: &str, must_exist: bool| Request {
            expose: vec![Expose {
                path: path.into(),
                writable: true,
                must_exist,
            }],
            ..web()
        };
        let plan = fs_check(&with("/var/appdata/com.example/cache/gst", true)).expect("inside");
        assert_eq!(
            plan.expose,
            vec![Mount {
                host: "/var/appdata/com.example/cache/gst".into(),
                at: "/var/appdata/com.example/cache/gst".into(),
                writable: true
            }]
        );
        assert!(
            fs_check(&with("/var/appdata/com.example/missing", false))
                .expect("try")
                .expose
                .is_empty()
        );
        for (p, what) in [
            ("/var/appdata/com.example/missing", "expose-missing"),
            ("/var/appdata/com.other", "expose"),
            ("/var/appdata/com.example/../com.other", "expose"),
            ("/usr", "expose"),
            ("/tmp", "expose"),
            ("data/data/com.example", "expose"),
        ] {
            assert_eq!(
                fs_check(&with(p, true)).map_err(|r| r.what),
                Err(what),
                "{p}"
            );
        }
    }

    fn pair() -> (OwnedFd, OwnedFd) {
        socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair")
    }

    fn count(what: &str) -> u64 {
        let api = format!("{INTERFACE}.{what}");
        atl_diag::counts()
            .into_iter()
            .find(|(o, a, _)| o == "refused" && *a == api)
            .map_or(0, |(_, _, n)| n)
    }

    /// Over a real channel: a refused program is answered `refused` and counted, `start` never runs; packets with the wrong fd count are counted too.
    #[test]
    fn channel_refuses_and_counts() {
        let (shim, broker) = pair();
        let server = std::thread::spawn(move || {
            let mut started = 0;
            serve(&broker, &app(), |_, _| {
                started += 1;
                Reply::Started { pid: 5 }
            })
            .expect("serve");
            started
        });
        let before = count("program");
        let (reply_ours, reply_theirs) = pair();
        let (sock, _keep) = pair();
        let req = Request {
            program: "/usr/bin/sh".into(),
            ..web()
        };
        let out = std::io::stdout();
        let err = std::io::stderr();
        send_request(
            &shim,
            &req,
            [sock.as_fd(), reply_theirs.as_fd(), out.as_fd(), err.as_fd()],
        )
        .expect("send");
        drop(reply_theirs);
        assert_eq!(
            read_reply(&reply_ours).expect("reply"),
            Reply::Refused {
                what: "program".into(),
                why: "/usr/bin/sh".into()
            }
        );
        assert_eq!(count("program"), before + 1);
        drop(shim);
        assert_eq!(server.join().expect("server"), 0);
    }
}
