// SPDX-License-Identifier: MIT
//! The compositor of one app process: the core ([`Compositor`], [`EventThread`]) behind one lock, the binders it knows, the vsync channels, the timer frame clock thread, and the calls into libgui that the core's callbacks become.
//! Binder threads call in (a transaction arrived, a connection asked for vsync); the clock thread ticks: it applies what is due, hands the new scene to the [`SceneSink`] (the platform layer, M3 slice 6), writes the vsync events and delivers the core's calls to the clients.
//! Calls into libgui are never made with the lock held, and the end of a layer handle only queues its destruction for the next tick: libgui may release a handle from inside its own locks.

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Duration;

use atl_compositor::callbacks::{ON_RELEASE_BUFFER, ON_TRANSACTION_COMPLETED};
use atl_compositor::event::{ConnectionId, EventThread, FrameTick, VsyncEventData};
use atl_compositor::model::{ClientCall, Compositor, CompositorConfig, FrameOutput, LayerSpec};
use atl_compositor::parcel::{BinderKey, StrongBinder, WireParcel};
use atl_compositor::scene::Scene;
use atl_compositor::transaction::TransactionState;
use atl_services::display_model::DisplayModel;
use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{
    BinderFeatures, Interface, SpIBinder, StatusCode, WpIBinder, declare_binder_interface,
};
use rustix::time::{ClockId, clock_gettime};

use crate::objects::ObjectMap;
use crate::wire::canonical_binder;
use crate::{bridge, clock, listener, vsync};

/// The physical display's id (`PhysicalDisplayId`): the one display, any stable number; the EventThread's events carry it.
pub const DISPLAY_ID: u64 = 1;

/// Where the compositor's threads come from. libgui's callbacks into Java (SurfaceControl transaction listeners) find the JNIEnv of the thread they run on, as on a binder thread (which libandroid_runtime
/// creates attached to the VM): a plain thread would crash them. The launcher owns the VM, so it supplies the threads (JNI AttachCurrentThread).
pub trait ThreadHost: Send + Sync {
    /// Runs `f` on a new thread called `name`, attached to the VM.
    fn spawn(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> std::io::Result<()>;
}

/// Where a frame goes: the platform layer's presentation. Called on the clock thread, once per tick that changed anything visible.
pub trait SceneSink: Send {
    /// A new scene.
    fn present(&mut self, scene: &Scene);
}

/// Logs each scene's size to stderr, `composer: scene frame=<n> quads=<q>`: the sink until the platform layer exists.
pub struct LogSink;

impl SceneSink for LogSink {
    fn present(&mut self, scene: &Scene) {
        let _ = writeln!(
            std::io::stderr(),
            "composer: scene frame={} quads={}",
            scene.frame,
            scene.quads.len()
        );
    }
}

/// What a compositor is built with.
pub struct ComposerConfig {
    /// The display it answers with and ticks at.
    pub display: DisplayModel,
    /// The core's settings.
    pub compositor: CompositorConfig,
}

/// What happened, for the log line and the tests: the visible record that the compositor ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Frame clock ticks.
    pub ticks: u64,
    /// Transactions received.
    pub transactions: u64,
    /// Layers created.
    pub layers_created: u64,
    /// Scenes handed to the sink.
    pub scenes: u64,
    /// Vsync events written to a client.
    pub vsync_sent: u64,
    /// Vsync events dropped because the client's channel was full or gone.
    pub vsync_dropped: u64,
    /// Calls delivered to a listener.
    pub callbacks_sent: u64,
    /// Calls that could not be delivered (listener gone, call refused, no route).
    pub callbacks_failed: u64,
}

struct State {
    compositor: Compositor,
    events: EventThread,
    objects: ObjectMap<WpIBinder>,
    /// The binders clients sent that are not the compositor's own (listeners), held strongly: see [`bridge::read_parcel`]. Never released yet: a client's listener lives as long as its process.
    foreign: HashMap<u64, SpIBinder>,
    /// The file descriptors transactions carried (a layer's release channel, later buffers' fences), duplicated out of the parcel that libbinder closes, by descriptor number: what the core's `FdObject`s name.
    /// Held until [`Composer::stop`]: the core has no event for an fd it no longer refers to, so a closed layer's channel stays open (one fd per BLASTBufferQueue layer). The ownership rule belongs to the buffer path (gralloc, M3 slice 2).
    fds: HashMap<i32, Arc<OwnedFd>>,
    /// The compositor's end of each connection's channel.
    tubes: HashMap<ConnectionId, OwnedFd>,
    sink: Box<dyn SceneSink>,
    /// Calls the core produced outside a tick (a layer destroyed), delivered at the next one.
    pending_calls: Vec<ClientCall>,
    next_layer_id: i32,
    next_vsync_id: i64,
    stats: Stats,
}

impl State {
    fn wants_frame(&self) -> bool {
        self.compositor.wants_frame() || self.events.wants_frame() || !self.pending_calls.is_empty()
    }

    /// `composer: counters <key>=<value> ...`: what the gate (`xtask test composer`) reads. `ignored` lists the layer-state bits the core counted instead of applying.
    fn counters_line(&self) -> String {
        let c = self.compositor.counters();
        let s = &self.stats;
        let ignored: Vec<String> = c.ignored.iter().map(|(k, n)| format!("{k}:{n}")).collect();
        format!(
            "composer: counters ticks={} transactions={} layers_created={} layers_alive={} transactions_applied={} layer_states={} unknown_layer={} buffers_acquired={} buffers_released={} completions={} scenes={} vsync_sent={} vsync_dropped={} callbacks_sent={} callbacks_failed={} ignored={}",
            s.ticks,
            s.transactions,
            s.layers_created,
            self.compositor.layer_count(),
            c.transactions_applied,
            c.layer_states_applied,
            c.unknown_layer,
            c.buffers_acquired,
            c.buffers_released,
            c.completions_sent,
            s.scenes,
            s.vsync_sent,
            s.vsync_dropped,
            s.callbacks_sent,
            s.callbacks_failed,
            ignored.join(","),
        )
    }
}

/// The compositor of this process.
pub struct Composer {
    state: Mutex<State>,
    /// The clock thread parks here while nothing wants a frame.
    wake: Condvar,
    stop: AtomicBool,
    display: DisplayModel,
    /// The token `getPhysicalDisplayToken` hands out: any binder, named by nothing.
    display_token: SpIBinder,
}

pub trait ILayerHandle: Interface {}

declare_binder_interface! {
    ILayerHandle["android.gui.LayerHandle"] {
        native: BnLayerHandle(on_transact),
        proxy: BpLayerHandle,
    }
}

fn on_transact(
    _: &dyn ILayerHandle,
    _: TransactionCode,
    _: &BorrowedParcel<'_>,
    _: &mut BorrowedParcel<'_>,
) -> Result<(), StatusCode> {
    Err(StatusCode::UNKNOWN_TRANSACTION)
}

impl ILayerHandle for BpLayerHandle {}
impl ILayerHandle for Binder<BnLayerHandle> {}

/// A bare binder (LayerHandle in libgui is a BBinder with no interface). A layer's handle ends its layer when its last reference does: the object is destroyed, this drops.
struct Handle {
    /// The layer's key; 0 for a binder that is no layer.
    key: Arc<AtomicU64>,
    composer: Weak<Composer>,
}

impl Interface for Handle {}
impl ILayerHandle for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        let key = self.key.load(Ordering::Acquire);
        if key != 0
            && let Some(composer) = self.composer.upgrade()
        {
            composer.layer_dropped(key);
        }
    }
}

fn monotonic_ns() -> i64 {
    let t = clock_gettime(ClockId::Monotonic);
    // hosts are 64-bit (arches.toml): tv_sec and tv_nsec are i64
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

impl Composer {
    /// A compositor with no clients; [`Composer::start`] runs its clock.
    pub fn new(config: ComposerConfig, sink: Box<dyn SceneSink>) -> Arc<Self> {
        let handle = Handle {
            key: Arc::new(AtomicU64::new(0)),
            composer: Weak::new(),
        };
        let display_token =
            BnLayerHandle::new_binder(handle, BinderFeatures::default()).as_binder();
        Arc::new(Composer {
            state: Mutex::new(State {
                compositor: Compositor::new(config.compositor),
                events: EventThread::new(DISPLAY_ID),
                objects: ObjectMap::new(),
                foreign: HashMap::new(),
                fds: HashMap::new(),
                tubes: HashMap::new(),
                sink,
                pending_calls: Vec::new(),
                next_layer_id: 1,
                next_vsync_id: 0,
                stats: Stats::default(),
            }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            display: config.display,
            display_token,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // a poisoned state still holds the core's plain data: keep serving
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The display this compositor answers with.
    pub fn display(&self) -> DisplayModel {
        self.display
    }

    /// `getMaxAcquiredBufferCount`.
    pub fn max_acquired_buffer_count(&self) -> i32 {
        i32::try_from(self.lock().compositor.config().max_acquired_buffer_count).unwrap_or(i32::MAX)
    }

    /// The token of the display.
    pub fn display_token(&self) -> SpIBinder {
        self.display_token.clone()
    }

    /// What happened so far.
    pub fn stats(&self) -> Stats {
        self.lock().stats.clone()
    }

    /// The core's counters and this front-end's, as one `key=value` line (`composer: counters ...`).
    pub fn counters_line(&self) -> String {
        self.lock().counters_line()
    }

    /// A parcel a client sent, as the core reads it ([`bridge::read_parcel`]).
    pub fn read_parcel(&self, data: &BorrowedParcel<'_>) -> Result<WireParcel, StatusCode> {
        let mut st = self.lock();
        let State {
            objects,
            foreign,
            fds,
            ..
        } = &mut *st;
        bridge::read_parcel(data, objects, foreign, fds)
    }

    /// A transaction arrived (`setTransactionState`): it is applied at the next tick.
    pub fn queue_transaction(&self, t: TransactionState) {
        let mut st = self.lock();
        st.stats.transactions += 1;
        st.compositor.queue_transaction(t);
        drop(st);
        self.wake.notify_all();
    }

    /// `createSurface`: a layer with a new handle. `parent` is the handle of the parent layer (null: a root). Returns the handle and the layer id.
    pub fn create_layer(
        self: &Arc<Self>,
        name: &str,
        parent: Option<&mut SpIBinder>,
    ) -> Result<(SpIBinder, i32), StatusCode> {
        let key_cell = Arc::new(AtomicU64::new(0));
        let handle = Handle {
            key: Arc::clone(&key_cell),
            composer: Arc::downgrade(self),
        };
        let mut sp = BnLayerHandle::new_binder(handle, BinderFeatures::default()).as_binder();
        let weak = sp.downgrade();
        let mut st = self.lock();
        let key = st.objects.key_for(&weak);
        let parent = parent.map(|p| BinderKey::Local(st.objects.key_for(&p.downgrade())));
        let id = st.next_layer_id;
        st.next_layer_id += 1;
        let created = st.compositor.create_layer(LayerSpec {
            handle: canonical_binder(key),
            id,
            name: name.to_owned(),
            parent,
        });
        if created.is_err() {
            st.objects.remove(key);
            return Err(StatusCode::BAD_VALUE);
        }
        st.stats.layers_created += 1;
        // the handle's end now ends the layer
        key_cell.store(key, Ordering::Release);
        drop(st);
        Ok((sp, id))
    }

    /// The last reference to a layer's handle went away.
    fn layer_dropped(&self, key: u64) {
        let mut st = self.lock();
        let gone = st.objects.remove(key);
        let calls = st.compositor.destroy_layer(BinderKey::Local(key));
        st.pending_calls.extend(calls);
        drop(st);
        // a weak reference: dropping it takes no lock of ours
        drop(gone);
        self.wake.notify_all();
    }

    /// `createDisplayEventConnection`: a connection with vsync off, and what its client gets of the channel: the end it reads, and a duplicate of the end the compositor writes
    /// (EventThreadConnection::stealReceiveChannel hands BitTube both; the real client has no use for the second).
    pub fn create_display_event_connection(
        &self,
    ) -> std::io::Result<(ConnectionId, (OwnedFd, OwnedFd))> {
        let (ours, theirs) = vsync::channel()?;
        let send = ours.try_clone()?;
        let mut st = self.lock();
        let id = st.events.create_connection();
        st.tubes.insert(id, ours);
        Ok((id, (theirs, send)))
    }

    /// The connection's service object went away.
    pub fn remove_connection(&self, id: ConnectionId) {
        let mut st = self.lock();
        st.events.remove_connection(id);
        st.tubes.remove(&id);
    }

    /// `setVsyncRate`.
    pub fn set_vsync_rate(&self, id: ConnectionId, count: i32) {
        let mut st = self.lock();
        st.events
            .set_vsync_rate(id, u32::try_from(count).unwrap_or(0));
        drop(st);
        self.wake.notify_all();
    }

    /// `requestNextVsync`.
    pub fn request_next_vsync(&self, id: ConnectionId) {
        let mut st = self.lock();
        st.events.request_next_vsync(id);
        drop(st);
        self.wake.notify_all();
    }

    /// `getLatestVsyncEventData`: the last tick's timelines, or, before the first tick, those a tick now would have.
    pub fn latest_vsync_event_data(&self) -> VsyncEventData {
        let st = self.lock();
        if let Some(data) = st.events.latest_vsync_event_data() {
            return data.clone();
        }
        let mut probe = EventThread::new(DISPLAY_ID);
        probe.on_frame_tick(FrameTick {
            vsync_id: st.next_vsync_id + 1,
            timestamp: monotonic_ns(),
            interval: clock::interval_ns(self.display.refresh_hz),
        });
        probe
            .latest_vsync_event_data()
            .cloned()
            .unwrap_or_else(|| VsyncEventData {
                frame_interval: clock::interval_ns(self.display.refresh_hz),
                preferred_frame_timeline_index: 0,
                frame_timelines: Vec::new(),
                number_queued_buffers: 0,
            })
    }

    /// The binder behind `key`, if it is alive.
    fn resolve(&self, key: u64) -> Option<SpIBinder> {
        let st = self.lock();
        match st.foreign.get(&key) {
            Some(binder) => Some(binder.clone()),
            None => st.objects.get(key)?.promote(),
        }
    }

    /// Runs the frame clock on a thread of `threads` until [`Composer::stop`]. The thread delivers the core's calls into libgui, whose SurfaceControl callbacks call into Java:
    /// it must be one the VM knows ([`ThreadHost`]).
    pub fn start(self: &Arc<Self>, threads: &dyn ThreadHost) -> std::io::Result<()> {
        let me = Arc::clone(self);
        threads.spawn("atl-frame-clock", Box::new(move || me.run()))
    }

    /// Ends the clock thread and closes the descriptors held.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let held = std::mem::take(&mut self.lock().fds);
        drop(held);
        self.wake.notify_all();
    }

    fn run(&self) {
        let interval = clock::interval_ns(self.display.refresh_hz);
        let origin = monotonic_ns();
        let mut parked = true;
        loop {
            {
                let mut st = self.lock();
                while !self.stop.load(Ordering::Acquire) && !st.wants_frame() {
                    if !parked {
                        parked = true;
                        let _ = writeln!(std::io::stderr(), "{}", st.counters_line());
                    }
                    st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                if self.stop.load(Ordering::Acquire) {
                    return;
                }
            }
            parked = false;
            let now = monotonic_ns();
            let at = clock::next_tick(origin, interval, now);
            std::thread::sleep(Duration::from_nanos(u64::try_from(at - now).unwrap_or(0)));
            self.tick(at, interval);
        }
    }

    /// One frame clock tick at `at_ns`: the core applies what is due, vsync goes out, the scene goes to the sink, the calls go to the clients.
    fn tick(&self, at_ns: i64, interval_ns: i64) {
        let calls = {
            let mut st = self.lock();
            st.stats.ticks += 1;
            let FrameOutput { scene, calls } = st.compositor.on_frame(at_ns);
            st.next_vsync_id += 1;
            let tick = FrameTick {
                vsync_id: st.next_vsync_id,
                timestamp: at_ns,
                interval: interval_ns,
            };
            for (id, event) in st.events.on_frame_tick(tick) {
                let sent = match st.tubes.get(&id) {
                    Some(tube) => vsync::send_event(tube, &event).is_ok(),
                    None => false,
                };
                if sent {
                    st.stats.vsync_sent += 1;
                } else {
                    st.stats.vsync_dropped += 1;
                }
            }
            if let Some(scene) = scene {
                st.stats.scenes += 1;
                st.sink.present(&scene);
            }
            let mut all = std::mem::take(&mut st.pending_calls);
            all.extend(calls);
            all
        };
        for call in calls {
            let delivered = self.deliver(&call);
            if let Err(why) = &delivered {
                let _ = writeln!(std::io::stderr(), "composer: callback failed: {why}");
            }
            let mut st = self.lock();
            if delivered.is_ok() {
                st.stats.callbacks_sent += 1;
            } else {
                st.stats.callbacks_failed += 1;
            }
        }
    }

    /// One call of the core into a client: `Ok` when it arrived, else why not.
    fn deliver(&self, call: &ClientCall) -> Result<(), String> {
        let (listener, code, wire) =
            match call {
                ClientCall::TransactionCompleted { listener, stats } => {
                    (listener, ON_TRANSACTION_COMPLETED, stats.encode_call())
                }
                ClientCall::ReleaseBuffer { listener, release } => {
                    (listener, ON_RELEASE_BUFFER, release.encode_call())
                }
                // the layer's BufferReleaseChannel: one datagram on the producer's socket (BBQ reads it on its own thread)
                ClientCall::ReleaseOnChannel { channel, release } => {
                    let (datagram, fence) = release.channel_datagram();
                    if fence.is_some() {
                        return Err("a release fence: fences come with buffers".to_owned());
                    }
                    let socket =
                        self.lock().fds.get(&channel.fd).cloned().ok_or_else(|| {
                            format!("release channel fd {} is not held", channel.fd)
                        })?;
                    return vsync::send_datagram(&*socket, &datagram)
                        .map_err(|e| format!("release channel fd {}: {e}", channel.fd));
                }
            };
        let wire = wire.map_err(|e| format!("encoding the call: {e}"))?;
        let binder = self
            .listener_binder(listener)
            .ok_or_else(|| format!("listener {listener:?} is gone"))?;
        listener::call(binder, code, &wire, &|key| self.resolve(key))
            .map_err(|e| format!("transaction {code} to the listener: {e:?}"))
    }

    fn listener_binder(&self, listener: &StrongBinder) -> Option<SpIBinder> {
        match listener.key()? {
            BinderKey::Local(key) => self.resolve(key),
            BinderKey::Handle(_) => None,
        }
    }
}
