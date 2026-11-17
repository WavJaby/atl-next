// SPDX-License-Identifier: MIT
//! The web-process channel (owner D2, 2026-11-15): an app process asks the broker for exactly one thing, "start a WebKit web process for me", and gets a process weaker than itself.
//! WebKit (host WebKitGTK in the app process) sandboxes its web process through Flatpak's route when `/.flatpak-info` exists: it runs `flatpak-spawn --sandbox ... WebKitWebProcess`, found in PATH.
//! Our `flatpak-spawn` (crate atl-flatpak-spawn, bound at [`SHIM_AT`]) turns that line into one [`Request`] on this channel; the broker checks it ([`admit`]) and starts the web process through
//! atl-sandbox-join in [`crate::Profile::WebProcess`]: a child pid namespace of the app's (WebKit takes the pid from SCM credentials and asserts it non-zero), built from the app's own mount view.
//!
//! Channel: one SEQPACKET socketpair per app sandbox, the app's end at fd [`FD_ENV`]; identity is the socket itself, so a request names no app. One JSON request per packet with [`FDS`] fds
//! (SCM_RIGHTS): WebKit's socket for the child, a reply socket (one per request: concurrent shims share the channel), stdout, stderr. The reply ([`Reply`]) goes to the reply socket.
//! Not here, on purpose: any other program, an unsandboxed start, a kill (the web process ends with its IPC socket, or with the app's pid namespace), a bind outside the app's writable dirs (a read-only expose of what the sandbox already shows read-only is admitted as a no-op).

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
/// `--sandbox-flag` values WebKit passes (FlatpakLauncher.cpp); `share-gpu` gives the app's DRM nodes, `share-sound` the host's PulseAudio-protocol socket ([`AppView::pulse`], the web process's only
/// sound grant: WebKitGTK plays through GStreamer's autoaudiosink, which picks pulsesink), the rest give nothing (no session bus, no display socket is shown).
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
    /// The PulseAudio-protocol socket the app's view shows at its own path (crate::WebSpawnHost::pulse), granted under `share-sound`; None: the host has none.
    pub pulse: Option<PathBuf>,
    /// The app's desktop identity (crate::Spawn::app_id): the web process's sound carries it ([`PULSE_APP_ID`]), so the desktop shows it, and keeps its saved volume, as the app's. None: a process of no package.
    pub app_id: Option<String>,
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
    /// Read-only expose paths (canonical) the sandbox already shows read-only: admitted, nothing bound ([`RO_EXPOSE_ALREADY_BOUND`]). WebKitGTK 2.52 asks for its injected-bundle dir under `/usr`.
    pub already_bound: Vec<PathBuf>,
    pub gpu: bool,
    /// `share-sound` asked for.
    pub sound: bool,
    /// The pulse socket granted (bound at its own path; `env` names it as `PULSE_SERVER`, as Flatpak's `--socket=pulseaudio` does): `sound` and the app has one.
    pub pulse: Option<PathBuf>,
}

/// The variable libpulse reads first for its server (Flatpak sets it for `--socket=pulseaudio`).
pub const PULSE_SERVER: &str = "PULSE_SERVER";
/// libpulse merges `PULSE_PROP_<key>` into every client's proplist; `application.id` reaches the stream's node (measured on PipeWire 1.4 and 1.6). `application.name` and the icon do not: the client's own name wins.
pub const PULSE_APP_ID: &str = "PULSE_PROP_application.id";

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

/// Variables the web process does not get: the loader's (the paths they name are not in its view), the broker's own fds, and libpulse's (`share-sound` alone names its server).
fn dropped(name: &str) -> bool {
    name.starts_with("LD_")
        || name.starts_with("PULSE_")
        || name == FD_ENV
        || name == crate::CONTROL_ENV
}

/// Whether the web sandbox shows `p` read-only from the host: under a [`crate::APP_ROOT_DIRS`] dir, bound at its own path in [`crate::Profile::WebProcess`] (`bwrap_args`). By text: pass plain or canonical paths.
fn ro_bound(p: &Path) -> bool {
    crate::APP_ROOT_DIRS
        .iter()
        .any(|d| p.starts_with(Path::new("/").join(d)))
}

/// Checks `req` against the app's view. `canonical`: a path's canonical form on the host (None = absent); `is_file`: whether a host path is a regular file. Refusals are not counted here ([`admit`] counts).
pub fn check(
    req: &Request,
    app: &AppView,
    canonical: impl Fn(&Path) -> Option<PathBuf>,
    is_file: impl Fn(&Path) -> bool,
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
    if !program_allowed(&program)
        || canonical(&program).as_deref() != Some(program.as_path())
        || !is_file(&program)
    {
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
    let mut already_bound = Vec::new();
    for e in &req.expose {
        let at = PathBuf::from(&e.path);
        if !plain(&at) {
            return Err(refuse("expose", e.path.clone()));
        }
        let host = app
            .writable
            .iter()
            .find_map(|m| Some(m.host.join(at.strip_prefix(&m.at).ok()?)));
        let Some(host) = host else {
            // ro outside the app's data: admitted only where the web sandbox already shows the host read-only, judged on the canonical path (a link out of that dir is refused); no bind added
            if e.writable || !ro_bound(&at) {
                return Err(refuse("expose", e.path.clone()));
            }
            match canonical(&at) {
                None if e.must_exist => return Err(refuse("expose-missing", e.path.clone())),
                None => {}
                Some(c) if ro_bound(&c) => already_bound.push(c),
                Some(c) => return Err(refuse("expose", format!("{} -> {}", e.path, c.display()))),
            }
            continue;
        };
        if canonical(&host).is_none() {
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
    let sound = req.flags.iter().any(|f| f == "share-sound");
    let pulse = app.pulse.clone().filter(|_| sound);
    let mut env: Vec<(OsString, OsString)> = req
        .env
        .iter()
        .filter(|(k, _)| !dropped(k))
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    if let Some(p) = &pulse {
        let mut v = OsString::from("unix:");
        v.push(p);
        env.push((PULSE_SERVER.into(), v));
        // host-decided, as the server is: a request-carried PULSE_* was dropped above
        if let Some(id) = &app.app_id {
            env.push((PULSE_APP_ID.into(), id.into()));
        }
    }
    Ok(Plan {
        program,
        identifier,
        env,
        expose,
        already_bound,
        gpu: req.flags.iter().any(|f| f == "share-gpu"),
        sound,
        pulse,
    })
}

/// [`admit`]'s count (`answered WebProcessSpawn.<this>`) per read-only expose the web sandbox already shows.
pub const RO_EXPOSE_ALREADY_BOUND: &str = "ro-expose-already-bound";
/// [`admit`]'s count per `share-sound` granted the pulse socket.
pub const SOUND_PULSE: &str = "sound-pulse";
/// [`admit`]'s count per `share-sound` with no pulse socket on the host: admitted, nothing granted (the web process plays into no server).
pub const SOUND_NO_SERVER: &str = "sound-no-server";

/// [`check`] on the host's filesystem, a refusal counted (`refused WebProcessSpawn.<what>`) and answered.
pub fn admit(req: &Request, app: &AppView) -> Result<Plan, Reply> {
    check(req, app, |p| std::fs::canonicalize(p).ok(), Path::is_file)
        .inspect(|plan| {
            for _ in &plan.already_bound {
                atl_diag::answered(INTERFACE, RO_EXPOSE_ALREADY_BOUND);
            }
            if plan.sound {
                let what = if plan.pulse.is_some() {
                    SOUND_PULSE
                } else {
                    SOUND_NO_SERVER
                };
                atl_diag::answered(INTERFACE, what);
            }
        })
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
            pulse: None,
            app_id: Some("atl.com.example".into()),
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

    /// Where a stand-in `link` resolves: outside every bind.
    const ESCAPE: &str = "/home/user/secret";

    /// Host filesystem stand-ins: every path exists as its own canonical form, except names holding `missing` (absent) or `link` (resolves to [`ESCAPE`]); files are the `*Process` names.
    fn fs_check(req: &Request) -> Result<Plan, Refusal> {
        check(
            req,
            &app(),
            |p| {
                let s = p.to_string_lossy();
                if s.contains("missing") {
                    None
                } else if s.contains("link") {
                    Some(PathBuf::from(ESCAPE))
                } else {
                    Some(p.to_path_buf())
                }
            },
            |p| p.to_string_lossy().ends_with("Process"),
        )
    }

    fn expose(path: &str, writable: bool, must_exist: bool) -> Request {
        Request {
            expose: vec![Expose {
                path: path.into(),
                writable,
                must_exist,
            }],
            ..web()
        }
    }

    /// WebKitGTK 2.52's line: a read-only expose of its injected-bundle dir, which the web sandbox already binds read-only (/usr): admitted, no bind, recorded for the count.
    #[test]
    fn ro_expose_of_a_ro_bound_dir_is_a_no_op() {
        const BUNDLE: &str = "/usr/lib/riscv64-linux-gnu/webkitgtk-6.0/injected-bundle/";
        let plan = fs_check(&expose(BUNDLE, false, true)).expect("admitted");
        assert!(plan.expose.is_empty(), "no bind: {:?}", plan.expose);
        assert_eq!(plan.already_bound, vec![PathBuf::from(BUNDLE)]);
        // a missing -try path is skipped, a missing required one refused, as in the app's dirs
        let missing = "/usr/lib/missing";
        assert!(
            fs_check(&expose(missing, false, false))
                .expect("try")
                .already_bound
                .is_empty()
        );
        assert_eq!(
            fs_check(&expose(missing, false, true)).map_err(|r| r.what),
            Err("expose-missing")
        );
    }

    /// Writable stays app-data-only; a ro path outside every bind, not plain, or linking out of the bind is refused.
    #[test]
    fn ro_expose_gets_no_new_rights() {
        for (p, writable) in [
            (
                "/usr/lib/riscv64-linux-gnu/webkitgtk-6.0/injected-bundle",
                true,
            ),
            ("/home/user", false),
            ("/var/appdata/com.other", false),
            ("/usr/../home/user", false),
            ("/usr/lib/link", false),
        ] {
            assert_eq!(
                fs_check(&expose(p, writable, true)).map_err(|r| r.what),
                Err("expose"),
                "{p} writable={writable}"
            );
        }
    }

    /// Over [`admit`] on the host's filesystem: each no-op is counted, a refused request counts none. The admitted half needs the host's WebKitGTK (skipped without it).
    #[test]
    fn ro_expose_no_op_is_counted() {
        let api = format!("{INTERFACE}.{RO_EXPOSE_ALREADY_BOUND}");
        let n = || {
            atl_diag::counts()
                .into_iter()
                .find(|(o, a, _)| o == "answered" && *a == api)
                .map_or(0, |(_, _, n)| n)
        };
        let before = n();
        let req = Request {
            directory: "/var".into(),
            ..expose("/usr", false, true)
        };
        // refused (directory, a counter no other test reads) before any expose is judged: not counted
        assert!(admit(&req, &app()).is_err());
        assert_eq!(n(), before);
        if Path::new(WEB).is_file() {
            let plan = admit(&expose("/usr/lib", false, true), &app()).expect("admitted");
            assert!(plan.expose.is_empty());
            assert_eq!(n(), before + 1);
        }
    }

    #[test]
    fn webkit_line_is_admitted() {
        let plan = fs_check(&web()).expect("admitted");
        assert_eq!(plan.program, PathBuf::from(WEB));
        assert_eq!(plan.identifier, "7");
        assert!(plan.gpu);
        // the loader's variables and the broker's fds are dropped, the rest kept in order
        assert_eq!(plan.env, vec![("HOME".into(), "/tmp".into())]);
        // share-sound asked, the app has no pulse socket: nothing granted
        assert!(plan.sound);
        assert_eq!(plan.pulse, None);
    }

    /// share-sound with the app's pulse socket: the socket, PULSE_SERVER naming it (Flatpak's --socket=pulseaudio) and the app's own id, replacing any PULSE_* the request carried; without the flag, neither.
    #[test]
    fn share_sound_grants_the_pulse_socket() {
        const SOCKET: &str = "/run/user/1000/pulse/native";
        let app = AppView {
            pulse: Some(SOCKET.into()),
            ..app()
        };
        let mut req = web();
        req.env
            .push((PULSE_SERVER.into(), "unix:/mnt/other".into()));
        req.env
            .push(("PULSE_COOKIE".into(), "/home/user/cookie".into()));
        req.env.push((PULSE_APP_ID.into(), "atl.com.other".into()));
        let check = |req: &Request| check(req, &app, |p| Some(p.to_path_buf()), |_| true);
        let plan = check(&req).expect("admitted");
        assert_eq!(plan.pulse.as_deref(), Some(Path::new(SOCKET)));
        assert_eq!(
            plan.env,
            vec![
                ("HOME".into(), "/tmp".into()),
                (PULSE_SERVER.into(), format!("unix:{SOCKET}").into()),
                // the fixture app's id (fn app), not the request's
                (PULSE_APP_ID.into(), "atl.com.example".into()),
            ]
        );
        req.flags.retain(|f| f != "share-sound");
        let plan = check(&req).expect("admitted");
        assert!(!plan.sound);
        assert_eq!(plan.pulse, None);
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
