// SPDX-License-Identifier: MIT
//! A private D-Bus session bus for a gate, and a fake freedesktop notification service on it (notifications plan Gates): atld is pointed at the bus by its environment
//! (`DBUS_SESSION_BUS_ADDRESS`), so a test never reaches the user's own session. The bus reads its own config: no service directories (nothing on the host is ever activated), any name may be owned.
//! The fake service records every call; it leaves the bus when it closes its first notification, so the next post finds no service (the gate's tier-none row).
//! Needs `dbus-daemon` on PATH (deps.toml `dbus-daemon`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// The freedesktop notification service's bus name, object path and interface (Desktop Notifications 1.3).
const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// How long the bus may take to make its socket.
const BUS_START: Duration = Duration::from_secs(10);

/// A `dbus-daemon` this gate started, listening at a socket in its own directory; killed (by its pid) on drop.
pub struct PrivateBus {
    child: Child,
    /// the address a client is given
    pub address: String,
}

impl PrivateBus {
    /// Starts the bus in `dir` (made if missing).
    pub fn start(dir: &Path) -> Result<PrivateBus> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let socket = dir.join("bus");
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        let config = dir.join("session.conf");
        std::fs::write(&config, config_text(&socket))
            .with_context(|| format!("writing {}", config.display()))?;
        let child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .context("starting dbus-daemon (deps.toml dbus-daemon)")?;
        let mut bus = PrivateBus {
            child,
            address: format!("unix:path={}", socket.display()),
        };
        let start = Instant::now();
        while !socket.exists() {
            if let Some(s) = bus.child.try_wait()? {
                bail!("dbus-daemon exited {s} before making {}", socket.display());
            }
            if start.elapsed() > BUS_START {
                bail!("dbus-daemon made no socket in {BUS_START:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(bus)
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The bus's whole configuration: a session bus at `socket`, EXTERNAL auth, everything allowed, no `<servicedir>` (so nothing is activated).
fn config_text(socket: &Path) -> String {
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        socket.display()
    )
}

/// One call the fake service received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `Notify`: its arguments, the hints as text (bytes and booleans in decimal / `true`), and the id answered.
    Notify {
        app_name: String,
        replaces_id: u32,
        summary: String,
        body: String,
        hints: BTreeMap<String, String>,
        expire_timeout: i32,
        id: u32,
    },
    /// `CloseNotification(id)`.
    Close(u32),
}

/// The calls so far, in order.
pub type Calls = Arc<Mutex<Vec<Call>>>;

struct Fake {
    calls: Calls,
    next: u32,
}

/// A hint's value as text: the kinds the spec's hints use.
fn hint_text(v: &zbus::Variant<'_>) -> String {
    match v {
        zbus::Variant::U8(n) => n.to_string(),
        zbus::Variant::Bool(b) => b.to_string(),
        zbus::Variant::I32(n) => n.to_string(),
        zbus::Variant::Str(s) => s.as_str().to_owned(),
        other => format!("{other:?}"),
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Fake {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        hints: std::collections::HashMap<String, zbus::OwnedVariant>,
        expire_timeout: i32,
    ) -> u32 {
        // spec: a replaces_id the server knows keeps that id; 0 asks for a new one
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next += 1;
            self.next
        };
        let hints = hints
            .iter()
            .map(|(k, v)| (k.clone(), hint_text(v)))
            .collect();
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Call::Notify {
                app_name,
                replaces_id,
                summary,
                body,
                hints,
                expire_timeout,
                id,
            });
        id
    }

    /// Records the close, then leaves the bus: the gate's next post finds no service.
    async fn close_notification(&self, #[zbus(connection)] conn: &zbus::Connection, id: u32) {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Call::Close(id));
        let _ = conn.release_name(NAME).await;
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_owned()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "atl-gate".to_owned(),
            "atl".to_owned(),
            "1".to_owned(),
            "1.3".to_owned(),
        )
    }
}

/// The fake service, connected to `bus` and owning the notification name until its first close. Dropping it ends its connection.
pub struct FakeNotifications {
    _conn: zbus::Connection,
    pub calls: Calls,
}

impl FakeNotifications {
    pub fn serve(bus: &PrivateBus) -> Result<FakeNotifications> {
        let calls = Calls::default();
        let fake = Fake {
            calls: Arc::clone(&calls),
            next: 0,
        };
        let conn = zbus::block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())
                .serve_at(PATH, fake)
                .name(NAME)
                .build()
                .await
        })
        .context("the fake notification service on the private bus")?;
        Ok(FakeNotifications { _conn: conn, calls })
    }

    /// The calls so far.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The address atld gets when a gate has no bus for it: a socket that never exists, so atld's notification tier is none and nothing reaches the user's session bus.
pub fn no_bus(scratch: &Path) -> String {
    let path: PathBuf = scratch.join("no-session-bus");
    format!("unix:path={}", path.display())
}
