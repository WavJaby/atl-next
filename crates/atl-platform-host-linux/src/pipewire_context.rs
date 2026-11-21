// SPDX-License-Identifier: MIT
//! Per-process PipeWire security contexts (`PipeWire:Interface:SecurityContext`, pipewire >= 1.0: the mechanism Flatpak and `pw-container` use) and atld as the permission manager of the clients arriving through them.
//! atld makes one listening socket per app process and hands it to PipeWire with `pipewire.sec.engine` / `.app-id` / `.instance-id` + `pipewire.access`; PipeWire copies those onto every client connecting through it,
//! and a client can neither set nor change them (pipewire impl-client.c `check_client_property_update`). The sandbox shows only that socket.
//! Tiers, chosen by probing (`Manager::start`): no SecurityContext global -> `Unavailable`. Else a probe client connects through a context with an access value the session manager has no rule for and atld reads
//! its default permissions: still empty -> `Enforced` (the session manager grants nothing, so atld's per-object grants are the whole truth); anything granted -> `WriteRestricted`: contexts carry
//! `pipewire.access=restricted`, which stock WirePlumber maps to read+execute on every object (no write, no metadata), and atld grants nothing itself -- visibility and capture stay open there.
//! An unknown access value on such a host is never used for apps: it means full access on wireplumber 0.5.19 (measured W 40, M 98).
//! Grants (Enforced): core RX, own client RWX, `client-node` factory RX, every Audio/Sink node + its ports R (a link needs the stream owner's R on the other node: impl-link.c `check_owner_permissions`),
//! own nodes + ports RWX, Audio/Source nodes + ports R only with RECORD_AUDIO, Video/Source + ports R only with CAMERA; everything else 0. Revocation sets ports too: link teardown listens on port globals.
//! A context ends when its [`AppContext`] drops: the close pipe's write end closes, PipeWire stops accepting on the listener (existing connections stay); the socket file goes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CString, c_int, c_void};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use pipewire as pw;
use pw::client::Client;
use pw::permissions::{Permission, PermissionFlags as P};
use pw::types::ObjectType;

/// The sandbox engine name every context carries (same as the Wayland contexts: `wayland_context::SANDBOX_ENGINE`).
pub const SANDBOX_ENGINE: &str = "atl";
/// The interface the probe looks for (pipewire extensions/security-context.h `PW_TYPE_INTERFACE_SecurityContext`).
pub const SECURITY_CONTEXT_INTERFACE: &str = "PipeWire:Interface:SecurityContext";
/// The access value of Enforced-tier contexts and of the tier probe: no stock session-manager rule names it.
const ACCESS_ATL: &str = "atl";
/// WirePlumber's read+execute value (0.5.8 access-default.lua, 0.5.19 find-default-access.lua): the WriteRestricted tier.
const ACCESS_RESTRICTED: &str = "restricted";
/// How long the probe waits for the session manager to act on its client before reading its permissions. WirePlumber handles a client within its next main-loop dispatches after
/// `client-added` (select-access event chain); 1 s leaves two orders of magnitude over a loaded desktop's dispatch latency. A slower session manager reads as Enforced too early -> its
/// grants then reach app clients; the gate row's foreign-node count catches that.
const PROBE_SETTLE: Duration = Duration::from_millis(1000);
const PROBE_POLL: Duration = Duration::from_millis(100);

/// The tier the probe chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// the session manager grants context clients nothing: atld's allow-list is the whole truth
    Enforced,
    /// the session manager grants context clients read to everything: contexts use `pipewire.access=restricted` (no write, no metadata); capture is not restricted
    WriteRestricted,
}

impl Tier {
    /// The log word: `enforced` / `write-restricted`.
    pub fn name(self) -> &'static str {
        match self {
            Tier::Enforced => "enforced",
            Tier::WriteRestricted => "write-restricted",
        }
    }
}

/// What the probe found.
pub enum Probe {
    Available(Manager),
    /// why apps get no context: no SecurityContext global, or atld could not ask PipeWire
    Unavailable(String),
}

/// Android permission state the grants follow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Grants {
    pub record_audio: bool,
    pub camera: bool,
}

enum Cmd {
    Create {
        app_id: Option<String>,
        instance: String,
        path: PathBuf,
        grants: Grants,
        reply: mpsc::Sender<io::Result<OwnedFd>>,
    },
    Grants {
        instance: String,
        grants: Grants,
    },
    Release {
        instance: String,
    },
}

/// atld's connection to PipeWire, on its own thread (pipewire-rs objects are thread-bound); one per run.
pub struct Manager {
    tx: pw::channel::Sender<Cmd>,
    tier: Tier,
    /// default permission bits the probe client got from the session manager (0 in Enforced)
    probe_default: u32,
    dir: PathBuf,
    seq: std::sync::atomic::AtomicU64,
}

impl Manager {
    /// Connects to the session's PipeWire (`remote`: the socket apps would get), looks for the SecurityContext global and runs the tier probe.
    pub fn start(remote: &Path) -> Probe {
        let Some(dir) = remote.parent().map(|d| d.join("atl")) else {
            return Probe::Unavailable("PipeWire socket has no parent dir".into());
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Probe::Unavailable(format!("{}: {e}", dir.display()));
        }
        let (tx, rx) = pw::channel::channel::<Cmd>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(Tier, u32), String>>();
        let remote = remote.to_owned();
        let probe_path = dir.join(format!("pipewire-{}.probe", std::process::id()));
        let spawned = std::thread::Builder::new()
            .name("atl-pw-sec".into())
            .spawn(move || {
                if let Err(e) = run(&remote, probe_path, rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            });
        if let Err(e) = spawned {
            return Probe::Unavailable(format!("thread: {e}"));
        }
        match ready_rx.recv() {
            Ok(Ok((tier, probe_default))) => Probe::Available(Self {
                tx,
                tier,
                probe_default,
                dir,
                seq: Default::default(),
            }),
            Ok(Err(e)) => Probe::Unavailable(e),
            Err(_) => Probe::Unavailable("PipeWire thread ended".into()),
        }
    }

    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Default permission bits (pipewire `PW_PERM_*`) the session manager gave the probe client: 0 in [`Tier::Enforced`].
    pub fn probe_default(&self) -> u32 {
        self.probe_default
    }

    /// A new context for one app process: `app_id` (None: a process of no package), instance id `<atld pid>.<n>`. Returns once PipeWire has taken the listener (a core sync after create).
    pub fn create(&self, app_id: Option<&str>, grants: Grants) -> io::Result<AppContext> {
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let instance = format!("{}.{n}", std::process::id());
        let path = self.dir.join(format!("pipewire-{instance}"));
        let (reply, got) = mpsc::channel();
        self.tx
            .send(Cmd::Create {
                app_id: app_id.map(str::to_owned),
                instance: instance.clone(),
                path: path.clone(),
                grants,
                reply,
            })
            .map_err(|_| io::Error::other("PipeWire thread ended"))?;
        let close = got
            .recv()
            .map_err(|_| io::Error::other("PipeWire thread ended"))??;
        Ok(AppContext {
            path,
            instance,
            _close: close,
            tx: self.tx.clone(),
        })
    }

    /// Re-evaluates one context's grants (an Android runtime permission changed). No effect in [`Tier::WriteRestricted`].
    pub fn set_grants(&self, ctx: &AppContext, grants: Grants) {
        let _ = self.tx.send(Cmd::Grants {
            instance: ctx.instance.clone(),
            grants,
        });
    }
}

/// One app process's context: its socket, served by PipeWire until this drops.
pub struct AppContext {
    path: PathBuf,
    instance: String,
    /// the close pipe's write end: dropping it hangs up PipeWire's close_fd
    _close: OwnedFd,
    tx: pw::channel::Sender<Cmd>,
}

impl AppContext {
    /// The listening socket the app's sandbox shows at the PipeWire socket's path.
    pub fn socket(&self) -> &Path {
        &self.path
    }
}

impl Drop for AppContext {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Release {
            instance: std::mem::take(&mut self.instance),
        });
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Client,
    Node { class: String, owner: Option<u32> },
    Port { node: u32 },
    Factory { name: String },
    Other,
}

/// A client atld watches: every client but atld's own; `instance` once its info names one of atld's contexts (`pipewire.sec.instance-id`, set by atld, unchangeable by the client).
struct Managed {
    instance: Option<String>,
    proxy: Client,
    applied: HashMap<u32, u32>,
    _listener: pw::client::ClientListener,
}

/// A watched client's info arrived: one of atld's contexts -> it is the probe client, or an app client whose grants start now (Enforced only).
fn on_client_info(st: &Rc<RefCell<State>>, id: u32, instance: Option<&str>) {
    let mut s = st.borrow_mut();
    let Some(inst) = instance else { return };
    if s.clients.get(&id).is_none_or(|m| m.instance.is_some()) {
        return;
    }
    if inst == PROBE_INSTANCE && s.tier.is_none() {
        s.probe_client = Some(id);
    } else if !s.apps.contains_key(inst) {
        return;
    }
    if let Some(m) = s.clients.get_mut(&id) {
        m.instance = Some(inst.to_owned());
    }
    if s.tier == Some(Tier::Enforced) {
        regrant(&mut s, id);
    }
}

/// The tier probe context's instance id.
const PROBE_INSTANCE: &str = "probe";

#[derive(Default)]
struct State {
    globals: HashMap<u32, Kind>,
    apps: HashMap<String, Grants>,
    clients: HashMap<u32, Managed>,
    tier: Option<Tier>,
    probe_client: Option<u32>,
    probe_default: Option<u32>,
}

const R: u32 = P::R.bits();
const RX: u32 = P::R.bits() | P::X.bits();
const RWX: u32 = P::R.bits() | P::W.bits() | P::X.bits();

/// The allow-list: what client `me` of an app with `g` may do with global `id` (module doc).
fn wanted(globals: &HashMap<u32, Kind>, me: u32, g: Grants, id: u32) -> u32 {
    let node_perm = |class: &str, owner: Option<u32>| -> u32 {
        if owner == Some(me) {
            RWX
        } else if class.starts_with("Audio/Sink")
            || (g.record_audio && class.starts_with("Audio/Source"))
            || (g.camera && class == "Video/Source")
        {
            R
        } else {
            0
        }
    };
    match globals.get(&id) {
        _ if id == 0 => RX,
        Some(Kind::Client) if id == me => RWX,
        Some(Kind::Factory { name }) if name == "client-node" => RX,
        Some(Kind::Node { class, owner }) => node_perm(class, *owner),
        Some(Kind::Port { node }) => match globals.get(node) {
            Some(Kind::Node { class, owner }) => node_perm(class, *owner),
            _ => 0,
        },
        _ => 0,
    }
}

/// Sends the difference between what `client` has and what the allow-list says; core last, so a fresh client leaves its busy state with every other grant in place.
fn regrant(st: &mut State, client: u32) {
    let State {
        globals,
        apps,
        clients,
        ..
    } = st;
    let Some(m) = clients.get_mut(&client) else {
        return;
    };
    let Some(g) = m.instance.as_ref().and_then(|i| apps.get(i)).copied() else {
        return;
    };
    let mut ids: Vec<u32> = globals
        .keys()
        .copied()
        .chain(m.applied.keys().copied())
        .collect();
    ids.sort_unstable_by_key(|&id| (id == 0, id));
    ids.dedup();
    let mut out = Vec::new();
    for id in ids {
        let want = if globals.contains_key(&id) || id == 0 {
            wanted(globals, client, g, id)
        } else {
            0
        };
        if m.applied.get(&id).copied().unwrap_or(0) != want {
            out.push(Permission::new(id, P::from_bits_truncate(want)));
            if globals.contains_key(&id) || id == 0 {
                m.applied.insert(id, want);
            } else {
                m.applied.remove(&id);
            }
        }
    }
    if !out.is_empty() {
        m.proxy.update_permissions(&out);
    }
}

fn kind_of(g: &pw::registry::GlobalObject<&pw::spa::utils::dict::DictRef>) -> Kind {
    let prop = |k: &str| g.props.and_then(|p| p.get(k)).map(str::to_owned);
    match &g.type_ {
        ObjectType::Client => Kind::Client,
        ObjectType::Node => Kind::Node {
            class: prop("media.class").unwrap_or_default(),
            owner: prop("client.id").and_then(|s| s.parse().ok()),
        },
        ObjectType::Port => match prop("node.id").and_then(|s| s.parse().ok()) {
            Some(node) => Kind::Port { node },
            None => Kind::Other,
        },
        ObjectType::Factory => Kind::Factory {
            name: prop("factory.name").unwrap_or_default(),
        },
        _ => Kind::Other,
    }
}

/// The security-context methods table (pipewire extensions/security-context.h `struct pw_security_context_methods`, PW_VERSION_SECURITY_CONTEXT_METHODS 0): pipewire-sys has no binding for the extension.
#[repr(C)]
struct SecurityContextMethods {
    version: u32,
    add_listener: *const c_void,
    create: Option<
        unsafe extern "C" fn(*mut c_void, c_int, c_int, *const pw::spa::sys::spa_dict) -> c_int,
    >,
}

/// Binds the SecurityContext global `id` and sends one `create` with `listen` / `close` and `props`; the proxy is destroyed after the request is queued (the request is in the stream already).
#[allow(unsafe_code)]
fn security_context_create(
    registry: &pw::registry::RegistryRc,
    id: u32,
    version: u32,
    listen: c_int,
    close: c_int,
    props: &pw::properties::PropertiesBox,
) -> io::Result<()> {
    let ty = CString::new(SECURITY_CONTEXT_INTERFACE).map_err(io::Error::other)?;
    // SAFETY: registry is a live pw_registry proxy owned by this thread; pw_registry_methods.bind is the C API's own method table (pipewire-sys binding); ty outlives the call.
    let proxy: *mut c_void = unsafe {
        pw::spa::spa_interface_call_method!(
            registry.as_raw_ptr(),
            pw::sys::pw_registry_methods,
            bind,
            id,
            ty.as_ptr(),
            version,
            0usize
        )
    };
    if proxy.is_null() {
        return Err(io::Error::other("bind SecurityContext failed"));
    }
    // SAFETY: proxy is the pw_proxy bind returned for a SecurityContext global, whose first member is its spa_interface; the method table layout is the header's (version, add_listener, create);
    // props' dict and the fds stay valid for the call, which marshals them into the outgoing message.
    let res = unsafe {
        let iface: *mut pw::spa::sys::spa_interface = proxy.cast();
        let funcs: *const SecurityContextMethods = (*iface).cb.funcs.cast();
        let r = match (*funcs).create {
            Some(f) => f((*iface).cb.data, listen, close, props.dict().as_raw_ptr()),
            None => -libc_enosys(),
        };
        pw::sys::pw_proxy_destroy(proxy.cast());
        r
    };
    if res < 0 {
        Err(io::Error::from_raw_os_error(-res))
    } else {
        Ok(())
    }
}

fn libc_enosys() -> c_int {
    rustix::io::Errno::NOSYS.raw_os_error()
}

/// One context: listener at `path` + close pipe, registered with PipeWire; returns the close pipe's write end and our copies of listen_fd / close_fd, which the caller drops after a core sync
/// (the message carrying them is flushed by then; PipeWire holds its own copies).
fn make_context(
    registry: &pw::registry::RegistryRc,
    sc: (u32, u32),
    path: &Path,
    props: &pw::properties::PropertiesBox,
) -> io::Result<(OwnedFd, (UnixListener, OwnedFd))> {
    // a stale file of an earlier atld with the same pid (crash) would make bind fail
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let (close_r, close_w) = std::io::pipe()?;
    let (close_r, close_w) = (OwnedFd::from(close_r), OwnedFd::from(close_w));
    if let Err(e) = security_context_create(
        registry,
        sc.0,
        sc.1,
        listener.as_raw_fd(),
        close_r.as_raw_fd(),
        props,
    ) {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok((close_w, (listener, close_r)))
}

fn context_props(
    engine_app: Option<&str>,
    instance: &str,
    access: &str,
) -> pw::properties::PropertiesBox {
    let mut p = pw::properties::PropertiesBox::new();
    p.insert("pipewire.sec.engine", SANDBOX_ENGINE);
    if let Some(a) = engine_app {
        p.insert("pipewire.sec.app-id", a);
    }
    p.insert("pipewire.sec.instance-id", instance);
    p.insert("pipewire.access", access);
    p
}

fn run(
    remote: &Path,
    probe_path: PathBuf,
    rx: pw::channel::Receiver<Cmd>,
    ready: &mpsc::Sender<Result<(Tier, u32), String>>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("main loop: {e}"))?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|e| format!("context: {e}"))?;
    let mut cp = pw::properties::PropertiesBox::new();
    cp.insert("remote.name", remote.to_string_lossy().as_ref());
    cp.insert("application.name", "atld");
    let core = context
        .connect_rc(Some(cp))
        .map_err(|e| format!("connect {}: {e}", remote.display()))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| format!("registry: {e}"))?;
    let st = Rc::new(RefCell::new(State::default()));
    let sc: Rc<RefCell<Option<(u32, u32)>>> = Rc::default();

    let _reg_listener = registry
        .add_listener_local()
        .global({
            let (st, sc, registry) = (st.clone(), sc.clone(), registry.clone());
            move |g| {
                if let ObjectType::Other(t) = &g.type_
                    && t == SECURITY_CONTEXT_INTERFACE
                {
                    *sc.borrow_mut() = Some((g.id, g.version));
                }
                let kind = kind_of(g);
                let mut s = st.borrow_mut();
                s.globals.insert(g.id, kind.clone());
                // a client global carries only access / app name (impl-client.c `global_keys`): its sec props come with the bound proxy's info
                if matches!(kind, Kind::Client)
                    && g.props.and_then(|p| p.get("application.name")) != Some("atld")
                    && let Ok(proxy) = registry.bind::<Client, _>(g)
                {
                    let id = g.id;
                    let listener = proxy
                        .add_listener_local()
                        .info({
                            let st = st.clone();
                            move |info| {
                                on_client_info(
                                    &st,
                                    id,
                                    info.props().and_then(|p| p.get("pipewire.sec.instance-id")),
                                )
                            }
                        })
                        .permissions({
                            let st = st.clone();
                            move |index, perms| {
                                let mut s = st.borrow_mut();
                                if index == 0
                                    && s.probe_client == Some(id)
                                    && let Some(d) = perms.first()
                                {
                                    s.probe_default = Some(d.permission_flags().bits());
                                }
                            }
                        })
                        .register();
                    s.clients.insert(
                        id,
                        Managed {
                            instance: None,
                            proxy,
                            applied: HashMap::new(),
                            _listener: listener,
                        },
                    );
                }
                if s.tier == Some(Tier::Enforced) {
                    let ids: Vec<u32> = s.clients.keys().copied().collect();
                    for c in ids {
                        regrant(&mut s, c);
                    }
                }
            }
        })
        .global_remove({
            let st = st.clone();
            move |id| {
                let mut s = st.borrow_mut();
                s.globals.remove(&id);
                s.clients.remove(&id);
                for m in s.clients.values_mut() {
                    m.applied.remove(&id);
                }
                if s.tier == Some(Tier::Enforced) {
                    let ids: Vec<u32> = s.clients.keys().copied().collect();
                    for c in ids {
                        regrant(&mut s, c);
                    }
                }
            }
        })
        .register();

    // initial registry contents
    sync(&mainloop, &core).map_err(|e| format!("sync: {e}"))?;
    let Some(sc_id) = *sc.borrow() else {
        return Err(format!("PipeWire offers no {SECURITY_CONTEXT_INTERFACE}"));
    };

    // tier probe: a client through a context with an access value no stock rule names
    let (probe_close, ours) = make_context(
        &registry,
        sc_id,
        &probe_path,
        &context_props(None, PROBE_INSTANCE, ACCESS_ATL),
    )
    .map_err(|e| format!("probe context: {e}"))?;
    sync(&mainloop, &core).map_err(|e| format!("sync: {e}"))?;
    drop(ours);
    let mut pp = pw::properties::PropertiesBox::new();
    pp.insert("remote.name", probe_path.to_string_lossy().as_ref());
    pp.insert("application.name", "atld-tier-probe");
    let probe_core = context
        .connect_rc(Some(pp))
        .map_err(|e| format!("probe connect: {e}"))?;
    let mut waited = Duration::ZERO;
    let mut granted = 0u32;
    while waited < PROBE_SETTLE {
        iterate(&mainloop, PROBE_POLL);
        waited += PROBE_POLL;
        let s = st.borrow();
        if let Some(c) = s.probe_client.and_then(|c| s.clients.get(&c)) {
            c.proxy.get_permissions(0, 1);
        }
        if let Some(d) = s.probe_default
            && d != 0
        {
            granted = d;
            break;
        }
    }
    sync(&mainloop, &core).map_err(|e| format!("sync: {e}"))?;
    if let Some(d) = st.borrow().probe_default {
        granted |= d;
    }
    drop(probe_core);
    drop(probe_close);
    let _ = std::fs::remove_file(&probe_path);
    let tier = if granted == 0 {
        Tier::Enforced
    } else {
        Tier::WriteRestricted
    };
    {
        let mut s = st.borrow_mut();
        s.tier = Some(tier);
        if let Some(c) = s.probe_client.take() {
            s.clients.remove(&c);
        }
    }
    let _ = ready.send(Ok((tier, granted)));

    let access = if tier == Tier::Enforced {
        ACCESS_ATL
    } else {
        ACCESS_RESTRICTED
    };
    // creates answered at their core sync's `done` (no nested loop iteration inside a loop callback)
    type Pending = (
        mpsc::Sender<io::Result<OwnedFd>>,
        OwnedFd,
        (UnixListener, OwnedFd),
    );
    let pending: Rc<RefCell<HashMap<i32, Pending>>> = Rc::default();
    let _done = core
        .add_listener_local()
        .done({
            let pending = pending.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE
                    && let Some((reply, close, ours)) = pending.borrow_mut().remove(&seq.seq())
                {
                    drop(ours);
                    let _ = reply.send(Ok(close));
                }
            }
        })
        .register();
    let _rx = rx.attach(mainloop.loop_(), {
        let (st, registry, core) = (st.clone(), registry.clone(), core.clone());
        move |cmd| match cmd {
            Cmd::Create {
                app_id,
                instance,
                path,
                grants,
                reply,
            } => {
                st.borrow_mut().apps.insert(instance.clone(), grants);
                match make_context(
                    &registry,
                    sc_id,
                    &path,
                    &context_props(app_id.as_deref(), &instance, access),
                ) {
                    Ok((close, ours)) => match core.sync(0) {
                        Ok(seq) => {
                            pending.borrow_mut().insert(seq.seq(), (reply, close, ours));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(io::Error::other(e)));
                        }
                    },
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::Grants { instance, grants } => {
                let mut s = st.borrow_mut();
                s.apps.insert(instance.clone(), grants);
                if s.tier == Some(Tier::Enforced) {
                    let ids: Vec<u32> = s
                        .clients
                        .iter()
                        .filter(|(_, m)| m.instance.as_deref() == Some(instance.as_str()))
                        .map(|(&c, _)| c)
                        .collect();
                    for c in ids {
                        regrant(&mut s, c);
                    }
                }
            }
            Cmd::Release { instance } => {
                let mut s = st.borrow_mut();
                s.apps.remove(&instance);
                s.clients
                    .retain(|_, m| m.instance.as_deref() != Some(instance.as_str()));
            }
        }
    });
    mainloop.run();
    Ok(())
}

/// Runs the loop until PipeWire answered a core sync: every request sent before it is processed.
fn sync(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) -> Result<(), pw::Error> {
    let done = Rc::new(std::cell::Cell::new(false));
    let pending = core.sync(0)?;
    let _l = core
        .add_listener_local()
        .done({
            let done = done.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                }
            }
        })
        .register();
    while !done.get() {
        iterate(mainloop, Duration::from_millis(50));
    }
    Ok(())
}

fn iterate(mainloop: &pw::main_loop::MainLoopRc, d: Duration) {
    mainloop.loop_().iterate(pw::loop_::Timeout::Finite(d));
}
