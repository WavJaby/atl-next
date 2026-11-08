// SPDX-License-Identifier: MIT
//! The compositor core: layer tree keyed by layer handle, the client buffer cache, per-apply-token transaction queues, buffer hand-off and the callbacks that go with it.
//! Message driven and synchronous: the front-end feeds decoded [`TransactionState`]s and layer create/destroy, calls [`Compositor::on_frame`] on each frame tick of the platform's clock, and delivers the [`ClientCall`]s
//! and the [`Scene`] that come back.
//!
//! What SurfaceFlinger does that matters to clients, and what is modelled here (Android 16 semantics read from libgui's BLASTBufferQueue / TransactionCompletedListener):
//! * a transaction is applied at a frame tick, queued per apply token in order; with `eEnableBackpressure` a layer takes one new buffer per tick, the next transaction waits (BBQ sets the flag on its layer);
//! * a buffer that stops being shown (replaced, dropped in the same tick, layer destroyed, cache miss) is released exactly once, through the layer's `BufferReleaseChannel` when it has one, else `onReleaseBuffer`
//!   on the buffer's release listener. BBQ stalls at `mNumAcquired >= mMaxAcquiredBuffers + 2` (`BLASTBufferQueue.cpp:564`) if releases never come, so none is ever skipped silently (an unroutable one is counted);
//! * transaction callbacks (commit and complete ids) are answered after the tick that applied the transaction, in one `onTransactionCompleted` per listener with `SurfaceStats` for each layer that registered the
//!   callback, `eventStats.frameNumber` = the latched frame number (BBQ's `transactionCallback` releases every submitted buffer older than it).
//!
//! Fields of `layer_state_t` the software path does not need are decoded and counted by name in [`Counters::ignored`], never dropped without a trace.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use crate::callbacks::{
    AcquireTimeOrFence, FrameEventHistoryStats, ListenerStats, NO_RELEASE_CALLBACK, ReleaseBuffer,
    ReleaseCallbackId, SurfaceStats, TransactionStats,
};
use crate::scene::{BufferContent, Content, Quad, Scene, SceneBuffer, SceneFd};
use atl_libgui_parcel::parcel::{BinderKey, FdObject, StrongBinder};
use atl_libgui_parcel::state::{
    BufferData, CallbackId, Fence, FloatRect, GraphicBuffer, LayerState, ListenerCallbacks, Rect,
    buffer_data_change, layer_flags, what,
};
use atl_libgui_parcel::transaction::TransactionState;

/// A layer colour component meaning "no fill": RequestedLayerState sets r, g, b to -1 for `eNoColorFill`, and `fillsColor` needs all three >= 0.
const NO_COLOR_FILL: f32 = -1.0;

/// `ui::Transform::ROT_90`, the one buffer-transform bit that swaps width and height.
const BUFFER_TRANSFORM_ROT_90: u32 = 4;

/// Static settings of the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompositorConfig {
    /// What `getMaxAcquiredBufferCount` answers and every release / `SurfaceStats` reports: how many buffers a BBQ producer may hold acquired.
    /// SurfaceFlinger derives it from its pipeline depth (`sf.max_acquired_buffers`, 2 by default); one tick latches one buffer and releases the previous, so 2 keeps a producer one buffer ahead of the screen.
    pub max_acquired_buffer_count: u32,
    /// The layer stack the scene shows.
    pub layer_stack: u32,
    /// Siblings are ordered by their z. false = a gate's deliberate break only (`xtask test window --stage surface-view --break-z-order`): creation order alone, so SurfaceViews leave their z side of the window and the readback check must fail.
    pub z_order: bool,
    /// Queued buffers are latched. false = a gate's deliberate break only (`xtask test window --stage hwui --break-queue`): every buffer a transaction carries is dropped unlatched, as if the producer never queued it, so no frame reaches the scene and the readback check must fail.
    pub latch_buffers: bool,
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            max_acquired_buffer_count: 2,
            layer_stack: 0,
            z_order: true,
            latch_buffers: true,
        }
    }
}

/// A layer the client created (`ISurfaceComposerClient.createSurface`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerSpec {
    /// The layer handle binder: the key of every later transaction on it.
    pub handle: StrongBinder,
    /// The layer id the client was given.
    pub id: i32,
    /// The layer name.
    pub name: String,
    /// The parent layer's handle; `None` for a root.
    pub parent: Option<BinderKey>,
    /// The layer fills its bounds with its colour while it has no buffer: created as an effect (colour) layer, `createSurface` flags `eFXSurfaceEffect` without `eNoColorFill`
    /// (SurfaceFlinger::createLayer gives every other kind `eNoColorFill`, RequestedLayerState then sets colour -1 = no fill). Its colour starts black.
    pub fills_color: bool,
}

/// A colour layer's state as the window manager sets it ([`Compositor::set_color_layer`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorLayer {
    /// The id of the layer this one is ordered relative to.
    pub relative_to: i32,
    /// Its z relative to that layer: negative = below it.
    pub z: i32,
    /// r, g, b in 0..=1.
    pub color: [f32; 3],
    pub alpha: f32,
    /// The crop from the layer's origin: width, height.
    pub size: (f32, f32),
}

/// Why the model refused a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// A layer handle that is null.
    NullHandle,
    /// A layer with this handle exists.
    DuplicateLayer(BinderKey),
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NullHandle => write!(f, "layer handle is null"),
            Self::DuplicateLayer(k) => write!(f, "layer {k:?} exists"),
        }
    }
}

impl std::error::Error for ModelError {}

/// A call the front-end must make into the client, produced by the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientCall {
    /// `ITransactionCompletedListener.onTransactionCompleted(stats)` on `listener`.
    TransactionCompleted {
        /// The listener the callbacks were registered with.
        listener: StrongBinder,
        /// The outcomes.
        stats: ListenerStats,
    },
    /// `ITransactionCompletedListener.onReleaseBuffer` on `listener`: the layer has no release channel.
    ReleaseBuffer {
        /// The buffer's release listener.
        listener: StrongBinder,
        /// The release.
        release: ReleaseBuffer,
    },
    /// A `BufferReleaseChannel` message: write `release.channel_datagram()` to the producer endpoint `channel` (the fence fd rides as SCM_RIGHTS).
    ReleaseOnChannel {
        /// The producer endpoint the layer registered.
        channel: FdObject,
        /// The release.
        release: ReleaseBuffer,
    },
}

/// Everything the model counted instead of doing: the visible record of what is not implemented or not routable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Transactions applied.
    pub transactions_applied: u64,
    /// Layer states applied.
    pub layer_states_applied: u64,
    /// `layer_state_t::what` bits set that the software path ignores, by AOSP name, and how often.
    pub ignored: BTreeMap<&'static str, u64>,
    /// Layer states naming a layer that does not exist.
    pub unknown_layer: u64,
    /// A cached-buffer id with no buffer behind it (the buffer is released at once).
    pub cache_misses: u64,
    /// `eMatrixChanged` with rotation or shear (only scale is applied).
    pub matrix_rotation_ignored: u64,
    /// Scenes that skipped a colour layer with no crop on it or any ancestor: SurfaceFlinger bounds it by the display, which this model does not know.
    pub unbounded_color_skipped: u64,
    /// Transactions with a desired present time (applied at the next tick regardless).
    pub desired_present_time_ignored: u64,
    /// Display states (one display, one layer stack: not applied).
    pub display_states_ignored: u64,
    /// Input window commands (focus requests, window-info listeners: not applied).
    pub input_commands_ignored: u64,
    /// Buffers released with neither a release channel nor a listener to tell.
    pub release_unroutable: u64,
    /// Callback registrations with a null listener.
    pub null_listener: u64,
    /// Buffers released (any route).
    pub buffers_released: u64,
    /// Buffers released through their replacing transaction's completion (`SurfaceStats.previous_release_callback_id`); the channel write, when the layer has one, goes with it.
    pub releases_carried: u64,
    /// Buffers acquired (accepted into a layer, or dropped on a cache miss).
    pub buffers_acquired: u64,
    /// Transaction-completed stats sent.
    pub completions_sent: u64,
    /// Ticks a transaction queue waited at its front because a buffer's acquire fence had not signalled.
    pub fence_waits: u64,
    /// Buffers the presenter still held when the core replaced or destroyed them: released at its paint, not at once.
    pub releases_held: u64,
    /// Completions held back at creation, until the presenter's paint released the older buffers they name.
    pub completions_held: u64,
}

impl Counters {
    fn ignore(&mut self, name: &'static str) {
        *self.ignored.entry(name).or_insert(0) += 1;
    }
}

/// What one frame tick produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameOutput {
    /// The new scene when anything visible changed since the last tick.
    pub scene: Option<Scene>,
    /// Calls to make into clients, in order.
    pub calls: Vec<ClientCall>,
}

/// The buffer a layer shows, and how to tell its producer when it is done with it.
#[derive(Clone, Debug)]
struct Held {
    buffer: Arc<SceneBuffer>,
    frame_number: u64,
    acquire_fence: Option<SceneFd>,
    release_listener: StrongBinder,
    /// `releaseBufferEndpoint`: the listener whose completion carries this buffer's release (AOSP `mPreviousReleaseBufferEndpoint`).
    release_endpoint: StrongBinder,
    /// The presenter holds this buffer ([`Compositor::set_held`]). Replacing it, the core waits for the presenter's paint ([`Compositor::painted`]).
    presented: bool,
}

/// A buffer the presenter may still draw after the core replaced it or destroyed its layer: released at the presenter's next paint.
#[derive(Debug)]
struct Retired {
    /// The layer it was on; that layer's completions wait for it.
    layer: BinderKey,
    held: Held,
    /// The layer's release channel when it was replaced.
    channel: Option<FdObject>,
    /// The replacing transaction's completion carries this release (`previousReleaseCallbackId`): the channel write goes first, then that completion.
    carried: bool,
}

/// A completion held back until the presenter released the older buffers of every layer it names. BBQ releases each submitted buffer older than the frame a completion names, faking NO_FENCE (BLASTBufferQueue.cpp:415-427), so a completion never overtakes a release it would fake.
#[derive(Debug)]
struct Pending {
    listener: StrongBinder,
    stats: TransactionStats,
    /// Each layer the completion names with its latched frame; frame 0 (no buffer) gates nothing.
    gates: Vec<(BinderKey, u64)>,
}

impl Held {
    fn id(&self) -> ReleaseCallbackId {
        ReleaseCallbackId {
            buffer_id: self.buffer.id,
            frame_number: self.frame_number,
        }
    }
}

#[derive(Clone, Debug)]
struct Layer {
    handle: StrongBinder,
    id: i32,
    name: String,
    parent: Option<BinderKey>,
    relative_to: Option<BinderKey>,
    z: i32,
    x: f32,
    y: f32,
    scale_x: f32,
    scale_y: f32,
    layer_stack: Option<u32>,
    flags: u32,
    alpha: f32,
    crop: Option<FloatRect>,
    buffer_crop: Option<Rect>,
    destination_frame: Option<Rect>,
    buffer_transform: u32,
    held: Option<Held>,
    /// r, g, b as `eColorChanged` set them; a component < 0 = no colour fill (RequestedLayerState's encoding).
    color: [f32; 3],
    release_channel: Option<FdObject>,
    latched_this_frame: bool,
    seq: u64,
}

/// What applying a buffer did to its layer: the frame numbers it replaced and took (0 when none), and the replaced buffer's release when its completion carries it.
#[derive(Debug)]
struct BufferChange {
    previous_frame: u64,
    frame: u64,
    carried: Option<(ReleaseCallbackId, StrongBinder)>,
}

/// What a frame knows about one transaction it applied, to answer its callbacks afterwards.
#[derive(Debug)]
struct Applied {
    transaction_level: Vec<ListenerCallbacks>,
    layers: Vec<AppliedLayer>,
}

#[derive(Debug)]
struct AppliedLayer {
    handle: StrongBinder,
    listeners: Vec<ListenerCallbacks>,
    frame_number: u64,
    previous_frame_number: u64,
    /// The replaced buffer's release, carried by this layer's completion to the listener it names.
    previous_release: Option<(ReleaseCallbackId, StrongBinder)>,
}

/// Whether a transaction's completion can carry the release of the buffer it replaced: one of its listeners is the buffer's release endpoint, with a complete callback (BBQ registers one on every buffer transaction).
fn carries_release(listeners: &[ListenerCallbacks], endpoint: &StrongBinder) -> bool {
    !endpoint.is_null()
        && listeners.iter().any(|lc| {
            !lc.listener.is_null()
                && lc.listener.key() == endpoint.key()
                && lc
                    .callback_ids
                    .iter()
                    .any(|id| id.ty == CallbackId::ON_COMPLETE)
        })
}

fn fence_of(fd: i32) -> Fence {
    Fence(Some(FdObject {
        fd,
        flags: 0,
        cookie: 0,
    }))
}

/// The release fence of a buffer nothing read: its acquire fence (AOSP `Layer::releasePreviousBuffer` passes `mDrawingState.acquireFence`, Layer.cpp:861-863).
fn acquire_fence_of(held: &Held) -> Fence {
    held.acquire_fence
        .map_or(Fence(None), |f| fence_of(f.raw()))
}

/// The buffers a call releases: its own release, or the one its completion carries.
fn released_buffers(call: &ClientCall) -> Vec<u64> {
    match call {
        ClientCall::ReleaseBuffer { release, .. }
        | ClientCall::ReleaseOnChannel { release, .. } => {
            vec![release.callback_id.buffer_id]
        }
        ClientCall::TransactionCompleted { stats, .. } => stats
            .transaction_stats
            .iter()
            .flat_map(|t| &t.surface_stats)
            .map(|s| s.previous_release_callback_id)
            .filter(|id| *id != NO_RELEASE_CALLBACK)
            .map(|id| id.buffer_id)
            .collect(),
    }
}

/// Gives each release in `calls`, and each completion's carried release, the read fence the window exported for its buffer (`fences`: buffer id -> sync_file). A release and the completion carrying it share one fence, taken from `fences` once; a buffer with none stays NO_FENCE.
/// Returns the fences by descriptor number: the caller keeps them open until the calls are delivered.
pub fn attach_read_fences(
    calls: &mut [ClientCall],
    fences: &mut HashMap<u64, OwnedFd>,
) -> HashMap<i32, OwnedFd> {
    let mut held = HashMap::new();
    let mut fd_of: HashMap<u64, i32> = HashMap::new();
    for call in calls.iter() {
        for buffer_id in released_buffers(call) {
            if fd_of.contains_key(&buffer_id) {
                continue;
            }
            if let Some(fence) = fences.remove(&buffer_id) {
                let fd = fence.as_raw_fd();
                fd_of.insert(buffer_id, fd);
                held.insert(fd, fence);
            }
        }
    }
    for call in calls.iter_mut() {
        match call {
            ClientCall::ReleaseBuffer { release, .. }
            | ClientCall::ReleaseOnChannel { release, .. } => {
                if let Some(&fd) = fd_of.get(&release.callback_id.buffer_id) {
                    release.release_fence = fence_of(fd);
                }
            }
            ClientCall::TransactionCompleted { stats, .. } => {
                for s in stats
                    .transaction_stats
                    .iter_mut()
                    .flat_map(|t| t.surface_stats.iter_mut())
                {
                    if s.previous_release_callback_id != NO_RELEASE_CALLBACK
                        && let Some(&fd) = fd_of.get(&s.previous_release_callback_id.buffer_id)
                    {
                        s.previous_release_fence = Some(fence_of(fd));
                    }
                }
            }
        }
    }
    held
}

fn rect_valid(r: &Rect) -> bool {
    r.right > r.left && r.bottom > r.top
}

fn float_rect_valid(r: &FloatRect) -> bool {
    r.right > r.left && r.bottom > r.top
}

fn intersect(a: FloatRect, b: FloatRect) -> FloatRect {
    FloatRect {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    }
}

/// A layer's accumulated placement: its world transform (scale and translation), alpha, visibility and clip.
#[derive(Clone, Copy, Debug)]
struct Geometry {
    sx: f32,
    sy: f32,
    tx: f32,
    ty: f32,
    alpha: f32,
    hidden: bool,
    layer_stack: u32,
    clip: Option<FloatRect>,
}

impl Geometry {
    fn root(layer_stack: u32) -> Self {
        Self {
            sx: 1.0,
            sy: 1.0,
            tx: 0.0,
            ty: 0.0,
            alpha: 1.0,
            hidden: false,
            layer_stack,
            clip: None,
        }
    }

    fn map(&self, r: FloatRect) -> FloatRect {
        FloatRect {
            left: r.left * self.sx + self.tx,
            top: r.top * self.sy + self.ty,
            right: r.right * self.sx + self.tx,
            bottom: r.bottom * self.sy + self.ty,
        }
    }
}

/// The compositor.
#[derive(Debug)]
pub struct Compositor {
    config: CompositorConfig,
    layers: HashMap<BinderKey, Layer>,
    next_seq: u64,
    /// The server side of the client buffer cache, keyed by the client's cache token and the buffer id.
    cache: HashMap<(BinderKey, u64), Arc<GraphicBuffer>>,
    /// Transactions waiting for a tick, per apply token (`None`: the null token), each queue in arrival order.
    queues: BTreeMap<Option<BinderKey>, VecDeque<TransactionState>>,
    frame: u64,
    dirty: bool,
    counters: Counters,
    /// Whether the acquire fence with this descriptor number has signalled (never waits); see [`Compositor::with_fence_probe`].
    fence_signalled: fn(i32) -> bool,
    /// Buffers the presenter may still draw after the core replaced or destroyed them: released at its paint ([`Compositor::painted`]).
    retired: Vec<Retired>,
    /// Completions held back ([`Pending`]), in the order they were produced.
    pending: Vec<Pending>,
}

/// The probe of a compositor whose producers fence nothing (the model tests): every fence counts as signalled.
fn every_fence_signalled(_fd: i32) -> bool {
    true
}

impl Compositor {
    /// A compositor with no layers.
    pub fn new(config: CompositorConfig) -> Self {
        Self {
            config,
            layers: HashMap::new(),
            next_seq: 0,
            cache: HashMap::new(),
            queues: BTreeMap::new(),
            frame: 0,
            dirty: false,
            counters: Counters::default(),
            fence_signalled: every_fence_signalled,
            retired: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Latches a buffer only once its acquire fence has signalled, as SurfaceFlinger does (`transactionReadyBufferCheck`, SurfaceFlinger.cpp: an unsignalled `acquireFence` makes the transaction `NotReady` unless latch-unsignaled is configured, which is off by default):
    /// the transaction stays queued, the layer keeps showing its latched buffer, and every tick checks again. `probe` reads the fence descriptor the core names, which its owner keeps open while it does ([`Compositor::referenced_fds`]).
    pub fn with_fence_probe(mut self, probe: fn(i32) -> bool) -> Self {
        self.fence_signalled = probe;
        self
    }

    /// The settings.
    pub fn config(&self) -> &CompositorConfig {
        &self.config
    }

    /// What was counted instead of done.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Layers alive.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Transactions queued and not yet applied.
    pub fn pending_transactions(&self) -> usize {
        self.queues.values().map(VecDeque::len).sum()
    }

    /// The descriptor numbers the core still refers to: the buffers layers show and the client cache keeps, the layers' release channels and acquire fences, and everything in the transactions still queued.
    /// The core never owns a descriptor; the front-end that took them from the parcels closes the ones missing here (the fd ownership rule: a descriptor lives while the core names it).
    pub fn referenced_fds(&self) -> BTreeSet<i32> {
        let mut fds: Vec<i32> = Vec::new();
        for l in self.layers.values() {
            fds.extend(l.release_channel.map(|f| f.fd));
            if let Some(h) = &l.held {
                fds.extend(h.buffer.fds.iter().map(|f| f.raw()));
                fds.extend(h.acquire_fence.map(SceneFd::raw));
            }
        }
        for b in self.cache.values() {
            fds.extend(b.fds.iter().map(|f| f.fd));
        }
        for r in &self.retired {
            fds.extend(r.channel.map(|f| f.fd));
            fds.extend(r.held.buffer.fds.iter().map(|f| f.raw()));
            fds.extend(r.held.acquire_fence.map(SceneFd::raw));
        }
        for t in self.queues.values().flatten() {
            fds.extend(t.fds().into_iter().map(|f| f.fd));
        }
        fds.into_iter().collect()
    }

    /// Whether the next tick has anything to do: the platform's frame clock may stay stopped otherwise.
    pub fn wants_frame(&self) -> bool {
        self.dirty || self.pending_transactions() > 0
    }

    /// A layer was created.
    pub fn create_layer(&mut self, spec: LayerSpec) -> Result<(), ModelError> {
        let key = spec.handle.key().ok_or(ModelError::NullHandle)?;
        if self.layers.contains_key(&key) {
            return Err(ModelError::DuplicateLayer(key));
        }
        self.next_seq += 1;
        self.layers.insert(
            key,
            Layer {
                handle: spec.handle,
                id: spec.id,
                name: spec.name,
                parent: spec.parent,
                relative_to: None,
                z: 0,
                x: 0.0,
                y: 0.0,
                scale_x: 1.0,
                scale_y: 1.0,
                layer_stack: None,
                flags: 0,
                alpha: 1.0,
                crop: None,
                buffer_crop: None,
                destination_frame: None,
                buffer_transform: 0,
                held: None,
                color: if spec.fills_color {
                    [0.0; 3]
                } else {
                    [NO_COLOR_FILL; 3]
                },
                release_channel: None,
                latched_this_frame: false,
                seq: self.next_seq,
            },
        );
        self.dirty = true;
        Ok(())
    }

    /// The window manager placed layer `id` in its parent (WindowState.updateSurfacePosition -> Transaction.setPosition on the window's surface, applied at once as SurfaceFlinger applies a
    /// transaction with no barrier). False: no layer has that id.
    pub fn set_position(&mut self, id: i32, x: f32, y: f32) -> bool {
        let Some(l) = self.layers.values_mut().find(|l| l.id == id) else {
            return false;
        };
        if (l.x, l.y) != (x, y) {
            (l.x, l.y) = (x, y);
            self.dirty = true;
        }
        true
    }

    /// The window manager's colour layer `id` (Dimmer's dim layer, Dimmer.java makeDimLayer: a colour layer): filled with `c.color` at `c.alpha`, cropped to `c.size` from its origin,
    /// ordered `c.z` relative to layer `c.relative_to` (DimmerAnimationHelper.java:257 setRelativeLayer(dim, window, -1): just below the dimming window). Applied at once, as set_position.
    /// False: no layer has id `id` or `c.relative_to`.
    pub fn set_color_layer(&mut self, id: i32, c: ColorLayer) -> bool {
        let Some(anchor) = self
            .layers
            .iter()
            .find(|(_, l)| l.id == c.relative_to)
            .map(|(k, _)| *k)
        else {
            return false;
        };
        let Some(l) = self.layers.values_mut().find(|l| l.id == id) else {
            return false;
        };
        l.relative_to = Some(anchor);
        l.z = c.z;
        l.color = c.color;
        l.alpha = c.alpha;
        l.crop = Some(FloatRect {
            left: 0.0,
            top: 0.0,
            right: c.size.0,
            bottom: c.size.1,
        });
        self.dirty = true;
        true
    }

    /// The client dropped its last reference to the layer: it leaves the tree (its children with it) and the buffer it held is released.
    pub fn destroy_layer(&mut self, handle: BinderKey) -> Vec<ClientCall> {
        let mut calls = Vec::new();
        if let Some(mut layer) = self.layers.remove(&handle) {
            self.dirty = true;
            if let Some(h) = layer.held.take() {
                if h.presented {
                    self.counters.releases_held += 1;
                    self.retired.push(Retired {
                        layer: handle,
                        held: h,
                        channel: layer.release_channel,
                        carried: false,
                    });
                } else {
                    let fence = acquire_fence_of(&h);
                    self.release(&h, layer.release_channel, fence, &mut calls);
                }
            }
        }
        calls
    }

    /// A transaction arrived: it waits for the next tick behind the earlier ones of its apply token.
    pub fn queue_transaction(&mut self, t: TransactionState) {
        self.queues
            .entry(t.apply_token.key())
            .or_default()
            .push_back(t);
    }

    /// Releases every held buffer (the compositor is going away) and drops what is queued. The presenter is going away too: what it still draws is released with the rest, not at its paint.
    pub fn shutdown(&mut self) -> Vec<ClientCall> {
        self.queues.clear();
        let mut calls = self.painted(&BTreeSet::new());
        for l in self.layers.values_mut() {
            if let Some(h) = l.held.as_mut() {
                h.presented = false;
            }
        }
        let keys: Vec<BinderKey> = self.layers.keys().copied().collect();
        for k in keys {
            calls.extend(self.destroy_layer(k));
        }
        calls
    }

    /// The release of `held` with `fence`: a retired buffer's read fence is attached when the window paints ([`attach_read_fences`]).
    fn release_of(&self, held: &Held, fence: Fence) -> ReleaseBuffer {
        ReleaseBuffer {
            callback_id: held.id(),
            release_fence: fence,
            current_max_acquired_buffer_count: self.config.max_acquired_buffer_count,
        }
    }

    fn release(
        &mut self,
        held: &Held,
        channel: Option<FdObject>,
        fence: Fence,
        calls: &mut Vec<ClientCall>,
    ) {
        self.counters.buffers_released += 1;
        let release = self.release_of(held, fence);
        self.push_release(release, held.release_listener, channel, calls);
    }

    /// AOSP `Layer::callReleaseBufferCallback` order: `onReleaseBuffer` on the listener, then the channel write. Binder serialises oneway calls to one node in send order, and the
    /// completions go to that node, so BBQ releases the replaced buffer before any completion of a later frame. The channel alone is read only when the producer waits in dequeue, and it
    /// loses that race (`Faking releaseBufferCallback`). The channel write stays: it wakes that wait (BLASTBufferQueue.cpp `waitForBufferRelease`).
    fn push_release(
        &mut self,
        release: ReleaseBuffer,
        listener: StrongBinder,
        channel: Option<FdObject>,
        calls: &mut Vec<ClientCall>,
    ) {
        if listener.is_null() && channel.is_none() {
            self.counters.release_unroutable += 1;
            return;
        }
        if !listener.is_null() {
            calls.push(ClientCall::ReleaseBuffer { listener, release });
        }
        if let Some(channel) = channel {
            calls.push(ClientCall::ReleaseOnChannel { channel, release });
        }
    }

    /// The ids a buffer-less release has: AOSP's `generateReleaseCallbackId` falls back to the cached id (which is the buffer id) when there is no buffer.
    fn release_unresolved(
        &mut self,
        bd: &BufferData,
        channel: Option<FdObject>,
        calls: &mut Vec<ClientCall>,
    ) {
        let id = ReleaseCallbackId {
            buffer_id: bd.cached_buffer_id,
            frame_number: bd.frame_number,
        };
        self.counters.buffers_released += 1;
        let release = ReleaseBuffer {
            callback_id: id,
            release_fence: Fence(None),
            current_max_acquired_buffer_count: self.config.max_acquired_buffer_count,
        };
        self.push_release(release, bd.release_buffer_listener, channel, calls);
    }

    /// Whether applying `t` now would give a backpressured layer a second buffer this tick.
    fn blocked(&self, t: &TransactionState) -> bool {
        t.composer_states.iter().any(|s| {
            s.what & what::E_BUFFER_CHANGED != 0
                && s.buffer_data
                    .as_ref()
                    .is_some_and(|b| b.buffer.is_some() || b.cached_buffer_id != 0)
                && s.surface
                    .key()
                    .and_then(|k| self.layers.get(&k))
                    .is_some_and(|l| {
                        l.latched_this_frame && l.flags & layer_flags::ENABLE_BACKPRESSURE != 0
                    })
        })
    }

    /// Whether `t` carries a buffer whose acquire fence has not signalled ([`Compositor::with_fence_probe`]).
    fn fence_pending(&self, t: &TransactionState) -> bool {
        t.composer_states.iter().any(|s| {
            s.what & what::E_BUFFER_CHANGED != 0
                && s.buffer_data
                    .as_ref()
                    .and_then(|b| b.acquire_fence.as_ref())
                    .and_then(|f| f.0)
                    .is_some_and(|f| !(self.fence_signalled)(f.fd))
        })
    }

    /// One frame tick at `now_ns`: apply what is due, answer callbacks, and build the scene if anything visible changed.
    pub fn on_frame(&mut self, now_ns: i64) -> FrameOutput {
        self.frame += 1;
        for l in self.layers.values_mut() {
            l.latched_this_frame = false;
        }
        let mut calls = Vec::new();
        let mut applied = Vec::new();
        let tokens: Vec<_> = self.queues.keys().copied().collect();
        for token in tokens {
            while let Some(front) = self.queues.get(&token).and_then(VecDeque::front) {
                if self.blocked(front) {
                    break;
                }
                if self.fence_pending(front) {
                    self.counters.fence_waits += 1;
                    break;
                }
                let t = self.queues.get_mut(&token).and_then(VecDeque::pop_front);
                if let Some(t) = t {
                    applied.push(self.apply(t, &mut calls));
                }
            }
        }
        self.queues.retain(|_, q| !q.is_empty());
        self.complete(applied, now_ns, &mut calls);
        let scene = if std::mem::take(&mut self.dirty) {
            let (scene, unbounded) = self.build_scene();
            self.counters.unbounded_color_skipped += unbounded;
            Some(scene)
        } else {
            None
        };
        FrameOutput { scene, calls }
    }

    fn apply(&mut self, t: TransactionState, calls: &mut Vec<ClientCall>) -> Applied {
        self.counters.transactions_applied += 1;
        if !t.is_auto_timestamp {
            self.counters.desired_present_time_ignored += 1;
        }
        if !t.display_states.is_empty() {
            self.counters.display_states_ignored += t.display_states.len() as u64;
        }
        if !t.input_window_commands.focus_requests.is_empty()
            || !t
                .input_window_commands
                .window_infos_reported_listeners
                .is_empty()
        {
            self.counters.input_commands_ignored += 1;
        }
        let mut layers = Vec::new();
        for s in &t.composer_states {
            if let Some(note) = self.apply_layer_state(s, calls) {
                layers.push(note);
            }
        }
        for u in &t.uncache_buffers {
            if let Some(k) = u.token.key() {
                self.cache.remove(&(k, u.id));
            }
        }
        Applied {
            transaction_level: t.listener_callbacks,
            layers,
        }
    }

    fn apply_layer_state(
        &mut self,
        s: &LayerState,
        calls: &mut Vec<ClientCall>,
    ) -> Option<AppliedLayer> {
        let Some(key) = s.surface.key() else {
            self.counters.unknown_layer += 1;
            return None;
        };
        if !self.layers.contains_key(&key) {
            self.counters.unknown_layer += 1;
            // a buffer for a layer that is gone still has a producer waiting for its release
            if s.what & what::E_BUFFER_CHANGED != 0
                && let Some(bd) = &s.buffer_data
            {
                let channel = s.buffer_release_channel.as_ref().map(|c| c.fd);
                self.counters.buffers_acquired += 1;
                self.release_unresolved(bd, channel, calls);
            }
            return None;
        }
        self.counters.layer_states_applied += 1;
        let w = s.what;
        let mut handled = 0u64;
        let mut frame_number = 0u64;
        let mut previous_frame_number = 0u64;
        let mut previous_release = None;
        {
            let l = self.layers.get_mut(&key).expect("checked above");
            if w & what::E_POSITION_CHANGED != 0 {
                l.x = s.x;
                l.y = s.y;
                handled |= what::E_POSITION_CHANGED;
            }
            if w & what::E_LAYER_CHANGED != 0 {
                l.z = s.z;
                l.relative_to = None;
                handled |= what::E_LAYER_CHANGED;
            }
            if w & what::E_RELATIVE_LAYER_CHANGED != 0 {
                l.z = s.z;
                l.relative_to = s.relative_layer.as_ref().and_then(|r| r.handle.key());
                handled |= what::E_RELATIVE_LAYER_CHANGED;
            }
            if w & what::E_REPARENT != 0 {
                l.parent = s.parent.as_ref().and_then(|p| p.handle.key());
                handled |= what::E_REPARENT;
            }
            if w & what::E_COLOR_CHANGED != 0 {
                l.color = [s.color[0], s.color[1], s.color[2]];
                handled |= what::E_COLOR_CHANGED;
            }
            if w & what::E_ALPHA_CHANGED != 0 {
                l.alpha = s.color[3];
                handled |= what::E_ALPHA_CHANGED;
            }
            if w & what::E_FLAGS_CHANGED != 0 {
                l.flags = (l.flags & !s.mask) | (s.flags & s.mask);
                handled |= what::E_FLAGS_CHANGED;
            }
            if w & what::E_LAYER_STACK_CHANGED != 0 {
                l.layer_stack = Some(s.layer_stack);
                handled |= what::E_LAYER_STACK_CHANGED;
            }
            if w & what::E_CROP_CHANGED != 0 {
                l.crop = float_rect_valid(&s.crop).then_some(s.crop);
                handled |= what::E_CROP_CHANGED;
            }
            if w & what::E_BUFFER_CROP_CHANGED != 0 {
                l.buffer_crop = rect_valid(&s.buffer_crop).then_some(s.buffer_crop);
                handled |= what::E_BUFFER_CROP_CHANGED;
            }
            if w & what::E_DESTINATION_FRAME_CHANGED != 0 {
                l.destination_frame =
                    rect_valid(&s.destination_frame).then_some(s.destination_frame);
                handled |= what::E_DESTINATION_FRAME_CHANGED;
            }
            if w & what::E_MATRIX_CHANGED != 0 {
                l.scale_x = s.matrix_dsdx;
                l.scale_y = s.matrix_dsdy;
                if s.matrix_dtdx != 0.0 || s.matrix_dtdy != 0.0 {
                    self.counters.matrix_rotation_ignored += 1;
                }
                handled |= what::E_MATRIX_CHANGED;
            }
            if w & what::E_BUFFER_TRANSFORM_CHANGED != 0 {
                l.buffer_transform = s.buffer_transform;
                handled |= what::E_BUFFER_TRANSFORM_CHANGED;
            }
            if w & what::E_BUFFER_RELEASE_CHANNEL_CHANGED != 0 {
                l.release_channel = s.buffer_release_channel.as_ref().map(|c| c.fd);
                handled |= what::E_BUFFER_RELEASE_CHANNEL_CHANGED;
            }
            // listener callbacks are answered per transaction (see `complete`)
            handled |= what::E_HAS_LISTENER_CALLBACKS_CHANGED;
        }
        if w & what::E_BUFFER_CHANGED != 0 && self.config.latch_buffers {
            handled |= what::E_BUFFER_CHANGED;
            let change = self.apply_buffer(key, s.buffer_data.as_ref(), &s.listeners, calls);
            previous_frame_number = change.previous_frame;
            frame_number = change.frame;
            previous_release = change.carried;
        }
        self.dirty = true;
        for &(bit, name) in what::ALL {
            if w & bit != 0 && handled & bit == 0 {
                self.counters.ignore(name);
            }
        }
        let l = self.layers.get(&key).expect("still present");
        Some(AppliedLayer {
            handle: l.handle,
            listeners: s.listeners.clone(),
            frame_number,
            previous_frame_number,
            previous_release,
        })
    }

    /// Gives the layer the buffer `bd` names (or clears it). The buffer it replaces is released: with its replacing transaction's completion when `listeners` carry it ([`carries_release`]), else standalone.
    fn apply_buffer(
        &mut self,
        key: BinderKey,
        bd: Option<&BufferData>,
        listeners: &[ListenerCallbacks],
        calls: &mut Vec<ClientCall>,
    ) -> BufferChange {
        let channel = self.layers.get(&key).and_then(|l| l.release_channel);
        let previous = self.layers.get_mut(&key).and_then(|l| l.held.take());
        let previous_frame = previous.as_ref().map_or(0, |h| h.frame_number);
        let mut new_frame = 0;
        let mut new_held = None;
        if let Some(bd) = bd {
            let cached = bd.flags & buffer_data_change::CACHED_BUFFER_CHANGED != 0;
            // a null buffer (no buffer, no cached id) clears the layer: there is no producer buffer to account for
            if bd.buffer.is_some() || cached {
                self.counters.buffers_acquired += 1;
            }
            let cache_key = bd
                .cached_buffer_token
                .key()
                .map(|k| (k, bd.cached_buffer_id));
            let resolved = match (&bd.buffer, cached, cache_key) {
                (Some(b), true, Some(ck)) => {
                    let b = Arc::new(b.clone());
                    self.cache.insert(ck, Arc::clone(&b));
                    Some(b)
                }
                (Some(b), _, _) => Some(Arc::new(b.clone())),
                (None, true, Some(ck)) => self.cache.get(&ck).cloned(),
                (None, _, _) => None,
            };
            match resolved {
                Some(buffer) => {
                    new_frame = bd.frame_number;
                    new_held = Some(Held {
                        buffer: Arc::new(SceneBuffer::of(&buffer)),
                        frame_number: bd.frame_number,
                        acquire_fence: bd.acquire_fence.as_ref().and_then(SceneFd::of_fence),
                        release_listener: bd.release_buffer_listener,
                        release_endpoint: bd.release_buffer_endpoint,
                        presented: false,
                    });
                }
                None if cached => {
                    self.counters.cache_misses += 1;
                    self.release_unresolved(bd, channel, calls);
                }
                None => {}
            }
        }
        if let Some(l) = self.layers.get_mut(&key) {
            l.held = new_held;
            l.latched_this_frame = true;
        }
        let mut carried = None;
        if let Some(p) = previous {
            if p.presented {
                // the presenter may still draw it: the release goes at its paint (`painted`). SurfaceFlinger sends a shown buffer's release with the completion of the transaction that replaced it (TransactionCallbackInvoker.cpp:137-161, 170-180)
                self.counters.releases_held += 1;
                if carries_release(listeners, &p.release_endpoint) {
                    carried = Some((p.id(), p.release_endpoint));
                }
                self.retired.push(Retired {
                    layer: key,
                    held: p,
                    channel,
                    carried: carried.is_some(),
                });
            } else {
                // never shown (replaced in the tick it was latched, or outside the scene): released now with its acquire fence (AOSP's dropped-buffer path, Layer.cpp:852-871)
                let fence = acquire_fence_of(&p);
                self.release(&p, channel, fence, calls);
            }
        }
        BufferChange {
            previous_frame,
            frame: new_frame,
            carried,
        }
    }

    fn complete(&mut self, applied: Vec<Applied>, now_ns: i64, calls: &mut Vec<ClientCall>) {
        // one TransactionStats per applied transaction and listener that registered callbacks with it; they are grouped per listener when they go out (`flush_completions`)
        for a in &applied {
            let mut listeners: Vec<StrongBinder> = Vec::new();
            for lc in a
                .transaction_level
                .iter()
                .chain(a.layers.iter().flat_map(|l| l.listeners.iter()))
            {
                if lc.listener.is_null() {
                    self.counters.null_listener += 1;
                } else if !listeners.iter().any(|l| l.key() == lc.listener.key()) {
                    listeners.push(lc.listener);
                }
            }
            for listener in listeners {
                let key = listener.key();
                let mut ids: Vec<CallbackId> = Vec::new();
                for lc in a
                    .transaction_level
                    .iter()
                    .chain(a.layers.iter().flat_map(|l| l.listeners.iter()))
                {
                    if lc.listener.key() == key {
                        for id in &lc.callback_ids {
                            if !ids.contains(id) {
                                ids.push(*id);
                            }
                        }
                    }
                }
                let surface_stats = a
                    .layers
                    .iter()
                    .filter(|l| l.listeners.iter().any(|lc| lc.listener.key() == key))
                    .map(|l| SurfaceStats {
                        surface_control: l.handle,
                        acquire_time_or_fence: AcquireTimeOrFence::Time(now_ns),
                        previous_release_fence: None,
                        transform_hint: None,
                        current_max_acquired_buffer_count: self.config.max_acquired_buffer_count,
                        event_stats: FrameEventHistoryStats {
                            frame_number: l.frame_number,
                            previous_frame_number: l.previous_frame_number,
                            refresh_start_time: now_ns,
                            ..Default::default()
                        },
                        previous_release_callback_id: l
                            .previous_release
                            .as_ref()
                            .filter(|(_, endpoint)| endpoint.key() == key)
                            .map_or(NO_RELEASE_CALLBACK, |(id, _)| *id),
                    })
                    .collect();
                let gates: Vec<(BinderKey, u64)> = a
                    .layers
                    .iter()
                    .filter(|l| {
                        l.frame_number > 0 && l.listeners.iter().any(|lc| lc.listener.key() == key)
                    })
                    .filter_map(|l| l.handle.key().map(|k| (k, l.frame_number)))
                    .collect();
                let stats = TransactionStats {
                    callback_ids: ids,
                    latch_time: now_ns,
                    present_fence: None,
                    surface_stats,
                };
                if self.gated(&gates) {
                    self.counters.completions_held += 1;
                }
                self.pending.push(Pending {
                    listener,
                    stats,
                    gates,
                });
            }
        }
        self.flush_completions(calls);
    }

    /// The buffers the presenter holds now, by `GraphicBuffer::id`: taken in a scene and not yet reported released. A layer's buffer is presented while held: replacing it, the core waits for
    /// [`painted`](Compositor::painted); one not held is released at once. Called before every tick and every destroy, so the core never releases a buffer the presenter may still draw.
    pub fn set_held(&mut self, held: &BTreeSet<u64>) {
        for l in self.layers.values_mut() {
            if let Some(h) = l.held.as_mut() {
                h.presented = held.contains(&h.buffer.id);
            }
        }
    }

    /// The presenter reported releases; `held` is what it still holds. Each replaced or destroyed buffer no longer held is released now, a release its completion carries going in the channel write
    /// before that completion, then the completions they held back follow; one still held stays retired. The caller attaches the read fences ([`attach_read_fences`]).
    pub fn painted(&mut self, held: &BTreeSet<u64>) -> Vec<ClientCall> {
        let mut calls = Vec::new();
        let (kept, released): (Vec<Retired>, Vec<Retired>) = std::mem::take(&mut self.retired)
            .into_iter()
            .partition(|r| held.contains(&r.held.buffer.id));
        self.retired = kept;
        for r in released {
            if r.carried {
                self.counters.buffers_released += 1;
                self.counters.releases_carried += 1;
                if let Some(channel) = r.channel {
                    calls.push(ClientCall::ReleaseOnChannel {
                        channel,
                        release: self.release_of(&r.held, Fence(None)),
                    });
                }
            } else {
                self.release(&r.held, r.channel, Fence(None), &mut calls);
            }
        }
        self.flush_completions(&mut calls);
        calls
    }

    /// Whether a completion naming `gates` must wait: a buffer older than one of its frames is still held by the presenter.
    fn gated(&self, gates: &[(BinderKey, u64)]) -> bool {
        self.retired.iter().any(|r| {
            gates
                .iter()
                .any(|(layer, frame)| *layer == r.layer && r.held.frame_number < *frame)
        })
    }

    /// Sends the pending completions whose gates are open. A completion waits behind an earlier one of its listener that still waits, so each listener's order is kept; the listeners are independent.
    fn flush_completions(&mut self, calls: &mut Vec<ClientCall>) {
        let mut blocked: Vec<BinderKey> = Vec::new();
        let mut ready: Vec<(StrongBinder, TransactionStats)> = Vec::new();
        let mut kept = Vec::new();
        for p in std::mem::take(&mut self.pending) {
            let listener = p
                .listener
                .key()
                .expect("a pending completion has a listener");
            if !blocked.contains(&listener) && !self.gated(&p.gates) {
                ready.push((p.listener, p.stats));
            } else {
                blocked.push(listener);
                kept.push(p);
            }
        }
        self.pending = kept;
        let mut per_listener: Vec<(StrongBinder, ListenerStats)> = Vec::new();
        for (listener, ts) in ready {
            match per_listener
                .iter_mut()
                .find(|(l, _)| l.key() == listener.key())
            {
                Some((_, stats)) => stats.transaction_stats.push(ts),
                None => per_listener.push((
                    listener,
                    ListenerStats {
                        transaction_stats: vec![ts],
                    },
                )),
            }
        }
        for (listener, stats) in per_listener {
            self.counters.completions_sent += 1;
            calls.push(ClientCall::TransactionCompleted { listener, stats });
        }
    }

    /// The effective placement of the layer at `key`, from its parent chain.
    fn geometry(&self, key: BinderKey, depth: usize) -> Option<Geometry> {
        let l = self.layers.get(&key)?;
        if depth > self.layers.len() {
            return None; // a parent cycle: the layer is not shown
        }
        let parent = match l.parent {
            Some(p) => self.geometry(p, depth + 1)?,
            None => Geometry::root(self.config.layer_stack),
        };
        let own = Geometry {
            sx: parent.sx * l.scale_x,
            sy: parent.sy * l.scale_y,
            tx: parent.tx + parent.sx * l.x,
            ty: parent.ty + parent.sy * l.y,
            alpha: parent.alpha * l.alpha,
            hidden: parent.hidden || l.flags & layer_flags::HIDDEN != 0,
            layer_stack: l.layer_stack.unwrap_or(parent.layer_stack),
            clip: parent.clip,
        };
        let clip = match l.crop {
            Some(c) => {
                let c = own.map(c);
                Some(parent.clip.map_or(c, |p| intersect(p, c)))
            }
            None => parent.clip,
        };
        Some(Geometry { clip, ..own })
    }

    /// The z-ordered draw list: per layer, children below zero, itself, children from zero up; a layer ordered `relative_to` another sorts among that layer's children.
    fn z_order(&self) -> Vec<BinderKey> {
        let mut children: HashMap<Option<BinderKey>, Vec<BinderKey>> = HashMap::new();
        for (k, l) in &self.layers {
            let anchor = match l.relative_to {
                Some(r) if self.layers.contains_key(&r) => Some(r),
                _ => l.parent.filter(|p| self.layers.contains_key(p)),
            };
            // a layer whose parent is gone leaves the tree with it
            if anchor.is_none() && l.parent.is_some() {
                continue;
            }
            children.entry(anchor).or_default().push(*k);
        }
        for v in children.values_mut() {
            v.sort_by_key(|k| {
                let l = &self.layers[k];
                (if self.config.z_order { l.z } else { 0 }, l.seq)
            });
        }
        let mut out = Vec::new();
        let mut visited = std::collections::HashSet::new();
        self.walk(None, &children, &mut visited, &mut out);
        out
    }

    fn walk(
        &self,
        node: Option<BinderKey>,
        children: &HashMap<Option<BinderKey>, Vec<BinderKey>>,
        visited: &mut std::collections::HashSet<BinderKey>,
        out: &mut Vec<BinderKey>,
    ) {
        let list = children.get(&node).map(Vec::as_slice).unwrap_or(&[]);
        let split = list.partition_point(|k| self.layers[k].z < 0);
        for &k in &list[..split] {
            self.visit(k, children, visited, out);
        }
        if let Some(n) = node {
            out.push(n);
        }
        for &k in &list[split..] {
            self.visit(k, children, visited, out);
        }
    }

    fn visit(
        &self,
        k: BinderKey,
        children: &HashMap<Option<BinderKey>, Vec<BinderKey>>,
        visited: &mut std::collections::HashSet<BinderKey>,
        out: &mut Vec<BinderKey>,
    ) {
        if visited.insert(k) {
            self.walk(Some(k), children, visited, out);
        }
    }

    /// The scene, and how many colour layers it skipped for having no bounds ([`Counters::unbounded_color_skipped`]).
    fn build_scene(&self) -> (Scene, u64) {
        let mut quads = Vec::new();
        let mut unbounded = 0;
        for key in self.z_order() {
            let l = &self.layers[&key];
            let Some(g) = self.geometry(key, 0) else {
                continue;
            };
            if g.hidden || g.alpha <= 0.0 || g.layer_stack != self.config.layer_stack {
                continue;
            }
            let Some(held) = &l.held else {
                // LayerSnapshot::fillsColor: no buffer, every component >= 0; the bounds are the crops of the layer and its ancestors (geomLayerBounds)
                if l.color.iter().all(|&c| c >= 0.0) {
                    match g.clip {
                        Some(bounds) => quads.push(Quad {
                            layer_id: l.id,
                            name: l.name.clone(),
                            content: Content::Color(l.color),
                            dst: bounds,
                            clip: g.clip,
                            alpha: g.alpha,
                        }),
                        None => unbounded += 1,
                    }
                }
                continue;
            };
            let (bw, bh) = (held.buffer.width as f32, held.buffer.height as f32);
            let src = l.buffer_crop.map_or(
                FloatRect {
                    left: 0.0,
                    top: 0.0,
                    right: bw,
                    bottom: bh,
                },
                |r| FloatRect {
                    left: r.left as f32,
                    top: r.top as f32,
                    right: r.right as f32,
                    bottom: r.bottom as f32,
                },
            );
            let use_destination = l.flags & layer_flags::IGNORE_DESTINATION_FRAME == 0;
            let local = match l.destination_frame.filter(|_| use_destination) {
                Some(d) => FloatRect {
                    left: d.left as f32,
                    top: d.top as f32,
                    right: d.right as f32,
                    bottom: d.bottom as f32,
                },
                None => {
                    let (w, h) = (src.right - src.left, src.bottom - src.top);
                    let (w, h) = if l.buffer_transform & BUFFER_TRANSFORM_ROT_90 != 0 {
                        (h, w)
                    } else {
                        (w, h)
                    };
                    FloatRect {
                        left: 0.0,
                        top: 0.0,
                        right: w,
                        bottom: h,
                    }
                }
            };
            quads.push(Quad {
                layer_id: l.id,
                name: l.name.clone(),
                content: Content::Buffer(BufferContent {
                    buffer: Arc::clone(&held.buffer),
                    frame_number: held.frame_number,
                    acquire_fence: held.acquire_fence,
                    src,
                    opaque: l.flags & layer_flags::OPAQUE != 0,
                    buffer_transform: l.buffer_transform,
                }),
                dst: g.map(local),
                clip: g.clip,
                alpha: g.alpha,
            });
        }
        (
            Scene {
                frame: self.frame,
                layer_stack: self.config.layer_stack,
                quads,
            },
            unbounded,
        )
    }
}

#[cfg(test)]
mod tests;
