// SPDX-License-Identifier: MIT
//! The compositor of one app process: the core ([`Compositor`], [`EventThread`]) behind one lock, the binders it knows, the vsync channels, the timer frame clock thread, and the calls into libgui that the core's callbacks become.
//! Binder threads call in (a transaction arrived, a connection asked for vsync); the clock thread ticks: it applies what is due and writes the vsync events; the present thread then hands the new scene to the [`SceneSink`] (the platform layer, M3 slice 6) and delivers the core's calls to the clients.
//! Calls into libgui are never made with the lock held, and the end of a layer handle only queues its destruction for the next tick: libgui may release a handle from inside its own locks.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use atl_compositor::callbacks::{ON_RELEASE_BUFFER, ON_TRANSACTION_COMPLETED};
use atl_compositor::event::{ConnectionId, EventThread, FrameTick, VsyncEventData};
use atl_compositor::model::{ClientCall, Compositor, CompositorConfig, FrameOutput, LayerSpec};
use atl_compositor::scene::{Quad, Scene};
use atl_libgui_parcel::parcel::WireParcel;
use atl_libgui_parcel::parcel::{BinderKey, StrongBinder};
use atl_libgui_parcel::transaction::{Transaction, TransactionState};
use atl_libgui_parcel::wire::set_binder_key;
use atl_parcelables::android::view::SurfaceControl_Transaction;
use atl_parcelables::codecs::android::view::SurfaceControl_Transaction::TransactionObject;
use atl_services::display_model::DisplayModel;
use binder::binder_impl::{Binder, BorrowedParcel, TransactionCode};
use binder::{
    BinderFeatures, Interface, SpIBinder, StatusCode, WpIBinder, declare_binder_interface,
};
use rustix::time::{ClockId, clock_gettime};

use crate::objects::ObjectMap;
use crate::sink::SceneSink;
use crate::{bridge, clock, listener, vsync};
use atl_libgui_parcel::wire::canonical_binder;

/// The physical display's id (`PhysicalDisplayId`): the one display, any stable number; the EventThread's events carry it.
pub const DISPLAY_ID: u64 = 1;

/// Where the compositor's threads come from. libgui's callbacks into Java (SurfaceControl transaction listeners) find the JNIEnv of the thread they run on, as on a binder thread (which libandroid_runtime
/// creates attached to the VM): a plain thread would crash them. The launcher owns the VM, so it supplies the threads (JNI AttachCurrentThread).
pub trait ThreadHost: Send + Sync {
    /// Runs `f` on a new thread called `name`, attached to the VM.
    fn spawn(&self, name: &str, f: Box<dyn FnOnce() + Send>) -> std::io::Result<()>;
}

/// Ticks' output queued for the present thread: one sink read longer than a period is absorbed; a sink slower than the clock for longer than this holds the clock back, so the queue cannot grow without bound.
const FRAMES_QUEUED: usize = 2;

/// What one tick hands the present thread.
struct Frame {
    scene: Option<Scene>,
    calls: Vec<ClientCall>,
}

impl Frame {
    /// The descriptors the present thread will use: every quad's buffer and every release channel.
    fn fds(&self) -> Vec<i32> {
        let buffers = self.scene.iter().flat_map(|s| &s.quads);
        let channels = self.calls.iter().filter_map(|c| match c {
            ClientCall::ReleaseOnChannel { channel, .. } => Some(channel.fd),
            _ => None,
        });
        buffers
            .filter_map(Quad::buffer)
            .flat_map(|b| b.buffer.fds.iter().map(|f| f.raw()))
            .chain(channels)
            .collect()
    }
}

/// UI mode's frame path (no clock thread: the window's frame clock ticks, owner decision 2026-11-05). [`UiFrames::tick`] runs on the UI thread; the scene is shown there, and the calls go to the deliver thread.
pub struct UiFrames {
    composer: Arc<Composer>,
    frames: SyncSender<Frame>,
}

impl UiFrames {
    /// The window's monitor as the display ([]); true when it changed.
    pub fn set_display(&self, display: DisplayModel) -> bool {
        self.composer.set_display(display)
    }

    /// The window clock is on demand, as SurfaceFlinger's EventThread is for an app (it sends vsync only to a connection that asked, `requestNextVsync` / `setVsyncRate`; Choreographer asks per frame, `scheduleVsyncLocked`):
    /// `request` is called whenever a client asks for vsync or sends work (a transaction, a destroyed layer), from any thread, and the UI thread then asks its clock for a tick. Without it nothing ticks.
    pub fn set_frame_requester(&self, request: Box<dyn Fn() + Send + Sync>) {
        if self.composer.ui_wake.set(request).is_err() {
            tracing::warn!("atl-composer: ui: a second frame requester refused");
        }
    }

    /// Whether the next tick has anything to do: a connection waits for vsync, a transaction or buffer is queued, or calls wait to be delivered. False: the clock stays stopped until [`UiFrames::set_frame_requester`]'s callback runs.
    pub fn wants_frame(&self) -> bool {
        self.composer.lock().wants_frame()
    }

    /// One frame at `time_ns` (CLOCK_MONOTONIC, the window clock's timestamp): the core applies what is due, vsync goes out, `present` shows the scene on this thread, then the calls are queued for the deliver thread (a release follows the scene that shows its buffer).
    pub fn tick(&self, time_ns: i64, present: impl FnOnce(&Scene)) {
        let interval_ns = clock::interval_ns(self.composer.display().refresh_hz);
        let frame = self.composer.tick(time_ns, interval_ns);
        if let Some(scene) = &frame.scene {
            present(scene);
        }
        // an error means the deliver thread is gone: the process is ending
        let _ = self.frames.send(Frame {
            scene: None,
            calls: frame.calls,
        });
    }
}

/// What a compositor is built with.
pub struct ComposerConfig {
    /// The display it answers with and ticks at.
    pub display: DisplayModel,
    /// The core's settings.
    pub compositor: CompositorConfig,
    /// Post-draw Transactions the window service forwards are applied. false = a gate's deliberate break only (`xtask test window --break-post-draw`): they are counted and dropped.
    pub post_draw: bool,
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
    /// Buffer releases written to a layer's BufferReleaseChannel socket.
    pub releases_on_channel: u64,
    /// Releases whose channel had no reader any more (counted apart: nobody was waiting).
    pub releases_peer_gone: u64,
    /// File descriptors taken out of transaction parcels.
    pub fds_taken: u64,
    /// File descriptors closed because the core named them no longer.
    pub fds_closed: u64,
    /// Post-draw Transactions forwarded by the window service (finishDrawing), and of those the ones queued for the core.
    pub post_draw_received: u64,
    pub post_draw_applied: u64,
}

struct State {
    compositor: Compositor,
    events: EventThread,
    objects: ObjectMap<WpIBinder>,
    /// The binders clients sent that are not the compositor's own (listeners), held strongly: see [`bridge::read_parcel`]. Never released yet: a client's listener lives as long as its process.
    foreign: HashMap<u64, SpIBinder>,
    /// The file descriptors transactions carried (a layer's release channel, buffers' dma-bufs and fences), duplicated out of the parcel that libbinder closes, by descriptor number: what the core's `FdObject`s name.
    /// The ownership rule: a descriptor lives while the core names it ([`Compositor::referenced_fds`], plus the channels of the calls not yet delivered). [`State::close_unreferenced`] closes the rest, after each tick's deliveries;
    /// taking the descriptors, decoding and queueing the transaction happen under one hold of the lock ([`Composer::set_transaction_state`]), so no descriptor is unreferenced for a moment between.
    fds: HashMap<i32, Arc<OwnedFd>>,
    /// The compositor's end of each connection's channel.
    tubes: HashMap<ConnectionId, OwnedFd>,
    /// The descriptors of each tick's output the present thread has not finished with, oldest first: the scene's buffers and the release channels of its calls, which the core may stop naming while they wait.
    in_flight: VecDeque<Vec<i32>>,
    /// Calls the core produced outside a tick (a layer destroyed), delivered at the next one.
    pending_calls: Vec<ClientCall>,
    next_layer_id: i32,
    next_vsync_id: i64,
    /// The last counters line logged, without its `ticks=` field (see `deliver_frame`).
    counters_logged: String,
    stats: Stats,
}

impl State {
    fn wants_frame(&self) -> bool {
        self.compositor.wants_frame() || self.events.wants_frame() || !self.pending_calls.is_empty()
    }

    /// Closes the descriptors the core no longer names: not in a layer, the client cache or a queued transaction, and not the channel of a call still to deliver.
    fn close_unreferenced(&mut self) {
        let mut keep = self.compositor.referenced_fds();
        keep.extend(self.pending_calls.iter().filter_map(|c| match c {
            ClientCall::ReleaseOnChannel { channel, .. } => Some(channel.fd),
            _ => None,
        }));
        keep.extend(self.in_flight.iter().flatten().copied());
        let before = self.fds.len();
        self.fds.retain(|number, _| keep.contains(number));
        self.stats.fds_closed += (before - self.fds.len()) as u64;
    }

    /// `composer: counters <key>=<value> ...`: what the gate (`xtask test composer`) reads. `ignored` lists the layer-state bits the core counted instead of applying.
    fn counters_line(&self) -> String {
        let c = self.compositor.counters();
        let s = &self.stats;
        let ignored: Vec<String> = c.ignored.iter().map(|(k, n)| format!("{k}:{n}")).collect();
        format!(
            "composer: counters ticks={} transactions={} layers_created={} layers_alive={} transactions_applied={} layer_states={} unknown_layer={} buffers_acquired={} buffers_released={} completions={} scenes={} vsync_sent={} vsync_dropped={} callbacks_sent={} callbacks_failed={} releases_on_channel={} releases_peer_gone={} fds_held={} fds_taken={} fds_closed={} post_draw_received={} post_draw_applied={} ignored={}",
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
            s.releases_on_channel,
            s.releases_peer_gone,
            self.fds.len(),
            s.fds_taken,
            s.fds_closed,
            s.post_draw_received,
            s.post_draw_applied,
            ignored.join(","),
        )
    }
}

/// The compositor of this process.
pub struct Composer {
    state: Mutex<State>,
    /// The clock thread parks here while nothing wants a frame.
    wake: Condvar,
    /// UI mode's clock restart (UiFrames::set_frame_requester): what [`Composer::notify`] calls besides `wake`.
    ui_wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
    stop: AtomicBool,
    /// the monitor the window is on (UI mode follows it: []); headless keeps the one it starts with
    display: Mutex<DisplayModel>,
    /// The token `getPhysicalDisplayToken` hands out: any binder, named by nothing.
    display_token: SpIBinder,
    /// ComposerConfig::post_draw.
    post_draw: bool,
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
    pub fn new(config: ComposerConfig) -> Arc<Self> {
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
                in_flight: VecDeque::new(),
                pending_calls: Vec::new(),
                next_layer_id: 1,
                next_vsync_id: 0,
                counters_logged: String::new(),
                stats: Stats::default(),
            }),
            wake: Condvar::new(),
            ui_wake: OnceLock::new(),
            stop: AtomicBool::new(false),
            display: Mutex::new(config.display),
            post_draw: config.post_draw,
            display_token,
        })
    }

    /// Something may want a frame now: wakes the clock thread, or asks UI mode's window clock for a tick. Called with the state lock released.
    fn notify(&self) {
        self.wake.notify_all();
        if let Some(request) = self.ui_wake.get() {
            request();
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // a poisoned state still holds the core's plain data: keep serving
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The display this compositor answers with now.
    pub fn display(&self) -> DisplayModel {
        *self.display.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The window moved to another monitor, or its monitor changed (UI mode): the display answered from now on and the frame interval the next ticks carry. True when it changed.
    pub fn set_display(&self, display: DisplayModel) -> bool {
        let mut current = self.display.lock().unwrap_or_else(|e| e.into_inner());
        if *current == display {
            return false;
        }
        *current = display;
        true
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

    /// `setTransactionState`: the parcel a client sent, as the core reads it ([`bridge::read_parcel`], `TransactionState::decode_call`), queued for the next tick. All under one hold of the lock: the descriptors
    /// the parcel carried are the core's to name from the moment they are taken, and a parcel that does not decode gives them back at once.
    pub fn set_transaction_state(&self, data: &BorrowedParcel<'_>) -> Result<(), StatusCode> {
        let mut st = self.lock();
        let State {
            objects,
            foreign,
            fds,
            ..
        } = &mut *st;
        let held = fds.len();
        let wire = bridge::read_parcel(data, objects, foreign, fds);
        st.stats.fds_taken += (st.fds.len() - held) as u64;
        let decoded = wire.and_then(|w| {
            TransactionState::decode_call(&w).map_err(|e| {
                tracing::warn!("composer: setTransactionState refused: {e}");
                StatusCode::BAD_VALUE
            })
        });
        match decoded {
            Ok(t) => {
                st.stats.transactions += 1;
                st.compositor.queue_transaction(t);
                drop(st);
                self.notify();
                Ok(())
            }
            Err(e) => {
                st.close_unreferenced();
                Err(e)
            }
        }
    }

    /// A post-draw Transaction the window service forwards from the app's `IWindowSession.finishDrawing`, as libgui parcelled it (bytes + their objects), queued for the next tick as
    /// `setTransactionState` would queue it. AOSP: WMS merges it into the window's sync / pending transaction (WindowStateAnimator.finishDrawingLocked:251-252) and the next traversal
    /// applies that to SurfaceFlinger, whose `setTransactionState` this joins; `Transaction::into_state` is what that `apply` sends. Counted received and applied (dropped under the gate's break).
    pub fn apply_post_draw(&self, t: SurfaceControl_Transaction) -> Result<(), StatusCode> {
        let mut st = self.lock();
        st.stats.post_draw_received += 1;
        if !self.post_draw {
            return Ok(());
        }
        let State {
            objects,
            foreign,
            fds,
            ..
        } = &mut *st;
        let held = fds.len();
        let mut bytes = t.data;
        let mut offsets = Vec::with_capacity(t.objects.len());
        for (offset, object) in t.objects {
            match object {
                TransactionObject::Binder(Some(b)) => {
                    set_binder_key(&mut bytes, offset, bridge::binder_key(b, objects, foreign));
                }
                // a null binder is no object in libbinder's table: the framer never lists one
                TransactionObject::Binder(None) => return Err(StatusCode::BAD_VALUE),
                TransactionObject::Fd(fd) => bridge::take_fd(&mut bytes, offset, fd, fds),
            }
            offsets.push(offset);
        }
        st.stats.fds_taken += (st.fds.len() - held) as u64;
        let decoded = WireParcel::from_wire(bytes, offsets)
            .and_then(|w| {
                let mut r = w.reader();
                let t = Transaction::read(&mut r)?;
                r.finish()?;
                Ok(t)
            })
            .map_err(|e| {
                tracing::warn!("composer: post-draw Transaction refused: {e}");
                StatusCode::BAD_VALUE
            });
        match decoded {
            Ok(t) => {
                st.stats.post_draw_applied += 1;
                st.compositor.queue_transaction(t.into_state());
                drop(st);
                self.notify();
                Ok(())
            }
            Err(e) => {
                st.close_unreferenced();
                Err(e)
            }
        }
    }

    /// `createSurface`: a layer with a new handle. `parent` is the handle of the parent layer (null: a root); `fills_color` as [`LayerSpec::fills_color`]. Returns the handle and the layer id.
    pub fn create_layer(
        self: &Arc<Self>,
        name: &str,
        parent: Option<&mut SpIBinder>,
        fills_color: bool,
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
            fills_color,
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

    /// Places layer `id` at (`x`, `y`) in its parent: the window service's frame origin for a window (ui_agent WINDOW_POSITION). BAD_VALUE for an unknown layer.
    pub fn set_layer_position(&self, id: i32, x: f32, y: f32) -> Result<(), StatusCode> {
        if !self.lock().compositor.set_position(id, x, y) {
            return Err(StatusCode::BAD_VALUE);
        }
        self.notify();
        Ok(())
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
        self.notify();
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
        self.notify();
    }

    /// `requestNextVsync`.
    pub fn request_next_vsync(&self, id: ConnectionId) {
        let mut st = self.lock();
        st.events.request_next_vsync(id);
        drop(st);
        self.notify();
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
            interval: clock::interval_ns(self.display().refresh_hz),
        });
        probe
            .latest_vsync_event_data()
            .cloned()
            .unwrap_or_else(|| VsyncEventData {
                frame_interval: clock::interval_ns(self.display().refresh_hz),
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

    /// Runs the frame clock and the present thread on threads of `threads` until [`Composer::stop`]. The present thread delivers the core's calls into libgui, whose SurfaceControl callbacks call into Java:
    /// it must be one the VM knows ([`ThreadHost`]).
    pub fn start(
        self: &Arc<Self>,
        threads: &dyn ThreadHost,
        sink: Box<dyn SceneSink>,
    ) -> std::io::Result<()> {
        let (frames, queue) = sync_channel(FRAMES_QUEUED);
        let me = Arc::clone(self);
        threads.spawn(
            "atl-frame-present",
            Box::new(move || me.present_frames(sink, queue)),
        )?;
        let me = Arc::clone(self);
        threads.spawn("atl-frame-clock", Box::new(move || me.run(frames)))
    }

    /// UI mode: one deliver thread from `threads` writes each frame's calls; the frame clock is the window's ([`UiFrames`]), so no clock thread runs. Called once, on the UI thread's startup path.
    pub fn start_ui(self: &Arc<Self>, threads: &dyn ThreadHost) -> std::io::Result<UiFrames> {
        let (frames, queue) = sync_channel(FRAMES_QUEUED);
        let me = Arc::clone(self);
        threads.spawn(
            "atl-frame-deliver",
            Box::new(move || me.deliver_frames(queue)),
        )?;
        Ok(UiFrames {
            composer: Arc::clone(self),
            frames,
        })
    }

    /// Ends the clock thread and closes the descriptors held.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let held = std::mem::take(&mut self.lock().fds);
        drop(held);
        self.notify();
    }

    fn run(&self, frames: SyncSender<Frame>) {
        let interval = clock::interval_ns(self.display().refresh_hz);
        let origin = monotonic_ns();
        loop {
            {
                let mut st = self.lock();
                while !self.stop.load(Ordering::Acquire) && !st.wants_frame() {
                    st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                if self.stop.load(Ordering::Acquire) {
                    return;
                }
            }
            let now = monotonic_ns();
            let at = clock::next_tick(origin, interval, now);
            std::thread::sleep(Duration::from_nanos(u64::try_from(at - now).unwrap_or(0)));
            let frame = self.tick(at, interval);
            // an error means the present thread is gone: the process is ending
            let _ = frames.send(frame);
        }
    }

    /// One frame clock tick at `at_ns`: the core applies what is due, vsync goes out, and the frame is returned for its thread to show and deliver. Nothing here waits on pixels: a tick that did would miss the next grid point, and every client's vsync with it.
    fn tick(&self, at_ns: i64, interval_ns: i64) -> Frame {
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
        if scene.is_some() {
            st.stats.scenes += 1;
        }
        let mut all = std::mem::take(&mut st.pending_calls);
        all.extend(calls);
        let frame = Frame { scene, calls: all };
        st.in_flight.push_back(frame.fds());
        frame
    }

    /// The present thread: each tick's output in order, the scene to the sink and then the calls to the clients. The order is what lets a sink read a buffer here: the release of a buffer comes in a later tick's calls than the scene that shows it, so the producer cannot rewrite it during the read.
    fn present_frames(&self, mut sink: Box<dyn SceneSink>, frames: Receiver<Frame>) {
        for frame in frames {
            if let Some(scene) = &frame.scene {
                sink.present(scene);
            }
            self.deliver_frame(frame.calls);
        }
    }

    /// UI mode's deliver thread: each frame's calls, after the UI thread showed its scene.
    fn deliver_frames(&self, frames: Receiver<Frame>) {
        for frame in frames {
            self.deliver_frame(frame.calls);
        }
    }

    /// One frame's calls into the clients, then the bookkeeping that frees its descriptors.
    fn deliver_frame(&self, calls: Vec<ClientCall>) {
        self.deliver_all(calls);
        let mut st = self.lock();
        st.in_flight.pop_front();
        // the calls are delivered (a release channel is written): what the core names no longer can be closed
        st.close_unreferenced();
        // the clock is about to park: nothing is left to count, so this is the line the gate reads
        // an idle clock parks on every tick and moves only `ticks`: log the line when anything else moved, so the last one is the state after the last work
        if st.in_flight.is_empty() && !st.wants_frame() {
            let line = st.counters_line();
            let key = line.replacen(&format!("ticks={} ", st.stats.ticks), "", 1);
            if key != st.counters_logged {
                st.counters_logged = key;
                tracing::info!("{line}");
            }
        }
    }

    fn deliver_all(&self, calls: Vec<ClientCall>) {
        for call in calls {
            let delivered = self.deliver(&call);
            if let Err(why) = &delivered {
                tracing::warn!("composer: callback failed: {why}");
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
        let (listener, code, wire) = match call {
            ClientCall::TransactionCompleted { listener, stats } => {
                (listener, ON_TRANSACTION_COMPLETED, stats.encode_call())
            }
            ClientCall::ReleaseBuffer { listener, release } => {
                (listener, ON_RELEASE_BUFFER, release.encode_call())
            }
            // the layer's BufferReleaseChannel: one datagram on the producer's socket (BBQ reads it on its own thread)
            ClientCall::ReleaseOnChannel { channel, release } => {
                let (datagram, fence) = release.channel_datagram();
                let held = |fd: i32, what: &str| {
                    self.lock()
                        .fds
                        .get(&fd)
                        .cloned()
                        .ok_or_else(|| format!("{what} fd {fd} is not held"))
                };
                let socket = held(channel.fd, "release channel")?;
                let fence = fence.map(|f| held(f.fd, "release fence")).transpose()?;
                match vsync::send_datagram_with_fd(
                    &*socket,
                    &datagram,
                    fence.as_deref().map(|f| f.as_fd()),
                ) {
                    Ok(()) => self.lock().stats.releases_on_channel += 1,
                    // the queue that held the other end is gone (its BBQ was destroyed, or its process closed it): nobody is waiting for the release
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::BrokenPipe
                                | std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::NotConnected
                        ) =>
                    {
                        self.lock().stats.releases_peer_gone += 1
                    }
                    Err(e) => return Err(format!("release channel fd {}: {e}", channel.fd)),
                }
                return Ok(());
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
