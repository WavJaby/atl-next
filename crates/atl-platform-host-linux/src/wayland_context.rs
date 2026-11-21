// SPDX-License-Identifier: MIT
//! Per-process Wayland security contexts (staging protocol `wp_security_context_v1`, the mechanism Flatpak uses): atld makes one listening socket per app process and registers it with the compositor under
//! sandbox engine + app id + instance id; the app sandbox shows only that socket, at the compositor socket's path, so the compositor can withhold its privileged globals (screen capture, clipboard managers,
//! foreign toplevels, virtual input) from every client connecting through it. Which globals a compositor withholds is its own policy, not the protocol's.
//! Tier: chosen by probing the compositor for `wp_security_context_manager_v1` ([`Contexts::probe`]); without it the app gets the compositor socket as-is and atld says so.
//! A context ends when its [`AppContext`] drops: the close pipe's write end closes, the compositor sees hangup on close_fd and stops accepting on the listener (existing connections stay); the socket file goes.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::wp::security_context::v1::client::wp_security_context_manager_v1::WpSecurityContextManagerV1;
use wayland_protocols::wp::security_context::v1::client::wp_security_context_v1::WpSecurityContextV1;

/// The sandbox engine name every context carries: the root of the desktop identities ATL gives apps (`atl.<package>`, atl-launch `app_id`), so app id + engine name one app, as the protocol asks.
pub const SANDBOX_ENGINE: &str = "atl";

/// The interface the probe looks for.
pub const MANAGER_INTERFACE: &str = "wp_security_context_manager_v1";

/// Dispatch state of atld's own connection: it only sends requests; the registry's later events are dropped.
struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: WpSecurityContextManagerV1);
delegate_noop!(State: WpSecurityContextV1);

struct Conn {
    queue: EventQueue<State>,
    manager: WpSecurityContextManagerV1,
    // held: the manager lives on it; the compositor keeps listeners after a disconnect too (protocol `create_listener`)
    _conn: Connection,
}

/// The compositor's security-context manager on atld's own connection; one per run.
pub struct Contexts {
    conn: Mutex<Conn>,
    /// where the listening sockets go: `atl/` beside the compositor socket (the user's runtime dir)
    dir: PathBuf,
    seq: AtomicU64,
}

/// What the probe found.
pub enum Probe {
    Available(Contexts),
    /// why apps get the compositor socket as-is: the compositor offers no manager, or atld could not ask it
    Unavailable(String),
}

impl Contexts {
    /// Connects to the compositor at `socket` and binds `wp_security_context_manager_v1` (version 1, the only one).
    pub fn probe(socket: &Path) -> Probe {
        match Self::connect(socket) {
            Ok(Some(c)) => Probe::Available(c),
            Ok(None) => Probe::Unavailable(format!("compositor offers no {MANAGER_INTERFACE}")),
            Err(e) => Probe::Unavailable(format!("probe: {e}")),
        }
    }

    fn connect(socket: &Path) -> io::Result<Option<Self>> {
        let conn =
            Connection::from_socket(UnixStream::connect(socket)?).map_err(io::Error::other)?;
        let (globals, queue) = registry_queue_init::<State>(&conn).map_err(io::Error::other)?;
        let qh = queue.handle();
        let Ok(manager) = globals.bind::<WpSecurityContextManagerV1, _, _>(&qh, 1..=1, ()) else {
            return Ok(None);
        };
        let dir = socket
            .parent()
            .ok_or_else(|| io::Error::other("compositor socket has no parent dir"))?
            .join("atl");
        std::fs::create_dir_all(&dir)?;
        Ok(Some(Self {
            conn: Mutex::new(Conn {
                queue,
                manager,
                _conn: conn,
            }),
            dir,
            seq: AtomicU64::new(0),
        }))
    }

    /// A new context for one app process: `app_id` (None: a process of no package) and an instance id unique among this atld's contexts (`<atld pid>.<n>`).
    /// Returns once the compositor has taken the listener (a roundtrip after `commit`), so the app may connect at once; a protocol error is returned, never a socket the compositor does not serve.
    pub fn create(&self, app_id: Option<&str>) -> io::Result<AppContext> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let instance = format!("{}.{n}", std::process::id());
        let path = self.dir.join(format!("wayland-{instance}"));
        // a stale file of an earlier atld with the same pid (crash) would make bind fail
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        // removes the socket file on every error below
        let mut ctx = AppContext { path, _close: None };
        let (close_r, close_w) = std::io::pipe()?;
        let mut c = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let qh = c.queue.handle();
        let sc = c.manager.create_listener(
            std::os::fd::AsFd::as_fd(&listener),
            std::os::fd::AsFd::as_fd(&close_r),
            &qh,
            (),
        );
        sc.set_sandbox_engine(SANDBOX_ENGINE.to_owned());
        if let Some(id) = app_id {
            sc.set_app_id(id.to_owned());
        }
        sc.set_instance_id(instance);
        sc.commit();
        sc.destroy();
        c.queue.roundtrip(&mut State).map_err(io::Error::other)?;
        // our copies: the compositor holds its own listen_fd and close_fd (received over the socket)
        drop((listener, close_r));
        ctx._close = Some(close_w);
        Ok(ctx)
    }
}

/// One app process's context: its socket, served by the compositor until this drops.
pub struct AppContext {
    path: PathBuf,
    /// the close pipe's write end: dropping it hangs up the compositor's close_fd
    _close: Option<std::io::PipeWriter>,
}

impl AppContext {
    /// The listening socket the app's sandbox shows at the compositor socket's path.
    pub fn socket(&self) -> &Path {
        &self.path
    }
}

impl Drop for AppContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
