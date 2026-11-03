// SPDX-License-Identifier: Apache-2.0
// Derived from the Android Open Source Project: frameworks/native/libs/gui/LayerState.cpp (layer_state_t, ComposerState, DisplayState, InputWindowCommands, BufferData, TrustedPresentationListener),
// frameworks/native/libs/gui/SurfaceControl.cpp (SurfaceControl parcel), frameworks/native/libs/gui/WindowInfo.cpp, frameworks/native/libs/gui/DisplayLuts.cpp, frameworks/native/libs/gui/LayerMetadata.cpp,
// frameworks/native/libs/gui/BufferReleaseChannel.cpp (ProducerEndpoint), frameworks/native/libs/ui/GraphicBuffer.cpp + Fence.cpp + Region.cpp (flatten), frameworks/native/libs/gui/HdrMetadata.cpp,
// frameworks/native/libs/gui/android/gui/*.aidl (FrameTimelineInfo, BorderSettings, EdgeExtensionParameters, TrustedPresentationThresholds, FocusRequest, InputApplicationInfo)
//! The state a client hands the compositor, as Rust values with AOSP's field order and wire form (Android 16, libgui flags as the converter build sets them: picture profiles on).
//! Every field of `layer_state_t` is decoded and re-encoded, so a parcel round-trips byte for byte whichever parts the model uses; [`crate::model`] applies the subset the software path needs and counts the rest.
//! Binders and fds are objects ([`StrongBinder`], [`FdObject`]); nothing here owns a descriptor.

use crate::parcel::{FdObject, ParcelError, Reader, StrongBinder, WireParcel};

type R<T> = Result<T, ParcelError>;

/// `GraphicBuffer::flatten`'s magic 'GB01' (64-bit usage).
const GRAPHIC_BUFFER_MAGIC: i32 = 0x4742_3031;
/// Words in a flattened GraphicBuffer before the native handle's ints.
const GRAPHIC_BUFFER_WORDS: usize = 13;
/// `HdrMetadata::Type` bits.
const HDR_SMPTE2086: u32 = 1 << 0;
const HDR_CTA861_3: u32 = 1 << 1;
const HDR_HDR10PLUS: u32 = 1 << 2;
/// `sizeof(android_smpte2086_metadata)`: four xy chromaticities, max and min luminance = ten floats.
const SMPTE2086_BYTES: usize = 40;
/// `sizeof(android_cta861_3_metadata)`: two floats.
const CTA861_3_BYTES: usize = 8;
/// `sizeof(StretchEffect)`: six floats and a FloatRect.
const STRETCH_EFFECT_BYTES: usize = 40;

/// `ui::Rect`: four int32, left top right bottom (a LightFlattenablePod, raw 16 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// left edge.
    pub left: i32,
    /// top edge.
    pub top: i32,
    /// right edge (exclusive).
    pub right: i32,
    /// bottom edge (exclusive).
    pub bottom: i32,
}

impl Rect {
    /// `Rect::INVALID_RECT`.
    pub const INVALID: Rect = Rect {
        left: 0,
        top: 0,
        right: -1,
        bottom: -1,
    };

    fn write(&self, p: &mut WireParcel) {
        p.write_i32(self.left);
        p.write_i32(self.top);
        p.write_i32(self.right);
        p.write_i32(self.bottom);
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        Ok(Self {
            left: r.read_i32()?,
            top: r.read_i32()?,
            right: r.read_i32()?,
            bottom: r.read_i32()?,
        })
    }
}

/// `FloatRect` as `layer_state_t::crop` holds it; the wire order is top, left, bottom, right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloatRect {
    /// left edge.
    pub left: f32,
    /// top edge.
    pub top: f32,
    /// right edge.
    pub right: f32,
    /// bottom edge.
    pub bottom: f32,
}

impl FloatRect {
    fn write_aosp_order(&self, p: &mut WireParcel) {
        p.write_f32(self.top);
        p.write_f32(self.left);
        p.write_f32(self.bottom);
        p.write_f32(self.right);
    }

    fn read_aosp_order(r: &mut Reader<'_>) -> R<Self> {
        let top = r.read_f32()?;
        let left = r.read_f32()?;
        let bottom = r.read_f32()?;
        let right = r.read_f32()?;
        Ok(Self {
            left,
            top,
            right,
            bottom,
        })
    }
}

/// `ui::Region` flattened: a uint32 rect count and the rects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Region {
    /// The region's rectangles.
    pub rects: Vec<Rect>,
}

impl Region {
    fn write(&self, p: &mut WireParcel) -> R<()> {
        let mut payload = WireParcel::new();
        payload.write_u32(
            u32::try_from(self.rects.len()).map_err(|_| ParcelError::TooLarge("region"))?,
        );
        for r in &self.rects {
            r.write(&mut payload);
        }
        p.write_flattenable(payload.data(), &[])
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        let f = r.read_flattenable()?;
        if !f.fds.is_empty() {
            return Err(ParcelError::BadValue("region carries fds"));
        }
        let sub = WireParcel::from_wire(f.payload.to_vec(), vec![])?;
        let mut s = sub.reader();
        let n =
            usize::try_from(s.read_u32()?).map_err(|_| ParcelError::BadValue("region count"))?;
        if n > s.remaining() / 16 {
            return Err(ParcelError::BadValue("region count exceeds its payload"));
        }
        let rects = (0..n).map(|_| Rect::read(&mut s)).collect::<R<Vec<_>>>()?;
        s.finish()?;
        Ok(Self { rects })
    }
}

/// `android::Fence` flattened: a uint32 fd count (0 or 1) and, when 1, the fd object after it. `None` is an invalid fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence(pub Option<FdObject>);

impl Fence {
    /// `Parcel::write(Fence)`.
    pub fn write(&self, p: &mut WireParcel) -> R<()> {
        let fds: Vec<FdObject> = self.0.into_iter().collect();
        p.write_flattenable(&u32::from(self.0.is_some()).to_le_bytes(), &fds)
    }

    /// `Parcel::read(Fence)`.
    pub fn read(r: &mut Reader<'_>) -> R<Self> {
        let f = r.read_flattenable()?;
        let count = u32::from_le_bytes(
            f.payload
                .try_into()
                .map_err(|_| ParcelError::BadValue("fence payload is one uint32"))?,
        );
        match (count, f.fds.as_slice()) {
            (0, []) => Ok(Self(None)),
            (1, [fd]) => Ok(Self(Some(*fd))),
            _ => Err(ParcelError::BadValue(
                "fence fd count disagrees with its payload",
            )),
        }
    }
}

/// A flattened `GraphicBuffer`: geometry, usage, id and the native handle (fds as objects, ints inline).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphicBuffer {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Row stride in pixels.
    pub stride: i32,
    /// `PixelFormat`.
    pub format: i32,
    /// Layer count.
    pub layer_count: u32,
    /// `GRALLOC_USAGE_*` / `AHARDWAREBUFFER_USAGE_*` bits, 64-bit.
    pub usage: u64,
    /// Process-unique buffer id (upper 32 bits: pid-derived, lower: counter).
    pub id: u64,
    /// Generation number the allocator stamped.
    pub generation_number: u32,
    /// The native handle's fds.
    pub fds: Vec<FdObject>,
    /// The native handle's ints.
    pub ints: Vec<i32>,
}

impl GraphicBuffer {
    fn write(&self, p: &mut WireParcel) -> R<()> {
        let mut w = WireParcel::new();
        w.write_i32(GRAPHIC_BUFFER_MAGIC);
        w.write_i32(self.width);
        w.write_i32(self.height);
        w.write_i32(self.stride);
        w.write_i32(self.format);
        w.write_u32(self.layer_count);
        w.write_u32(self.usage as u32);
        w.write_u32((self.id >> 32) as u32);
        w.write_u32(self.id as u32);
        w.write_u32(self.generation_number);
        w.write_len(self.fds.len(), "buffer fd count")?;
        w.write_len(self.ints.len(), "buffer int count")?;
        w.write_u32((self.usage >> 32) as u32);
        for &i in &self.ints {
            w.write_i32(i);
        }
        p.write_flattenable(w.data(), &self.fds)
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        let f = r.read_flattenable()?;
        let sub = WireParcel::from_wire(f.payload.to_vec(), vec![])?;
        let mut s = sub.reader();
        if s.read_i32()? != GRAPHIC_BUFFER_MAGIC {
            return Err(ParcelError::BadValue("GraphicBuffer magic is not GB01"));
        }
        let width = s.read_i32()?;
        let height = s.read_i32()?;
        let stride = s.read_i32()?;
        let format = s.read_i32()?;
        let layer_count = s.read_u32()?;
        let usage_lo = s.read_u32()?;
        let id_hi = s.read_u32()?;
        let id_lo = s.read_u32()?;
        let generation_number = s.read_u32()?;
        let num_fds =
            usize::try_from(s.read_i32()?).map_err(|_| ParcelError::BadValue("buffer fd count"))?;
        let num_ints = usize::try_from(s.read_i32()?)
            .map_err(|_| ParcelError::BadValue("buffer int count"))?;
        let usage_hi = s.read_u32()?;
        if num_fds != f.fds.len()
            || num_ints != s.remaining() / 4
            || GRAPHIC_BUFFER_WORDS * 4 + num_ints * 4 != f.payload.len()
        {
            return Err(ParcelError::BadValue(
                "GraphicBuffer handle counts disagree with the payload",
            ));
        }
        let ints = (0..num_ints).map(|_| s.read_i32()).collect::<R<Vec<_>>>()?;
        Ok(Self {
            width,
            height,
            stride,
            format,
            layer_count,
            usage: u64::from(usage_hi) << 32 | u64::from(usage_lo),
            id: u64::from(id_hi) << 32 | u64::from(id_lo),
            generation_number,
            fds: f.fds,
            ints,
        })
    }
}

/// `BufferData::flags` bits (`BufferDataChange`).
pub mod buffer_data_change {
    /// The acquire fence changed.
    pub const FENCE_CHANGED: i32 = 0x01;
    /// The frame number changed.
    pub const FRAME_NUMBER_CHANGED: i32 = 0x02;
    /// The cached-buffer id changed.
    pub const CACHED_BUFFER_CHANGED: i32 = 0x04;
}

/// `BufferData`: the buffer a layer shows, or the cached id standing for one the compositor already holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferData {
    /// `BufferDataChange` bits.
    pub flags: i32,
    /// The buffer itself; `None` once it is cached and the id stands for it.
    pub buffer: Option<GraphicBuffer>,
    /// The fence signalled when the producer is done writing.
    pub acquire_fence: Option<Fence>,
    /// BufferQueue frame number (BBQ's own counter).
    pub frame_number: u64,
    /// `releaseBufferListener` as a binder (an `ITransactionCompletedListener`).
    pub release_buffer_listener: StrongBinder,
    /// The binder the release information is sent to.
    pub release_buffer_endpoint: StrongBinder,
    /// `cachedBuffer.token`: identifies the client's cache.
    pub cached_buffer_token: StrongBinder,
    /// `cachedBuffer.id`.
    pub cached_buffer_id: u64,
    /// The layer must apply after `barrier_frame_number`.
    pub has_barrier: bool,
    /// See `has_barrier`.
    pub barrier_frame_number: u64,
    /// BufferQueue producer id.
    pub producer_id: u32,
    /// When the buffer was dequeued, ns (-1 unset).
    pub dequeue_time: i64,
}

impl BufferData {
    fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_i32(self.flags);
        match &self.buffer {
            Some(b) => {
                p.write_bool(true);
                b.write(p)?;
            }
            None => p.write_bool(false),
        }
        match &self.acquire_fence {
            Some(f) => {
                p.write_bool(true);
                f.write(p)?;
            }
            None => p.write_bool(false),
        }
        p.write_u64(self.frame_number);
        p.write_strong_binder(&self.release_buffer_listener);
        p.write_strong_binder(&self.release_buffer_endpoint);
        p.write_strong_binder(&self.cached_buffer_token);
        p.write_u64(self.cached_buffer_id);
        p.write_bool(self.has_barrier);
        p.write_u64(self.barrier_frame_number);
        p.write_u32(self.producer_id);
        p.write_i64(self.dequeue_time);
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        let flags = r.read_i32()?;
        let buffer = if r.read_bool()? {
            Some(GraphicBuffer::read(r)?)
        } else {
            None
        };
        let acquire_fence = if r.read_bool()? {
            Some(Fence::read(r)?)
        } else {
            None
        };
        Ok(Self {
            flags,
            buffer,
            acquire_fence,
            frame_number: r.read_u64()?,
            release_buffer_listener: r.read_strong_binder(true, "releaseBufferListener")?,
            release_buffer_endpoint: r.read_strong_binder(true, "releaseBufferEndpoint")?,
            cached_buffer_token: r.read_strong_binder(true, "cachedBuffer.token")?,
            cached_buffer_id: r.read_u64()?,
            has_barrier: r.read_bool()?,
            barrier_frame_number: r.read_u64()?,
            producer_id: r.read_u32()?,
            dequeue_time: r.read_i64()?,
        })
    }
}

/// `CallbackId`: a client-side callback number and its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CallbackId {
    /// The id.
    pub id: i64,
    /// `CallbackId::Type`: 0 on commit, 1 on complete, 2 unspecified.
    pub ty: i32,
}

impl CallbackId {
    /// `CallbackId::Type::ON_COMMIT`.
    pub const ON_COMMIT: i32 = 0;
    /// `CallbackId::Type::ON_COMPLETE`.
    pub const ON_COMPLETE: i32 = 1;

    pub(crate) fn write_parcelable(&self, p: &mut WireParcel) {
        p.write_parcelable_flag();
        p.write_i64(self.id);
        p.write_i32(self.ty);
    }

    pub(crate) fn read_parcelable(r: &mut Reader<'_>) -> R<Self> {
        r.read_parcelable_flag("CallbackId")?;
        Ok(Self {
            id: r.read_i64()?,
            ty: r.read_i32()?,
        })
    }
}

pub(crate) fn write_callback_ids(p: &mut WireParcel, ids: &[CallbackId]) -> R<()> {
    p.write_len(ids.len(), "callback ids")?;
    ids.iter().for_each(|c| c.write_parcelable(p));
    Ok(())
}

pub(crate) fn read_callback_ids(r: &mut Reader<'_>) -> R<Vec<CallbackId>> {
    let n = r.read_count("callback id count")?;
    (0..n).map(|_| CallbackId::read_parcelable(r)).collect()
}

/// `ListenerCallbacks`: a transaction-completed listener and the callbacks it registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerCallbacks {
    /// The `ITransactionCompletedListener` binder.
    pub listener: StrongBinder,
    /// The callbacks registered for it.
    pub callback_ids: Vec<CallbackId>,
}

impl ListenerCallbacks {
    pub(crate) fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_strong_binder(&self.listener);
        write_callback_ids(p, &self.callback_ids)
    }

    pub(crate) fn read(r: &mut Reader<'_>, nullable_listener: bool) -> R<Self> {
        Ok(Self {
            listener: r.read_strong_binder(nullable_listener, "listener")?,
            callback_ids: read_callback_ids(r)?,
        })
    }
}

/// `SurfaceControl::writeToParcel`: how a layer is named across processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceControlParcel {
    /// The `ISurfaceComposerClient` binder the layer belongs to.
    pub client: StrongBinder,
    /// The layer handle binder.
    pub handle: StrongBinder,
    /// The layer id.
    pub layer_id: i32,
    /// The layer name.
    pub name: String,
    /// The transform hint.
    pub transform_hint: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format.
    pub format: u32,
}

impl SurfaceControlParcel {
    /// `SurfaceControl::writeToParcel`.
    pub fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_strong_binder(&self.client);
        p.write_strong_binder(&self.handle);
        p.write_i32(self.layer_id);
        p.write_utf8_as_utf16(&self.name)?;
        p.write_u32(self.transform_hint);
        p.write_u32(self.width);
        p.write_u32(self.height);
        p.write_u32(self.format);
        Ok(())
    }

    /// `SurfaceControl::readFromParcel`.
    pub fn read(r: &mut Reader<'_>) -> R<Self> {
        Ok(Self {
            client: r.read_strong_binder(false, "SurfaceControl client")?,
            handle: r.read_strong_binder(false, "SurfaceControl handle")?,
            layer_id: r.read_i32()?,
            name: r.read_utf8_from_utf16()?,
            transform_hint: r.read_u32()?,
            width: r.read_u32()?,
            height: r.read_u32()?,
            format: r.read_u32()?,
        })
    }

    fn write_nullable(sc: &Option<Self>, p: &mut WireParcel) -> R<()> {
        p.write_bool(sc.is_some());
        sc.as_ref().map_or(Ok(()), |s| s.write(p))
    }

    fn read_nullable(r: &mut Reader<'_>) -> R<Option<Self>> {
        if r.read_bool()? {
            Self::read(r).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// `android.gui.FrameTimelineInfo` (AIDL): the vsync a frame was started for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameTimelineInfo {
    /// The Choreographer vsync id (`INVALID_VSYNC_ID` = -1).
    pub vsync_id: i64,
    /// The input event this frame responds to.
    pub input_event_id: i32,
    /// When the frame started, ns.
    pub start_time_nanos: i64,
    /// Whether the refresh-rate selection may use it.
    pub use_for_refresh_rate_selection: bool,
    /// The vsync of a skipped frame.
    pub skipped_frame_vsync_id: i64,
    /// Start time of a skipped frame, ns.
    pub skipped_frame_start_time_nanos: i64,
}

impl Default for FrameTimelineInfo {
    fn default() -> Self {
        Self {
            vsync_id: -1,
            input_event_id: 0,
            start_time_nanos: 0,
            use_for_refresh_rate_selection: false,
            skipped_frame_vsync_id: -1,
            skipped_frame_start_time_nanos: 0,
        }
    }
}

impl FrameTimelineInfo {
    pub(crate) fn write_parcelable(&self, p: &mut WireParcel) -> R<()> {
        p.write_parcelable_flag();
        let s = p.begin_aidl_parcelable();
        p.write_i64(self.vsync_id);
        p.write_i32(self.input_event_id);
        p.write_i64(self.start_time_nanos);
        p.write_bool(self.use_for_refresh_rate_selection);
        p.write_i64(self.skipped_frame_vsync_id);
        p.write_i64(self.skipped_frame_start_time_nanos);
        p.end_aidl_parcelable(s)
    }

    pub(crate) fn read_parcelable(r: &mut Reader<'_>) -> R<Self> {
        r.read_parcelable_flag("FrameTimelineInfo")?;
        let end = r.begin_aidl_parcelable()?;
        let mut v = Self::default();
        if r.more_before(end) {
            v.vsync_id = r.read_i64()?;
        }
        if r.more_before(end) {
            v.input_event_id = r.read_i32()?;
        }
        if r.more_before(end) {
            v.start_time_nanos = r.read_i64()?;
        }
        if r.more_before(end) {
            v.use_for_refresh_rate_selection = r.read_bool()?;
        }
        if r.more_before(end) {
            v.skipped_frame_vsync_id = r.read_i64()?;
        }
        if r.more_before(end) {
            v.skipped_frame_start_time_nanos = r.read_i64()?;
        }
        r.skip_to(end)?;
        Ok(v)
    }
}

/// `android.gui.BorderSettings` (AIDL).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BorderSettings {
    /// Stroke width in pixels.
    pub stroke_width: f32,
    /// ARGB colour.
    pub color: i32,
}

/// `android.gui.EdgeExtensionParameters` (AIDL): which edges extend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeExtensionParameters {
    /// Extend the right edge.
    pub extend_right: bool,
    /// Extend the left edge.
    pub extend_left: bool,
    /// Extend the top edge.
    pub extend_top: bool,
    /// Extend the bottom edge.
    pub extend_bottom: bool,
}

/// `android.gui.TrustedPresentationThresholds` (AIDL).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrustedPresentationThresholds {
    /// Minimum alpha, -1 unset.
    pub min_alpha: f32,
    /// Minimum fraction rendered, -1 unset.
    pub min_fraction_rendered: f32,
    /// Stability requirement, ms.
    pub stability_requirement_ms: i32,
}

impl Default for TrustedPresentationThresholds {
    fn default() -> Self {
        Self {
            min_alpha: -1.0,
            min_fraction_rendered: -1.0,
            stability_requirement_ms: 0,
        }
    }
}

/// `TrustedPresentationListener`: the callback binder and id (hand-written Parcelable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrustedPresentationListener {
    /// The `ITransactionCompletedListener` binder, null when unset.
    pub callback_interface: StrongBinder,
    /// The callback id, -1 unset.
    pub callback_id: i32,
}

impl Default for TrustedPresentationListener {
    fn default() -> Self {
        Self {
            callback_interface: StrongBinder::NULL,
            callback_id: -1,
        }
    }
}

/// `android.gui.FocusRequest` (AIDL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusRequest {
    /// The window token.
    pub token: StrongBinder,
    /// The window name.
    pub window_name: String,
    /// The request time, ns.
    pub timestamp: i64,
    /// The display id.
    pub display_id: i32,
}

/// `android.gui.InputApplicationInfo` (AIDL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputApplicationInfo {
    /// The application token, null when unset.
    pub token: StrongBinder,
    /// The application name.
    pub name: String,
    /// The dispatching timeout, ms.
    pub dispatching_timeout_millis: i64,
}

/// `ui::Transform` as WindowInfo writes it: six floats, `dsdx dtdx tx dtdy dsdy ty`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform2D {
    /// dsdx
    pub dsdx: f32,
    /// dtdx
    pub dtdx: f32,
    /// x translation
    pub tx: f32,
    /// dtdy
    pub dtdy: f32,
    /// dsdy
    pub dsdy: f32,
    /// y translation
    pub ty: f32,
}

impl Transform2D {
    fn write(&self, p: &mut WireParcel) {
        for v in [self.dsdx, self.dtdx, self.tx, self.dtdy, self.dsdy, self.ty] {
            p.write_f32(v);
        }
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        Ok(Self {
            dsdx: r.read_f32()?,
            dtdx: r.read_f32()?,
            tx: r.read_f32()?,
            dtdy: r.read_f32()?,
            dsdy: r.read_f32()?,
            ty: r.read_f32()?,
        })
    }
}

/// `gui::WindowInfo` when its name is not empty (an empty-named info is written as one 0 word: [`LayerState::window_info`] = `None`).
#[derive(Clone, Debug, PartialEq)]
pub struct WindowInfo {
    /// Input channel token.
    pub token: StrongBinder,
    /// Dispatching timeout, ns.
    pub dispatching_timeout_ns: i64,
    /// Window id.
    pub id: i32,
    /// Non-empty window name.
    pub name: String,
    /// `layoutParamsFlags`.
    pub layout_params_flags: i32,
    /// `layoutParamsType`.
    pub layout_params_type: i32,
    /// Window frame in display space.
    pub frame: Rect,
    /// Content width.
    pub content_width: i32,
    /// Content height.
    pub content_height: i32,
    /// Surface inset.
    pub surface_inset: i32,
    /// Global scale factor.
    pub global_scale_factor: f32,
    /// Input alpha.
    pub alpha: f32,
    /// Window transform.
    pub transform: Transform2D,
    /// Touch occlusion mode.
    pub touch_occlusion_mode: i32,
    /// Owner pid.
    pub owner_pid: i32,
    /// Owner uid.
    pub owner_uid: i32,
    /// Package name.
    pub package_name: String,
    /// `inputConfig` bits.
    pub input_config: i32,
    /// Display id.
    pub display_id: i32,
    /// The application the window belongs to.
    pub application_info: InputApplicationInfo,
    /// The touchable region.
    pub touchable_region: Region,
    /// Replace the touchable region with the crop.
    pub replace_touchable_region_with_crop: bool,
    /// Layer handle whose crop bounds the touchable region.
    pub touchable_region_crop_handle: StrongBinder,
    /// The window token.
    pub window_token: StrongBinder,
    /// Focus transfer target.
    pub focus_transfer_target: StrongBinder,
    /// Whether the window may occlude a presentation.
    pub can_occlude_presentation: bool,
    /// Clone-layer-stack transform.
    pub clone_layer_stack_transform: Option<Transform2D>,
}

impl WindowInfo {
    fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_strong_binder(&self.token);
        p.write_i64(self.dispatching_timeout_ns);
        p.write_i32(self.id);
        p.write_utf8_as_utf16(&self.name)?;
        p.write_i32(self.layout_params_flags);
        p.write_i32(self.layout_params_type);
        self.frame.write(p);
        p.write_i32(self.content_width);
        p.write_i32(self.content_height);
        p.write_i32(self.surface_inset);
        p.write_f32(self.global_scale_factor);
        p.write_f32(self.alpha);
        self.transform.write(p);
        p.write_i32(self.touch_occlusion_mode);
        p.write_i32(self.owner_pid);
        p.write_i32(self.owner_uid);
        p.write_utf8_as_utf16(&self.package_name)?;
        p.write_i32(self.input_config);
        p.write_i32(self.display_id);
        self.application_info.write_aidl(p)?;
        self.touchable_region.write(p)?;
        p.write_bool(self.replace_touchable_region_with_crop);
        p.write_strong_binder(&self.touchable_region_crop_handle);
        p.write_strong_binder(&self.window_token);
        p.write_strong_binder(&self.focus_transfer_target);
        p.write_bool(self.can_occlude_presentation);
        p.write_bool(self.clone_layer_stack_transform.is_some());
        if let Some(t) = &self.clone_layer_stack_transform {
            t.write(p);
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        let token = r.read_strong_binder(true, "WindowInfo.token")?;
        let dispatching_timeout_ns = r.read_i64()?;
        let id = r.read_i32()?;
        let name = r.read_utf8_from_utf16()?;
        let layout_params_flags = r.read_i32()?;
        let layout_params_type = r.read_i32()?;
        let frame = Rect::read(r)?;
        let content_width = r.read_i32()?;
        let content_height = r.read_i32()?;
        let surface_inset = r.read_i32()?;
        let global_scale_factor = r.read_f32()?;
        let alpha = r.read_f32()?;
        let transform = Transform2D::read(r)?;
        let touch_occlusion_mode = r.read_i32()?;
        let owner_pid = r.read_i32()?;
        let owner_uid = r.read_i32()?;
        let package_name = r.read_utf8_from_utf16()?;
        let input_config = r.read_i32()?;
        let display_id = r.read_i32()?;
        let application_info = InputApplicationInfo::read_aidl(r)?;
        let touchable_region = Region::read(r)?;
        let replace_touchable_region_with_crop = r.read_bool()?;
        let touchable_region_crop_handle =
            r.read_strong_binder(true, "touchableRegionCropHandle")?;
        let window_token = r.read_strong_binder(true, "windowToken")?;
        let focus_transfer_target = r.read_strong_binder(true, "focusTransferTarget")?;
        let can_occlude_presentation = r.read_bool()?;
        let clone_layer_stack_transform = if r.read_bool()? {
            Some(Transform2D::read(r)?)
        } else {
            None
        };
        Ok(Self {
            token,
            dispatching_timeout_ns,
            id,
            name,
            layout_params_flags,
            layout_params_type,
            frame,
            content_width,
            content_height,
            surface_inset,
            global_scale_factor,
            alpha,
            transform,
            touch_occlusion_mode,
            owner_pid,
            owner_uid,
            package_name,
            input_config,
            display_id,
            application_info,
            touchable_region,
            replace_touchable_region_with_crop,
            touchable_region_crop_handle,
            window_token,
            focus_transfer_target,
            can_occlude_presentation,
            clone_layer_stack_transform,
        })
    }
}

impl InputApplicationInfo {
    /// The AIDL body (size header, token, name, timeout), with no non-null flag: `applicationInfo.writeToParcel` as WindowInfo calls it.
    fn write_aidl(&self, p: &mut WireParcel) -> R<()> {
        let s = p.begin_aidl_parcelable();
        p.write_strong_binder(&self.token);
        p.write_utf8_as_utf16(&self.name)?;
        p.write_i64(self.dispatching_timeout_millis);
        p.end_aidl_parcelable(s)
    }

    fn read_aidl(r: &mut Reader<'_>) -> R<Self> {
        let end = r.begin_aidl_parcelable()?;
        let mut v = Self {
            token: StrongBinder::NULL,
            name: String::new(),
            dispatching_timeout_millis: 0,
        };
        if r.more_before(end) {
            v.token = r.read_strong_binder(true, "InputApplicationInfo.token")?;
        }
        if r.more_before(end) {
            v.name = r.read_utf8_from_utf16()?;
        }
        if r.more_before(end) {
            v.dispatching_timeout_millis = r.read_i64()?;
        }
        r.skip_to(end)?;
        Ok(v)
    }
}

/// `gui::DisplayLuts`: a LUT file descriptor, the offsets of each LUT in it, and their properties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayLuts {
    /// The fd holding the LUT data.
    pub fd: FdObject,
    /// Offsets of each LUT inside it.
    pub offsets: Vec<i32>,
    /// Dimension, size, sampling key of each LUT.
    pub lut_properties: Vec<LutEntry>,
}

/// `DisplayLuts::Entry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LutEntry {
    /// 1D or 3D.
    pub dimension: i32,
    /// Samples per dimension.
    pub size: i32,
    /// Sampling key.
    pub sampling_key: i32,
}

/// `gui::BufferReleaseChannel::ProducerEndpoint`: the socket SurfaceFlinger writes release fences to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferReleaseEndpoint {
    /// Debug name.
    pub name: String,
    /// The producer socket.
    pub fd: FdObject,
}

/// `native_handle_t` as `Parcel::writeNativeHandle` writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeHandle {
    /// The fds.
    pub fds: Vec<FdObject>,
    /// The ints.
    pub ints: Vec<i32>,
}

/// `BlurRegion`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlurRegion {
    /// Blur radius in pixels.
    pub blur_radius: u32,
    /// Top-left corner radius.
    pub corner_radius_tl: f32,
    /// Top-right corner radius.
    pub corner_radius_tr: f32,
    /// Bottom-left corner radius.
    pub corner_radius_bl: f32,
    /// Bottom-right corner radius.
    pub corner_radius_br: f32,
    /// Alpha.
    pub alpha: f32,
    /// Left edge.
    pub left: i32,
    /// Top edge.
    pub top: i32,
    /// Right edge.
    pub right: i32,
    /// Bottom edge.
    pub bottom: i32,
}

/// `HdrMetadata` (a variable-size LightFlattenable): the validity bits and the parts they announce, kept as the raw bytes AOSP's structs flatten to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HdrMetadata {
    /// `HdrMetadata::Type` bits.
    pub valid_types: u32,
    /// `android_smpte2086_metadata`, present when its bit is set.
    pub smpte2086: Option<[u8; SMPTE2086_BYTES]>,
    /// `android_cta861_3_metadata`, present when its bit is set.
    pub cta861_3: Option<[u8; CTA861_3_BYTES]>,
    /// HDR10+ bytes, present when its bit is set.
    pub hdr10plus: Option<Vec<u8>>,
}

impl HdrMetadata {
    /// The flattened bytes (`HdrMetadata::flatten`): no padding between parts, the HDR10+ bytes end it unpadded; the parcel pads the whole blob.
    fn flatten(&self) -> R<Vec<u8>> {
        let mut w = Vec::new();
        w.extend_from_slice(&self.valid_types.to_le_bytes());
        if self.valid_types & HDR_SMPTE2086 != 0 {
            w.extend_from_slice(&self.smpte2086.unwrap_or([0; SMPTE2086_BYTES]));
        }
        if self.valid_types & HDR_CTA861_3 != 0 {
            w.extend_from_slice(&self.cta861_3.unwrap_or([0; CTA861_3_BYTES]));
        }
        if self.valid_types & HDR_HDR10PLUS != 0 {
            let plus = self.hdr10plus.as_deref().unwrap_or(&[]);
            let n = u32::try_from(plus.len()).map_err(|_| ParcelError::TooLarge("hdr10plus"))?;
            w.extend_from_slice(&n.to_le_bytes());
            w.extend_from_slice(plus);
        }
        Ok(w)
    }

    fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_light_flattenable(&self.flatten()?)
    }

    fn read(r: &mut Reader<'_>) -> R<Self> {
        let payload = r.read_light_flattenable()?;
        let mut at = 0usize;
        let mut take = |n: usize| -> R<&[u8]> {
            let s = at
                .checked_add(n)
                .and_then(|end| payload.get(at..end))
                .ok_or(ParcelError::BadValue("HdrMetadata payload too short"))?;
            at += n;
            Ok(s)
        };
        let bad = |_| ParcelError::BadValue("HdrMetadata part size");
        let valid_types = u32::from_le_bytes(take(4)?.try_into().map_err(bad)?);
        let smpte2086: Option<[u8; SMPTE2086_BYTES]> = if valid_types & HDR_SMPTE2086 != 0 {
            Some(take(SMPTE2086_BYTES)?.try_into().map_err(bad)?)
        } else {
            None
        };
        let cta861_3: Option<[u8; CTA861_3_BYTES]> = if valid_types & HDR_CTA861_3 != 0 {
            Some(take(CTA861_3_BYTES)?.try_into().map_err(bad)?)
        } else {
            None
        };
        let hdr10plus = if valid_types & HDR_HDR10PLUS != 0 {
            let n = u32::from_le_bytes(take(4)?.try_into().map_err(bad)?);
            let n = usize::try_from(n).map_err(|_| ParcelError::BadValue("hdr10plus size"))?;
            Some(take(n)?.to_vec())
        } else {
            None
        };
        if at != payload.len() {
            return Err(ParcelError::BadValue(
                "HdrMetadata payload has trailing bytes",
            ));
        }
        Ok(Self {
            valid_types,
            smpte2086,
            cta861_3,
            hdr10plus,
        })
    }
}

/// `layer_state_t`: every field in the order AOSP writes it. Defaults mirror its constructor ([`LayerState::new`]).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerState {
    /// The layer handle binder.
    pub surface: StrongBinder,
    /// The layer id, -1 unset.
    pub layer_id: i32,
    /// `what`: the bits of the fields this state changes.
    pub what: u64,
    /// x position.
    pub x: f32,
    /// y position.
    pub y: f32,
    /// Z order among siblings.
    pub z: i32,
    /// The layer stack id.
    pub layer_stack: u32,
    /// `eLayerHidden` / `eLayerOpaque` ... bits.
    pub flags: u32,
    /// Which `flags` bits this state changes.
    pub mask: u32,
    /// `matrix.dsdx`.
    pub matrix_dsdx: f32,
    /// `matrix.dtdx`.
    pub matrix_dtdx: f32,
    /// `matrix.dtdy`.
    pub matrix_dtdy: f32,
    /// `matrix.dsdy`.
    pub matrix_dsdy: f32,
    /// The crop, in layer space; `bottom`/`right` -1 means unset.
    pub crop: FloatRect,
    /// `relativeLayerSurfaceControl`.
    pub relative_layer: Option<SurfaceControlParcel>,
    /// `parentSurfaceControlForChild`.
    pub parent: Option<SurfaceControlParcel>,
    /// `color` as r, g, b, a floats (a half4 in memory); `color[3]` is the layer alpha.
    pub color: [f32; 4],
    /// The input window info; `None` when it is empty (name empty).
    pub window_info: Option<WindowInfo>,
    /// The transparent region hint.
    pub transparent_region: Region,
    /// `bufferTransform`: HAL transform bits.
    pub buffer_transform: u32,
    /// `transformToDisplayInverse`.
    pub transform_to_display_inverse: bool,
    /// The buffer dataspace.
    pub dataspace: u32,
    /// HDR metadata.
    pub hdr_metadata: HdrMetadata,
    /// The surface damage region.
    pub surface_damage_region: Region,
    /// `NATIVE_WINDOW_API_*` of the producer, -1 unset.
    pub api: i32,
    /// The sideband stream's native handle.
    pub sideband_stream: Option<NativeHandle>,
    /// The colour transform, 4x4 column-major floats.
    pub color_transform: [f32; 16],
    /// Corner radius.
    pub corner_radius: f32,
    /// Corner radius the client drew itself.
    pub client_drawn_corner_radius: f32,
    /// Background blur radius.
    pub background_blur_radius: u32,
    /// Layer metadata (key, bytes) in the order written.
    pub metadata: Vec<(u32, Vec<u8>)>,
    /// Background colour r, g, b, a.
    pub bg_color: [f32; 4],
    /// Background colour dataspace.
    pub bg_color_dataspace: u32,
    /// Colour-space agnostic layer.
    pub color_space_agnostic: bool,
    /// Transaction-completed listeners registered through this layer.
    pub listeners: Vec<ListenerCallbacks>,
    /// Shadow radius.
    pub shadow_radius: f32,
    /// Border settings.
    pub border_settings: BorderSettings,
    /// Frame-rate selection priority.
    pub frame_rate_selection_priority: i32,
    /// Frame rate, Hz.
    pub frame_rate: f32,
    /// Frame-rate compatibility.
    pub frame_rate_compatibility: i8,
    /// Change-frame-rate strategy.
    pub change_frame_rate_strategy: i8,
    /// Default frame-rate compatibility.
    pub default_frame_rate_compatibility: i8,
    /// Frame-rate category.
    pub frame_rate_category: i8,
    /// Smooth-switch-only category.
    pub frame_rate_category_smooth_switch_only: bool,
    /// Frame-rate selection strategy.
    pub frame_rate_selection_strategy: i8,
    /// Fixed transform hint, `ROT_INVALID` (0x80) unset.
    pub fixed_transform_hint: u32,
    /// Auto-refresh.
    pub auto_refresh: bool,
    /// Dimming enabled.
    pub dimming_enabled: bool,
    /// Blur regions.
    pub blur_regions: Vec<BlurRegion>,
    /// Stretch effect: width, height, vectorX, vectorY, maxAmountX, maxAmountY, then the mapped child bounds (left, top, right, bottom) floats.
    pub stretch_effect: [f32; 10],
    /// Edge extension.
    pub edge_extension: EdgeExtensionParameters,
    /// The buffer crop, `Rect::INVALID` unset.
    pub buffer_crop: Rect,
    /// The destination frame, `Rect::INVALID` unset.
    pub destination_frame: Rect,
    /// `TrustedOverlay`.
    pub trusted_overlay: i32,
    /// `DropInputMode`.
    pub drop_input_mode: u32,
    /// The buffer.
    pub buffer_data: Option<BufferData>,
    /// Trusted-presentation thresholds.
    pub trusted_presentation_thresholds: TrustedPresentationThresholds,
    /// Trusted-presentation listener.
    pub trusted_presentation_listener: TrustedPresentationListener,
    /// Current HDR/SDR ratio.
    pub current_hdr_sdr_ratio: f32,
    /// Desired HDR/SDR ratio.
    pub desired_hdr_sdr_ratio: f32,
    /// `CachingHint`.
    pub caching_hint: i32,
    /// The release channel the compositor writes buffer releases to.
    pub buffer_release_channel: Option<BufferReleaseEndpoint>,
    /// Picture profile id (`PictureProfileHandle`), 0 none.
    pub picture_profile_handle: i64,
    /// App content priority.
    pub app_content_priority: i32,
    /// LUTs.
    pub luts: Option<DisplayLuts>,
}

/// `layer_state_t::what` bits (`layer_state_t.h`), by name, in bit order.
pub mod what {
    macro_rules! bits {
        ($($name:ident = $v:expr),* $(,)?) => {
            $(
                #[doc = concat!("`layer_state_t::", stringify!($name), "`.")]
                pub const $name: u64 = $v;
            )*
            /// Every bit and its AOSP name, in bit order.
            pub const ALL: &[(u64, &str)] = &[$(($name, stringify!($name))),*];
        };
    }
    bits! {
        E_POSITION_CHANGED = 0x1,
        E_LAYER_CHANGED = 0x2,
        E_TRUSTED_PRESENTATION_INFO_CHANGED = 0x4,
        E_ALPHA_CHANGED = 0x8,
        E_MATRIX_CHANGED = 0x10,
        E_TRANSPARENT_REGION_CHANGED = 0x20,
        E_FLAGS_CHANGED = 0x40,
        E_LAYER_STACK_CHANGED = 0x80,
        E_FLUSH_JANK_DATA = 0x100,
        E_CACHING_HINT_CHANGED = 0x200,
        E_DIMMING_ENABLED_CHANGED = 0x400,
        E_SHADOW_RADIUS_CHANGED = 0x800,
        E_LUTS_CHANGED = 0x1000,
        E_BUFFER_CROP_CHANGED = 0x2000,
        E_RELATIVE_LAYER_CHANGED = 0x4000,
        E_REPARENT = 0x8000,
        E_COLOR_CHANGED = 0x1_0000,
        E_FRAME_RATE_CATEGORY_CHANGED = 0x2_0000,
        E_BUFFER_TRANSFORM_CHANGED = 0x4_0000,
        E_TRANSFORM_TO_DISPLAY_INVERSE_CHANGED = 0x8_0000,
        E_CROP_CHANGED = 0x10_0000,
        E_BUFFER_CHANGED = 0x20_0000,
        E_DEFAULT_FRAME_RATE_COMPATIBILITY_CHANGED = 0x40_0000,
        E_DATASPACE_CHANGED = 0x80_0000,
        E_HDR_METADATA_CHANGED = 0x100_0000,
        E_SURFACE_DAMAGE_REGION_CHANGED = 0x200_0000,
        E_API_CHANGED = 0x400_0000,
        E_SIDEBAND_STREAM_CHANGED = 0x800_0000,
        E_COLOR_TRANSFORM_CHANGED = 0x1000_0000,
        E_HAS_LISTENER_CALLBACKS_CHANGED = 0x2000_0000,
        E_INPUT_INFO_CHANGED = 0x4000_0000,
        E_CORNER_RADIUS_CHANGED = 0x8000_0000,
        E_DESTINATION_FRAME_CHANGED = 0x1_0000_0000,
        E_FRAME_RATE_SELECTION_STRATEGY_CHANGED = 0x2_0000_0000,
        E_BACKGROUND_COLOR_CHANGED = 0x4_0000_0000,
        E_METADATA_CHANGED = 0x8_0000_0000,
        E_COLOR_SPACE_AGNOSTIC_CHANGED = 0x10_0000_0000,
        E_FRAME_RATE_SELECTION_PRIORITY = 0x20_0000_0000,
        E_FRAME_RATE_CHANGED = 0x40_0000_0000,
        E_BACKGROUND_BLUR_RADIUS_CHANGED = 0x80_0000_0000,
        E_PRODUCER_DISCONNECT = 0x100_0000_0000,
        E_FIXED_TRANSFORM_HINT_CHANGED = 0x200_0000_0000,
        E_DESIRED_HDR_HEADROOM_CHANGED = 0x400_0000_0000,
        E_BLUR_REGIONS_CHANGED = 0x800_0000_0000,
        E_AUTO_REFRESH_CHANGED = 0x1000_0000_0000,
        E_STRETCH_CHANGED = 0x2000_0000_0000,
        E_TRUSTED_OVERLAY_CHANGED = 0x4000_0000_0000,
        E_DROP_INPUT_MODE_CHANGED = 0x8000_0000_0000,
        E_EXTENDED_RANGE_BRIGHTNESS_CHANGED = 0x1_0000_0000_0000,
        E_EDGE_EXTENSION_CHANGED = 0x2_0000_0000_0000,
        E_BUFFER_RELEASE_CHANNEL_CHANGED = 0x4_0000_0000_0000,
        E_PICTURE_PROFILE_HANDLE_CHANGED = 0x8_0000_0000_0000,
        E_APP_CONTENT_PRIORITY_CHANGED = 0x10_0000_0000_0000,
        E_CLIENT_DRAWN_CORNER_RADIUS_CHANGED = 0x20_0000_0000_0000,
        E_BORDER_SETTINGS_CHANGED = 0x40_0000_0000_0000,
    }
}

/// `layer_state_t::eLayerHidden` ... flag bits.
pub mod layer_flags {
    /// Hidden.
    pub const HIDDEN: u32 = 0x01;
    /// Opaque.
    pub const OPAQUE: u32 = 0x02;
    /// Skip screenshots.
    pub const SKIP_SCREENSHOT: u32 = 0x40;
    /// Secure.
    pub const SECURE: u32 = 0x80;
    /// Queue buffers instead of dropping the oldest (BBQ sets it).
    pub const ENABLE_BACKPRESSURE: u32 = 0x100;
    /// Display decoration.
    pub const DISPLAY_DECORATION: u32 = 0x200;
    /// Ignore the destination frame.
    pub const IGNORE_DESTINATION_FRAME: u32 = 0x400;
    /// Refresh-rate indicator.
    pub const REFRESH_RATE_INDICATOR: u32 = 0x800;
    /// Can occlude a presentation.
    pub const CAN_OCCLUDE_PRESENTATION: u32 = 0x1000;
    /// Recoverable from buffer stuffing.
    pub const RECOVERABLE_FROM_BUFFER_STUFFING: u32 = 0x2000;
}

impl LayerState {
    /// `layer_state_t()`: AOSP's constructor defaults.
    pub fn new() -> Self {
        let mut ct = [0.0f32; 16];
        for i in 0..4 {
            ct[i * 5] = 1.0;
        }
        Self {
            surface: StrongBinder::NULL,
            layer_id: -1,
            what: 0,
            x: 0.0,
            y: 0.0,
            z: 0,
            layer_stack: 0,
            flags: 0,
            mask: 0,
            matrix_dsdx: 1.0,
            matrix_dtdx: 0.0,
            matrix_dtdy: 0.0,
            matrix_dsdy: 1.0,
            crop: FloatRect {
                left: 0.0,
                top: 0.0,
                right: -1.0,
                bottom: -1.0,
            },
            relative_layer: None,
            parent: None,
            color: [0.0; 4],
            window_info: None,
            transparent_region: Region::default(),
            buffer_transform: 0,
            transform_to_display_inverse: false,
            dataspace: 0,
            hdr_metadata: HdrMetadata::default(),
            surface_damage_region: Region::default(),
            api: -1,
            sideband_stream: None,
            color_transform: ct,
            corner_radius: 0.0,
            client_drawn_corner_radius: 0.0,
            background_blur_radius: 0,
            metadata: Vec::new(),
            bg_color: [0.0; 4],
            bg_color_dataspace: 0,
            color_space_agnostic: false,
            listeners: Vec::new(),
            shadow_radius: 0.0,
            border_settings: BorderSettings::default(),
            frame_rate_selection_priority: -1,
            frame_rate: 0.0,
            frame_rate_compatibility: 0,
            change_frame_rate_strategy: 0,
            default_frame_rate_compatibility: 0,
            frame_rate_category: 0,
            frame_rate_category_smooth_switch_only: false,
            frame_rate_selection_strategy: 0,
            fixed_transform_hint: 0x80,
            auto_refresh: false,
            dimming_enabled: true,
            blur_regions: Vec::new(),
            stretch_effect: [0.0; 10],
            edge_extension: EdgeExtensionParameters::default(),
            buffer_crop: Rect::INVALID,
            destination_frame: Rect::INVALID,
            trusted_overlay: 0,
            drop_input_mode: 0,
            buffer_data: None,
            trusted_presentation_thresholds: TrustedPresentationThresholds::default(),
            trusted_presentation_listener: TrustedPresentationListener::default(),
            current_hdr_sdr_ratio: 1.0,
            desired_hdr_sdr_ratio: 1.0,
            caching_hint: 1,
            buffer_release_channel: None,
            picture_profile_handle: 0,
            app_content_priority: 0,
            luts: None,
        }
    }

    /// `layer_state_t::write`.
    pub fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_strong_binder(&self.surface);
        p.write_i32(self.layer_id);
        p.write_u64(self.what);
        p.write_f32(self.x);
        p.write_f32(self.y);
        p.write_i32(self.z);
        p.write_u32(self.layer_stack);
        p.write_u32(self.flags);
        p.write_u32(self.mask);
        for v in [
            self.matrix_dsdx,
            self.matrix_dtdx,
            self.matrix_dtdy,
            self.matrix_dsdy,
        ] {
            p.write_f32(v);
        }
        self.crop.write_aosp_order(p);
        SurfaceControlParcel::write_nullable(&self.relative_layer, p)?;
        SurfaceControlParcel::write_nullable(&self.parent, p)?;
        self.color.iter().for_each(|&v| p.write_f32(v));
        match &self.window_info {
            Some(w) => {
                p.write_i32(1);
                w.write(p)?;
            }
            None => p.write_i32(0),
        }
        self.transparent_region.write(p)?;
        p.write_u32(self.buffer_transform);
        p.write_bool(self.transform_to_display_inverse);
        p.write_u32(self.dataspace);
        self.hdr_metadata.write(p)?;
        self.surface_damage_region.write(p)?;
        p.write_i32(self.api);
        match &self.sideband_stream {
            Some(h) => {
                p.write_bool(true);
                p.write_len(h.fds.len(), "native handle fds")?;
                p.write_len(h.ints.len(), "native handle ints")?;
                h.fds.iter().for_each(|f| p.write_fd(f));
                h.ints.iter().for_each(|&i| p.write_i32(i));
            }
            None => p.write_bool(false),
        }
        self.color_transform.iter().for_each(|&v| p.write_f32(v));
        p.write_f32(self.corner_radius);
        p.write_f32(self.client_drawn_corner_radius);
        p.write_u32(self.background_blur_radius);
        p.write_parcelable_flag();
        p.write_len(self.metadata.len(), "layer metadata")?;
        for (k, v) in &self.metadata {
            p.write_u32(*k);
            p.write_len(v.len(), "layer metadata value")?;
            p.write_padded(v);
        }
        self.bg_color.iter().for_each(|&v| p.write_f32(v));
        p.write_u32(self.bg_color_dataspace);
        p.write_bool(self.color_space_agnostic);
        p.write_len(self.listeners.len(), "listeners")?;
        for l in &self.listeners {
            l.write(p)?;
        }
        p.write_f32(self.shadow_radius);
        p.write_parcelable_flag();
        let s = p.begin_aidl_parcelable();
        p.write_f32(self.border_settings.stroke_width);
        p.write_i32(self.border_settings.color);
        p.end_aidl_parcelable(s)?;
        p.write_i32(self.frame_rate_selection_priority);
        p.write_f32(self.frame_rate);
        p.write_byte(self.frame_rate_compatibility);
        p.write_byte(self.change_frame_rate_strategy);
        p.write_byte(self.default_frame_rate_compatibility);
        p.write_byte(self.frame_rate_category);
        p.write_bool(self.frame_rate_category_smooth_switch_only);
        p.write_byte(self.frame_rate_selection_strategy);
        p.write_u32(self.fixed_transform_hint);
        p.write_bool(self.auto_refresh);
        p.write_bool(self.dimming_enabled);
        p.write_len(self.blur_regions.len(), "blur regions")?;
        for b in &self.blur_regions {
            p.write_u32(b.blur_radius);
            for v in [
                b.corner_radius_tl,
                b.corner_radius_tr,
                b.corner_radius_bl,
                b.corner_radius_br,
                b.alpha,
            ] {
                p.write_f32(v);
            }
            for v in [b.left, b.top, b.right, b.bottom] {
                p.write_i32(v);
            }
        }
        // StretchEffect is a fixed-size LightFlattenablePod: its bytes, no size word
        self.stretch_effect.iter().for_each(|&v| p.write_f32(v));
        p.write_parcelable_flag();
        let s = p.begin_aidl_parcelable();
        for v in [
            self.edge_extension.extend_right,
            self.edge_extension.extend_left,
            self.edge_extension.extend_top,
            self.edge_extension.extend_bottom,
        ] {
            p.write_bool(v);
        }
        p.end_aidl_parcelable(s)?;
        self.buffer_crop.write(p);
        self.destination_frame.write(p);
        p.write_i32(self.trusted_overlay);
        p.write_u32(self.drop_input_mode);
        match &self.buffer_data {
            Some(b) => {
                p.write_bool(true);
                p.write_parcelable_flag();
                b.write(p)?;
            }
            None => p.write_bool(false),
        }
        p.write_parcelable_flag();
        let s = p.begin_aidl_parcelable();
        p.write_f32(self.trusted_presentation_thresholds.min_alpha);
        p.write_f32(self.trusted_presentation_thresholds.min_fraction_rendered);
        p.write_i32(
            self.trusted_presentation_thresholds
                .stability_requirement_ms,
        );
        p.end_aidl_parcelable(s)?;
        p.write_parcelable_flag();
        p.write_strong_binder(&self.trusted_presentation_listener.callback_interface);
        p.write_i32(self.trusted_presentation_listener.callback_id);
        p.write_f32(self.current_hdr_sdr_ratio);
        p.write_f32(self.desired_hdr_sdr_ratio);
        p.write_i32(self.caching_hint);
        match &self.buffer_release_channel {
            Some(e) => {
                p.write_bool(true);
                p.write_parcelable_flag();
                p.write_utf8_as_utf16(&e.name)?;
                p.write_fd(&e.fd);
            }
            None => p.write_bool(false),
        }
        p.write_i64(self.picture_profile_handle);
        p.write_i32(self.app_content_priority);
        match &self.luts {
            Some(l) => {
                p.write_bool(true);
                p.write_parcelable_flag();
                p.write_fd(&l.fd);
                p.write_len(l.offsets.len(), "lut offsets")?;
                l.offsets.iter().for_each(|&o| p.write_i32(o));
                p.write_len(l.lut_properties.len(), "lut properties")?;
                for e in &l.lut_properties {
                    p.write_i32(e.dimension);
                    p.write_i32(e.size);
                    p.write_i32(e.sampling_key);
                }
            }
            None => p.write_bool(false),
        }
        Ok(())
    }

    /// `layer_state_t::read`.
    pub fn read(r: &mut Reader<'_>) -> R<Self> {
        let mut s = Self::new();
        s.surface = r.read_strong_binder(true, "layer surface")?;
        s.layer_id = r.read_i32()?;
        s.what = r.read_u64()?;
        s.x = r.read_f32()?;
        s.y = r.read_f32()?;
        s.z = r.read_i32()?;
        s.layer_stack = r.read_u32()?;
        s.flags = r.read_u32()?;
        s.mask = r.read_u32()?;
        s.matrix_dsdx = r.read_f32()?;
        s.matrix_dtdx = r.read_f32()?;
        s.matrix_dtdy = r.read_f32()?;
        s.matrix_dsdy = r.read_f32()?;
        s.crop = FloatRect::read_aosp_order(r)?;
        s.relative_layer = SurfaceControlParcel::read_nullable(r)?;
        s.parent = SurfaceControlParcel::read_nullable(r)?;
        for c in &mut s.color {
            *c = r.read_f32()?;
        }
        s.window_info = if r.read_i32()? == 0 {
            None
        } else {
            Some(WindowInfo::read(r)?)
        };
        s.transparent_region = Region::read(r)?;
        s.buffer_transform = r.read_u32()?;
        s.transform_to_display_inverse = r.read_bool()?;
        s.dataspace = r.read_u32()?;
        s.hdr_metadata = HdrMetadata::read(r)?;
        s.surface_damage_region = Region::read(r)?;
        s.api = r.read_i32()?;
        s.sideband_stream = if r.read_bool()? {
            let nfds = r.read_count("native handle fds")?;
            let nints = r.read_count("native handle ints")?;
            let fds = (0..nfds).map(|_| r.read_fd()).collect::<R<Vec<_>>>()?;
            let ints = (0..nints).map(|_| r.read_i32()).collect::<R<Vec<_>>>()?;
            Some(NativeHandle { fds, ints })
        } else {
            None
        };
        for c in &mut s.color_transform {
            *c = r.read_f32()?;
        }
        s.corner_radius = r.read_f32()?;
        s.client_drawn_corner_radius = r.read_f32()?;
        s.background_blur_radius = r.read_u32()?;
        r.read_parcelable_flag("layer metadata")?;
        let n = r.read_count("layer metadata count")?;
        s.metadata = (0..n)
            .map(|_| {
                let k = r.read_u32()?;
                let len = r.read_count("layer metadata value")?;
                Ok((k, r.read_padded(len)?.to_vec()))
            })
            .collect::<R<Vec<_>>>()?;
        for c in &mut s.bg_color {
            *c = r.read_f32()?;
        }
        s.bg_color_dataspace = r.read_u32()?;
        s.color_space_agnostic = r.read_bool()?;
        let n = r.read_count("listener count")?;
        s.listeners = (0..n)
            .map(|_| ListenerCallbacks::read(r, true))
            .collect::<R<Vec<_>>>()?;
        s.shadow_radius = r.read_f32()?;
        r.read_parcelable_flag("borderSettings")?;
        let end = r.begin_aidl_parcelable()?;
        if r.more_before(end) {
            s.border_settings.stroke_width = r.read_f32()?;
        }
        if r.more_before(end) {
            s.border_settings.color = r.read_i32()?;
        }
        r.skip_to(end)?;
        s.frame_rate_selection_priority = r.read_i32()?;
        s.frame_rate = r.read_f32()?;
        s.frame_rate_compatibility = r.read_byte()?;
        s.change_frame_rate_strategy = r.read_byte()?;
        s.default_frame_rate_compatibility = r.read_byte()?;
        s.frame_rate_category = r.read_byte()?;
        s.frame_rate_category_smooth_switch_only = r.read_bool()?;
        s.frame_rate_selection_strategy = r.read_byte()?;
        s.fixed_transform_hint = r.read_u32()?;
        s.auto_refresh = r.read_bool()?;
        s.dimming_enabled = r.read_bool()?;
        let n = r.read_count("blur region count")?;
        s.blur_regions = (0..n)
            .map(|_| {
                Ok(BlurRegion {
                    blur_radius: r.read_u32()?,
                    corner_radius_tl: r.read_f32()?,
                    corner_radius_tr: r.read_f32()?,
                    corner_radius_bl: r.read_f32()?,
                    corner_radius_br: r.read_f32()?,
                    alpha: r.read_f32()?,
                    left: r.read_i32()?,
                    top: r.read_i32()?,
                    right: r.read_i32()?,
                    bottom: r.read_i32()?,
                })
            })
            .collect::<R<Vec<_>>>()?;
        debug_assert_eq!(STRETCH_EFFECT_BYTES, s.stretch_effect.len() * 4);
        for c in &mut s.stretch_effect {
            *c = r.read_f32()?;
        }
        r.read_parcelable_flag("edgeExtensionParameters")?;
        let end = r.begin_aidl_parcelable()?;
        let mut flags = [false; 4];
        for f in &mut flags {
            if r.more_before(end) {
                *f = r.read_bool()?;
            }
        }
        r.skip_to(end)?;
        s.edge_extension = EdgeExtensionParameters {
            extend_right: flags[0],
            extend_left: flags[1],
            extend_top: flags[2],
            extend_bottom: flags[3],
        };
        s.buffer_crop = Rect::read(r)?;
        s.destination_frame = Rect::read(r)?;
        s.trusted_overlay = r.read_i32()?;
        s.drop_input_mode = r.read_u32()?;
        s.buffer_data = if r.read_bool()? {
            r.read_parcelable_flag("bufferData")?;
            Some(BufferData::read(r)?)
        } else {
            None
        };
        r.read_parcelable_flag("trustedPresentationThresholds")?;
        let end = r.begin_aidl_parcelable()?;
        if r.more_before(end) {
            s.trusted_presentation_thresholds.min_alpha = r.read_f32()?;
        }
        if r.more_before(end) {
            s.trusted_presentation_thresholds.min_fraction_rendered = r.read_f32()?;
        }
        if r.more_before(end) {
            s.trusted_presentation_thresholds.stability_requirement_ms = r.read_i32()?;
        }
        r.skip_to(end)?;
        r.read_parcelable_flag("trustedPresentationListener")?;
        s.trusted_presentation_listener = TrustedPresentationListener {
            callback_interface: r.read_strong_binder(true, "trustedPresentationListener")?,
            callback_id: r.read_i32()?,
        };
        s.current_hdr_sdr_ratio = r.read_f32()?;
        s.desired_hdr_sdr_ratio = r.read_f32()?;
        s.caching_hint = r.read_i32()?;
        s.buffer_release_channel = if r.read_bool()? {
            r.read_parcelable_flag("bufferReleaseChannel")?;
            Some(BufferReleaseEndpoint {
                name: r.read_utf8_from_utf16()?,
                fd: r.read_fd()?,
            })
        } else {
            None
        };
        s.picture_profile_handle = r.read_i64()?;
        s.app_content_priority = r.read_i32()?;
        s.luts = if r.read_bool()? {
            r.read_parcelable_flag("luts")?;
            let fd = r.read_fd()?;
            let n = r.read_count("lut offsets")?;
            let offsets = (0..n).map(|_| r.read_i32()).collect::<R<Vec<_>>>()?;
            let n = r.read_count("lut properties")?;
            let lut_properties = (0..n)
                .map(|_| {
                    Ok(LutEntry {
                        dimension: r.read_i32()?,
                        size: r.read_i32()?,
                        sampling_key: r.read_i32()?,
                    })
                })
                .collect::<R<Vec<_>>>()?;
            Some(DisplayLuts {
                fd,
                offsets,
                lut_properties,
            })
        } else {
            None
        };
        Ok(s)
    }
}

impl Default for LayerState {
    fn default() -> Self {
        Self::new()
    }
}

/// `DisplayState::what` bits.
pub mod display_what {
    /// The sink surface changed.
    pub const SURFACE_CHANGED: u32 = 0x01;
    /// The layer stack changed.
    pub const LAYER_STACK_CHANGED: u32 = 0x02;
    /// The projection changed.
    pub const PROJECTION_CHANGED: u32 = 0x04;
    /// The size changed.
    pub const SIZE_CHANGED: u32 = 0x08;
    /// The flags changed.
    pub const FLAGS_CHANGED: u32 = 0x10;
}

/// `DisplayState`: a display (by token) and what a transaction changes about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayState {
    /// The display token.
    pub token: StrongBinder,
    /// The virtual display's sink (an `IGraphicBufferProducer` binder), null otherwise.
    pub surface: StrongBinder,
    /// `DisplayState::what`.
    pub what: u32,
    /// Display flags.
    pub flags: u32,
    /// The layer stack shown.
    pub layer_stack: u32,
    /// `ui::Rotation` as an int.
    pub orientation: u32,
    /// The layer-stack-space clip.
    pub layer_stack_space_rect: Rect,
    /// Where the clip lands on the display.
    pub oriented_display_space_rect: Rect,
    /// Display width.
    pub width: u32,
    /// Display height.
    pub height: u32,
}

impl DisplayState {
    /// `DisplayState::write`.
    pub fn write(&self, p: &mut WireParcel) {
        p.write_strong_binder(&self.token);
        p.write_strong_binder(&self.surface);
        p.write_u32(self.what);
        p.write_u32(self.flags);
        p.write_u32(self.layer_stack);
        p.write_u32(self.orientation);
        self.layer_stack_space_rect.write(p);
        self.oriented_display_space_rect.write(p);
        p.write_u32(self.width);
        p.write_u32(self.height);
    }

    /// `DisplayState::read`.
    pub fn read(r: &mut Reader<'_>) -> R<Self> {
        Ok(Self {
            token: r.read_strong_binder(false, "display token")?,
            surface: r.read_strong_binder(true, "display surface")?,
            what: r.read_u32()?,
            flags: r.read_u32()?,
            layer_stack: r.read_u32()?,
            orientation: r.read_u32()?,
            layer_stack_space_rect: Rect::read(r)?,
            oriented_display_space_rect: Rect::read(r)?,
            width: r.read_u32()?,
            height: r.read_u32()?,
        })
    }
}

/// `InputWindowCommands`: focus requests and window-info-reported listeners.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputWindowCommands {
    /// Focus requests.
    pub focus_requests: Vec<FocusRequest>,
    /// `IWindowInfosReportedListener` binders, in the order written.
    pub window_infos_reported_listeners: Vec<StrongBinder>,
}

impl InputWindowCommands {
    pub(crate) fn write(&self, p: &mut WireParcel) -> R<()> {
        p.write_len(self.focus_requests.len(), "focus requests")?;
        for f in &self.focus_requests {
            p.write_parcelable_flag();
            let s = p.begin_aidl_parcelable();
            p.write_strong_binder(&f.token);
            p.write_utf8_as_utf16(&f.window_name)?;
            p.write_i64(f.timestamp);
            p.write_i32(f.display_id);
            p.end_aidl_parcelable(s)?;
        }
        p.write_len(
            self.window_infos_reported_listeners.len(),
            "window infos listeners",
        )?;
        self.window_infos_reported_listeners
            .iter()
            .for_each(|l| p.write_strong_binder(l));
        Ok(())
    }

    pub(crate) fn read(r: &mut Reader<'_>) -> R<Self> {
        let n = r.read_count("focus request count")?;
        let focus_requests = (0..n)
            .map(|_| {
                r.read_parcelable_flag("FocusRequest")?;
                let end = r.begin_aidl_parcelable()?;
                let mut f = FocusRequest {
                    token: StrongBinder::NULL,
                    window_name: String::new(),
                    timestamp: 0,
                    display_id: 0,
                };
                if r.more_before(end) {
                    f.token = r.read_strong_binder(true, "FocusRequest.token")?;
                }
                if r.more_before(end) {
                    f.window_name = r.read_utf8_from_utf16()?;
                }
                if r.more_before(end) {
                    f.timestamp = r.read_i64()?;
                }
                if r.more_before(end) {
                    f.display_id = r.read_i32()?;
                }
                r.skip_to(end)?;
                Ok(f)
            })
            .collect::<R<Vec<_>>>()?;
        let n = r.read_count("window infos listener count")?;
        let window_infos_reported_listeners = (0..n)
            .map(|_| r.read_strong_binder(false, "windowInfosReportedListener"))
            .collect::<R<Vec<_>>>()?;
        Ok(Self {
            focus_requests,
            window_infos_reported_listeners,
        })
    }
}
