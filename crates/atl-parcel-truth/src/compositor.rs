// SPDX-License-Identifier: MIT
//! The Rust half of `xtask test compositor` (xtask/testdata/compositor/java/CompositorTruth.java): atl-compositor's codecs against libgui's C++ on host ART. Test-only, never shipped.
//! Java marshals SurfaceControl.Transactions with libgui's own writer; `checkTransaction` decodes those bytes (every one consumed), compares every value Java set, and re-encodes them byte-equal.
//! `rustTransaction` / `rustCompleted` / `rustRelease` go the other way: atl-compositor's bytes, replayed by Java into a real Parcel, are read by libgui's C++ readers.
//! A Java Parcel's data and object table are read through libbinder's C++ accessors (the NDK refuses to marshal a Parcel that holds objects).

use std::collections::BTreeMap;
use std::ffi::c_void;

use atl_compositor::callbacks::{
    AcquireTimeOrFence, FrameEventHistoryStats, ListenerStats, NO_RELEASE_CALLBACK, ReleaseBuffer,
    ReleaseCallbackId, SurfaceStats, TransactionStats,
};
use atl_compositor::parcel::{
    BINDER_TYPE_BINDER, BINDER_TYPE_FD, BINDER_TYPE_HANDLE, FLAT_OBJECT_SIZE, FdObject, FlatObject,
    ParcelError, Reader, StrongBinder, WireParcel,
};
use atl_compositor::state::{
    BlurRegion, BorderSettings, BufferData, BufferReleaseEndpoint, CallbackId, DisplayLuts,
    DisplayState, EdgeExtensionParameters, Fence, FloatRect, FocusRequest, HdrMetadata,
    InputApplicationInfo, InputWindowCommands, LayerState, ListenerCallbacks, LutEntry,
    NativeHandle, Rect, Region, SurfaceControlParcel, Transform2D, TrustedPresentationListener,
    TrustedPresentationThresholds, WindowInfo, buffer_data_change, what,
};
use atl_compositor::transaction::{TransactionState, UncacheBuffer};

use crate::{
    GET_OBJECT_ARRAY_ELEMENT, JObject, JniEnv, byte_array, bytes, java_string, last_error,
    set_error, slot, start_reg, string,
};

/// `Java_CompositorTruth_*` modes (CompositorTruth.java).
/// (mode 0, plain, is every value this does not name.)
const MODE_CORRUPT: i32 = 1;
const MODE_SWAP_EXPECTED: i32 = 2;

/// `android::AParcel` is opaque here; `AParcel_fromJavaParcel` and `AParcel_delete` are libbinder_ndk's stable C API, `AParcel_viewPlatformParcel` its platform one (C++ linkage).
#[repr(C)]
struct AParcel {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn AParcel_fromJavaParcel(env: JniEnv, parcel: JObject) -> *mut AParcel;
    fn AParcel_delete(parcel: *mut AParcel);
    /// `const android::Parcel* AParcel_viewPlatformParcel(const AParcel*)`.
    #[link_name = "_Z26AParcel_viewPlatformParcelPK7AParcel"]
    fn view_platform_parcel(p: *const AParcel) -> *const c_void;
    /// `const uint8_t* android::Parcel::data() const`; member functions take `this` first.
    #[link_name = "_ZNK7android6Parcel4dataEv"]
    fn parcel_data(this: *const c_void) -> *const u8;
    /// `size_t android::Parcel::dataSize() const`.
    #[link_name = "_ZNK7android6Parcel8dataSizeEv"]
    fn parcel_data_size(this: *const c_void) -> usize;
    /// `size_t android::Parcel::objectsCount() const`.
    #[link_name = "_ZNK7android6Parcel12objectsCountEv"]
    fn parcel_objects_count(this: *const c_void) -> usize;
}

/// The offsets of the objects in `data`, found by their type words: a listed object is a binder or handle with a non-zero binder word, or an fd. libbinder's own table is not exported, so the count it reports
/// (`objectsCount`) is checked against what the scan found.
fn scan_objects(data: &[u8], expected_count: usize) -> Result<Vec<usize>, String> {
    let mut offsets = Vec::new();
    let mut at = 0usize;
    while at + FLAT_OBJECT_SIZE <= data.len() {
        let ty = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
        let binder = u64::from_le_bytes(data[at + 8..at + 16].try_into().map_err(|_| "slice")?);
        let listed = match ty {
            BINDER_TYPE_FD => true,
            BINDER_TYPE_BINDER | BINDER_TYPE_HANDLE => binder != 0,
            _ => false,
        };
        if listed {
            offsets.push(at);
            at += FLAT_OBJECT_SIZE;
        } else {
            at += 4;
        }
    }
    if offsets.len() != expected_count {
        return Err(format!(
            "object scan found {} objects, libbinder says {expected_count}",
            offsets.len()
        ));
    }
    Ok(offsets)
}

/// The data and object table of the Java Parcel `parcel`, as a [`WireParcel`].
///
/// # Safety
/// `env` is the JNIEnv ART passed to this thread's native method; `parcel` a local reference to an android.os.Parcel that stays valid for the call.
unsafe fn read_java_parcel(env: JniEnv, parcel: JObject) -> Result<WireParcel, String> {
    // SAFETY: the caller's guarantees; the view is non-owning and deleted below
    let pa = unsafe { AParcel_fromJavaParcel(env, parcel) };
    if pa.is_null() {
        return Err("not an android.os.Parcel".to_owned());
    }
    // SAFETY: `pa` is a live AParcel; the platform Parcel it views lives as long as the Java Parcel, which the caller keeps; data() points at dataSize() bytes
    let result = unsafe {
        let p = view_platform_parcel(pa);
        let data = std::slice::from_raw_parts(parcel_data(p), parcel_data_size(p)).to_vec();
        let count = parcel_objects_count(p);
        scan_objects(&data, count)
            .and_then(|o| WireParcel::from_wire(data, o).map_err(|e| e.to_string()))
    };
    // SAFETY: created above, not used after
    unsafe { AParcel_delete(pa) };
    result
}

// ---- field table: every value a Java scenario sets, by name

fn put(m: &mut BTreeMap<String, i64>, k: String, v: i64) {
    m.insert(k, v);
}

fn bits(f: f32) -> i64 {
    i64::from(f.to_bits())
}

/// The decoded values of one layer state, named as CompositorTruth.java names them (`L<i>.<field>`).
fn layer_fields(i: usize, s: &LayerState, m: &mut BTreeMap<String, i64>) {
    let mut p = |name: &str, v: i64| put(m, format!("L{i}.{name}"), v);
    p("layerId", i64::from(s.layer_id));
    p("what", s.what as i64);
    p("x", bits(s.x));
    p("y", bits(s.y));
    p("z", i64::from(s.z));
    p("layerStack", i64::from(s.layer_stack));
    p("flags", i64::from(s.flags));
    p("mask", i64::from(s.mask));
    p("matrix.dsdx", bits(s.matrix_dsdx));
    p("matrix.dtdx", bits(s.matrix_dtdx));
    p("matrix.dtdy", bits(s.matrix_dtdy));
    p("matrix.dsdy", bits(s.matrix_dsdy));
    p("crop.left", bits(s.crop.left));
    p("crop.top", bits(s.crop.top));
    p("crop.right", bits(s.crop.right));
    p("crop.bottom", bits(s.crop.bottom));
    for (n, c) in ["r", "g", "b", "a"].iter().zip(s.color) {
        p(&format!("color.{n}"), bits(c));
    }
    p("cornerRadius", bits(s.corner_radius));
    p(
        "clientDrawnCornerRadius",
        bits(s.client_drawn_corner_radius),
    );
    p("backgroundBlurRadius", i64::from(s.background_blur_radius));
    p("shadowRadius", bits(s.shadow_radius));
    p("frameRate", bits(s.frame_rate));
    p(
        "frameRateCompatibility",
        i64::from(s.frame_rate_compatibility),
    );
    p(
        "changeFrameRateStrategy",
        i64::from(s.change_frame_rate_strategy),
    );
    p("frameRateCategory", i64::from(s.frame_rate_category));
    p(
        "frameRateCategorySmoothSwitchOnly",
        i64::from(s.frame_rate_category_smooth_switch_only),
    );
    p(
        "frameRateSelectionPriority",
        i64::from(s.frame_rate_selection_priority),
    );
    p(
        "frameRateSelectionStrategy",
        i64::from(s.frame_rate_selection_strategy),
    );
    p(
        "defaultFrameRateCompatibility",
        i64::from(s.default_frame_rate_compatibility),
    );
    p("metadata.count", s.metadata.len() as i64);
    if let Some((k, v)) = s.metadata.first() {
        p("metadata.key0", i64::from(*k));
        p("metadata.len0", v.len() as i64);
        p("metadata.byte0", v.first().map_or(-1, |b| i64::from(*b)));
    }
    for (n, v) in s.stretch_effect.iter().enumerate() {
        p(&format!("stretch.{n}"), bits(*v));
    }
    p("edge.left", i64::from(s.edge_extension.extend_left));
    p("edge.right", i64::from(s.edge_extension.extend_right));
    p("edge.top", i64::from(s.edge_extension.extend_top));
    p("edge.bottom", i64::from(s.edge_extension.extend_bottom));
    p("trustedOverlay", i64::from(s.trusted_overlay));
    p("dropInputMode", i64::from(s.drop_input_mode));
    p("destinationFrame.left", i64::from(s.destination_frame.left));
    p("destinationFrame.top", i64::from(s.destination_frame.top));
    p(
        "destinationFrame.right",
        i64::from(s.destination_frame.right),
    );
    p(
        "destinationFrame.bottom",
        i64::from(s.destination_frame.bottom),
    );
    p("blur.count", s.blur_regions.len() as i64);
    if let Some(b) = s.blur_regions.first() {
        p("blur0.radius", i64::from(b.blur_radius));
        p("blur0.alpha", bits(b.alpha));
        p("blur0.left", i64::from(b.left));
        p("blur0.top", i64::from(b.top));
        p("blur0.right", i64::from(b.right));
        p("blur0.bottom", i64::from(b.bottom));
        p("blur0.cornerRadiusTL", bits(b.corner_radius_tl));
        p("blur0.cornerRadiusTR", bits(b.corner_radius_tr));
        p("blur0.cornerRadiusBL", bits(b.corner_radius_bl));
        p("blur0.cornerRadiusBR", bits(b.corner_radius_br));
    }
    if let Some(parent) = &s.parent {
        p("parent.layerId", i64::from(parent.layer_id));
    }
    if let Some(rel) = &s.relative_layer {
        p("relative.layerId", i64::from(rel.layer_id));
    }
    p("bufferData.present", i64::from(s.buffer_data.is_some()));
    if let Some(b) = &s.buffer_data {
        p("bufferData.hasBuffer", i64::from(b.buffer.is_some()));
        p("bufferData.frameNumber", b.frame_number as i64);
    }
}

fn tx_fields(t: &TransactionState, suffix: &Suffix) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    put(&mut m, "tx.flags".into(), i64::from(t.flags));
    put(
        &mut m,
        "tx.desiredPresentTime".into(),
        t.desired_present_time,
    );
    put(
        &mut m,
        "tx.isAutoTimestamp".into(),
        i64::from(t.is_auto_timestamp),
    );
    put(&mut m, "tx.vsyncId".into(), t.frame_timeline_info.vsync_id);
    put(
        &mut m,
        "tx.composerStates".into(),
        t.composer_states.len() as i64,
    );
    put(
        &mut m,
        "tx.listenerSuffix".into(),
        suffix.entries.len() as i64,
    );
    for (i, s) in t.composer_states.iter().enumerate() {
        layer_fields(i, s, &mut m);
    }
    m
}

// ---- libgui's `Transaction::writeToParcel` suffix: the listeners with callbacks and surfaces

#[derive(Default)]
struct Suffix {
    entries: Vec<SuffixEntry>,
}

struct SuffixEntry {
    listener: StrongBinder,
    callback_ids: Vec<CallbackId>,
    surfaces: Vec<SurfaceControlParcel>,
}

impl Suffix {
    fn read(r: &mut Reader<'_>) -> Result<Self, ParcelError> {
        let n = r.read_count("listener entries")?;
        let entries = (0..n)
            .map(|_| {
                let listener = r.read_strong_binder(false, "suffix listener")?;
                let n_ids = r.read_count("callback ids")?;
                let callback_ids = (0..n_ids)
                    .map(|_| CallbackId::read_parcelable(r))
                    .collect::<Result<Vec<_>, _>>()?;
                let n_sc = r.read_count("surfaces")?;
                let surfaces = (0..n_sc)
                    .map(|_| SurfaceControlParcel::read(r))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(SuffixEntry {
                    listener,
                    callback_ids,
                    surfaces,
                })
            })
            .collect::<Result<Vec<_>, ParcelError>>()?;
        Ok(Self { entries })
    }

    fn write(&self, p: &mut WireParcel) -> Result<(), ParcelError> {
        p.write_len(self.entries.len(), "listener entries")?;
        for e in &self.entries {
            p.write_strong_binder(&e.listener);
            p.write_len(e.callback_ids.len(), "callback ids")?;
            e.callback_ids.iter().for_each(|c| c.write_parcelable(p));
            p.write_len(e.surfaces.len(), "surfaces")?;
            for s in &e.surfaces {
                s.write(p)?;
            }
        }
        Ok(())
    }
}

/// A Java `Transaction.writeToParcel` parcel: the int 1, libgui's TransactionState, the listener suffix; every byte consumed.
fn decode_java_transaction(p: &WireParcel) -> Result<(TransactionState, Suffix, usize), String> {
    let mut r = p.reader();
    let e = |e: ParcelError| e.to_string();
    if r.read_i32().map_err(e)? != 1 {
        return Err("a Java Transaction parcel starts with 1".to_owned());
    }
    let state = TransactionState::read(&mut r).map_err(e)?;
    let suffix_at = r.position();
    let suffix = Suffix::read(&mut r).map_err(e)?;
    r.finish().map_err(e)?;
    Ok((state, suffix, suffix_at))
}

fn encode_java_transaction(
    state: &TransactionState,
    suffix: &Suffix,
) -> Result<WireParcel, ParcelError> {
    let mut p = WireParcel::new();
    p.write_i32(1);
    state.write(&mut p)?;
    suffix.write(&mut p)?;
    Ok(p)
}

/// First difference between two dumps' bytes, for a failure message.
fn first_difference(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(i) => format!(
            "byte {i} of {} vs {}: {:#04x} vs {:#04x}",
            a.len(),
            b.len(),
            a[i],
            b[i]
        ),
        None => format!("lengths {} vs {}", a.len(), b.len()),
    }
}

fn check_transaction(dump: &[u8], expected: &[(String, i64)], mode: i32) -> Result<String, String> {
    let parcel = WireParcel::from_dump(dump).map_err(|e| e.to_string())?;
    let (state, suffix, _) =
        decode_java_transaction(&parcel).map_err(|e| format!("fail decode: {e}"))?;
    let have = tx_fields(&state, &suffix);
    let mut want: BTreeMap<String, i64> = expected.iter().cloned().collect();
    if mode == MODE_SWAP_EXPECTED {
        let (x, y) = (want.get("L0.x").copied(), want.get("L0.y").copied());
        if let (Some(x), Some(y)) = (x, y) {
            want.insert("L0.x".into(), y);
            want.insert("L0.y".into(), x);
        }
    }
    let mut bad = Vec::new();
    for (k, v) in &want {
        match have.get(k) {
            Some(got) if got == v => {}
            Some(got) => bad.push(format!("expected {k}={v}, decoded {got}")),
            None => bad.push(format!("expected {k}={v}, no such decoded field")),
        }
    }
    if !bad.is_empty() {
        return Ok(format!("fail {}", bad.join("; ")));
    }
    let mut again =
        encode_java_transaction(&state, &suffix).map_err(|e| format!("fail encode: {e}"))?;
    let mut again_dump = again.to_dump();
    if mode == MODE_CORRUPT {
        again_dump[4] ^= 1;
        again = WireParcel::from_dump(&again_dump).map_err(|e| e.to_string())?;
    }
    if again_dump != dump {
        return Ok(format!(
            "fail re-encoded bytes differ from libgui's at {}",
            first_difference(&again_dump, dump)
        ));
    }
    let _ = again;
    Ok(format!(
        "ok {} bytes, {} objects, {} named values checked, re-encoded byte-equal",
        parcel.data().len(),
        parcel.objects().len(),
        want.len()
    ))
}

// ---- Rust-built transactions

fn bn(c: u64) -> StrongBinder {
    StrongBinder {
        obj: FlatObject {
            ty: BINDER_TYPE_BINDER,
            flags: 0x113,
            binder: 0x7000 + c,
            cookie: 0x8000 + c,
        },
        stability: 12,
    }
}

fn fd(n: i32) -> FdObject {
    FdObject::new(n, true)
}

fn window_info(c: u64) -> WindowInfo {
    WindowInfo {
        token: bn(c),
        dispatching_timeout_ns: 5_000_000_000,
        id: 41,
        name: "truth-window".into(),
        layout_params_flags: 0x1234,
        layout_params_type: 2,
        frame: Rect {
            left: 1,
            top: 2,
            right: 3,
            bottom: 4,
        },
        content_width: 5,
        content_height: 6,
        surface_inset: 7,
        global_scale_factor: 1.5,
        alpha: 0.5,
        transform: Transform2D {
            dsdx: 1.0,
            dtdx: 0.25,
            tx: 3.0,
            dtdy: 0.5,
            dsdy: 2.0,
            ty: 4.0,
        },
        touch_occlusion_mode: 1,
        owner_pid: 1234,
        owner_uid: 10042,
        package_name: "com.example.truth".into(),
        input_config: 0x30,
        display_id: 2,
        application_info: InputApplicationInfo {
            token: bn(c + 1),
            name: "app".into(),
            dispatching_timeout_millis: 5000,
        },
        // libui validates regions on unflatten; one rect is always valid (its own bounds)
        touchable_region: Region {
            rects: vec![Rect {
                left: 0,
                top: 0,
                right: 10,
                bottom: 10,
            }],
        },
        replace_touchable_region_with_crop: true,
        touchable_region_crop_handle: bn(c + 2),
        window_token: bn(c + 3),
        focus_transfer_target: bn(c + 4),
        can_occlude_presentation: true,
        clone_layer_stack_transform: Some(Transform2D {
            dsdx: 2.0,
            dtdx: 0.0,
            tx: 1.0,
            dtdy: 0.0,
            dsdy: 2.0,
            ty: 1.0,
        }),
    }
}

/// A layer state with every field set to a value of its own, as far as libgui's reader can take it without a gralloc mapper: the buffer is the cached form (no GraphicBuffer).
fn full_layer(i: u64) -> LayerState {
    let mut s = LayerState::new();
    s.surface = bn(0x100 * (i + 1));
    s.layer_id = 20 + i as i32;
    s.what = what::E_BUFFER_CHANGED
        | what::E_POSITION_CHANGED
        | what::E_HAS_LISTENER_CALLBACKS_CHANGED
        | what::E_INPUT_INFO_CHANGED
        | what::E_BORDER_SETTINGS_CHANGED;
    s.x = 1.25;
    s.y = 2.5;
    s.z = 3;
    s.layer_stack = 4;
    s.flags = 0x102;
    s.mask = 0x103;
    s.matrix_dsdx = 1.5;
    s.matrix_dtdx = 0.125;
    s.matrix_dtdy = 0.375;
    s.matrix_dsdy = 2.5;
    s.crop = FloatRect {
        left: 1.0,
        top: 2.0,
        right: 33.0,
        bottom: 44.0,
    };
    s.relative_layer = Some(SurfaceControlParcel {
        client: bn(0x31),
        handle: bn(0x32),
        layer_id: 77,
        name: "relative".into(),
        transform_hint: 1,
        width: 2,
        height: 3,
        format: 4,
    });
    s.parent = Some(SurfaceControlParcel {
        client: bn(0x41),
        handle: bn(0x42),
        layer_id: 78,
        name: "parent \u{e9}\u{1f600}".into(),
        transform_hint: 5,
        width: 6,
        height: 7,
        format: 8,
    });
    s.color = [0.5, 0.25, 0.75, 0.125];
    s.window_info = Some(window_info(0x500));
    s.transparent_region = Region {
        rects: vec![Rect {
            left: 6,
            top: 7,
            right: 8,
            bottom: 9,
        }],
    };
    s.buffer_transform = 5;
    s.transform_to_display_inverse = true;
    s.dataspace = 0x8c0000;
    s.hdr_metadata = HdrMetadata {
        valid_types: 7,
        smpte2086: Some([7; 40]),
        cta861_3: Some([9; 8]),
        hdr10plus: Some(vec![1, 2, 3]),
    };
    s.surface_damage_region = Region {
        rects: vec![Rect {
            left: 3,
            top: 3,
            right: 14,
            bottom: 24,
        }],
    };
    s.api = 2;
    s.sideband_stream = Some(NativeHandle {
        fds: vec![fd(3)],
        ints: vec![11, 12],
    });
    for (n, v) in s.color_transform.iter_mut().enumerate() {
        *v = n as f32 * 0.5 + 0.25;
    }
    s.corner_radius = 4.5;
    s.client_drawn_corner_radius = 2.5;
    s.background_blur_radius = 9;
    // one entry: libgui keeps metadata in an unordered_map, so the order of several is its hash order, not the reader's
    s.metadata = vec![(9, vec![1, 2, 3])];
    s.bg_color = [0.0625, 0.125, 0.1875, 0.25];
    s.bg_color_dataspace = 0x20000;
    s.color_space_agnostic = true;
    s.listeners = vec![
        ListenerCallbacks {
            listener: bn(0x61),
            callback_ids: vec![CallbackId { id: 1, ty: 0 }, CallbackId { id: 2, ty: 1 }],
        },
        ListenerCallbacks {
            listener: bn(0x62),
            callback_ids: vec![],
        },
    ];
    s.shadow_radius = 6.5;
    s.border_settings = BorderSettings {
        stroke_width: 1.5,
        color: 0x11223344,
    };
    s.frame_rate_selection_priority = 12;
    s.frame_rate = 59.5;
    s.frame_rate_compatibility = 1;
    s.change_frame_rate_strategy = 1;
    s.default_frame_rate_compatibility = 2;
    s.frame_rate_category = 3;
    s.frame_rate_category_smooth_switch_only = true;
    s.frame_rate_selection_strategy = 1;
    s.fixed_transform_hint = 4;
    s.auto_refresh = true;
    s.dimming_enabled = false;
    s.blur_regions = vec![
        BlurRegion {
            blur_radius: 20,
            corner_radius_tl: 1.0,
            corner_radius_tr: 2.0,
            corner_radius_bl: 3.0,
            corner_radius_br: 4.0,
            alpha: 0.5,
            left: 5,
            top: 6,
            right: 7,
            bottom: 8,
        },
        BlurRegion {
            blur_radius: 21,
            corner_radius_tl: 1.5,
            corner_radius_tr: 2.5,
            corner_radius_bl: 3.5,
            corner_radius_br: 4.5,
            alpha: 0.25,
            left: 15,
            top: 16,
            right: 17,
            bottom: 18,
        },
    ];
    s.stretch_effect = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
    s.edge_extension = EdgeExtensionParameters {
        extend_right: true,
        extend_left: false,
        extend_top: true,
        extend_bottom: false,
    };
    s.buffer_crop = Rect {
        left: 1,
        top: 2,
        right: 3,
        bottom: 4,
    };
    s.destination_frame = Rect {
        left: 5,
        top: 6,
        right: 7,
        bottom: 8,
    };
    s.trusted_overlay = 2;
    s.drop_input_mode = 1;
    s.buffer_data = Some(BufferData {
        flags: buffer_data_change::CACHED_BUFFER_CHANGED
            | buffer_data_change::FRAME_NUMBER_CHANGED
            | buffer_data_change::FENCE_CHANGED,
        buffer: None,
        acquire_fence: None,
        frame_number: 99,
        release_buffer_listener: bn(0x71),
        release_buffer_endpoint: bn(0x72),
        cached_buffer_token: bn(0x73),
        cached_buffer_id: 0xAABB_0000_0042,
        has_barrier: true,
        barrier_frame_number: 98,
        producer_id: 3,
        dequeue_time: 123_456_789,
    });
    s.trusted_presentation_thresholds = TrustedPresentationThresholds {
        min_alpha: 0.5,
        min_fraction_rendered: 0.75,
        stability_requirement_ms: 100,
    };
    s.trusted_presentation_listener = TrustedPresentationListener {
        callback_interface: bn(0x81),
        callback_id: 7,
    };
    s.current_hdr_sdr_ratio = 1.5;
    s.desired_hdr_sdr_ratio = 2.5;
    s.caching_hint = 0;
    s.buffer_release_channel = Some(BufferReleaseEndpoint {
        name: "truth-channel".into(),
        fd: fd(4),
    });
    s.picture_profile_handle = 0x1_0000_0002;
    s.app_content_priority = 5;
    s.luts = Some(DisplayLuts {
        fd: fd(5),
        offsets: vec![0, 16, 32],
        lut_properties: vec![
            LutEntry {
                dimension: 1,
                size: 8,
                sampling_key: 0,
            },
            LutEntry {
                dimension: 3,
                size: 17,
                sampling_key: 1,
            },
        ],
    });
    s
}

fn full_transaction() -> TransactionState {
    let mut t = TransactionState::new();
    t.id = 0x1_0000_0007;
    t.flags = 0x20;
    t.desired_present_time = 424_242;
    t.is_auto_timestamp = false;
    t.frame_timeline_info.vsync_id = 31;
    t.frame_timeline_info.input_event_id = 32;
    t.frame_timeline_info.start_time_nanos = 33;
    t.frame_timeline_info.use_for_refresh_rate_selection = true;
    t.frame_timeline_info.skipped_frame_vsync_id = 34;
    t.frame_timeline_info.skipped_frame_start_time_nanos = 35;
    t.apply_token = bn(0x901);
    t.may_contain_buffer = true;
    t.log_call_points = true;
    t.display_states = vec![
        DisplayState {
            token: bn(0xa01),
            surface: bn(0xa02),
            what: 0x1f,
            flags: 3,
            layer_stack: 4,
            orientation: 1,
            layer_stack_space_rect: Rect {
                left: 1,
                top: 2,
                right: 3,
                bottom: 4,
            },
            oriented_display_space_rect: Rect {
                left: 5,
                top: 6,
                right: 7,
                bottom: 8,
            },
            width: 1920,
            height: 1080,
        },
        DisplayState {
            token: bn(0xa03),
            surface: StrongBinder::NULL,
            what: 2,
            flags: 0,
            layer_stack: 9,
            orientation: 0,
            layer_stack_space_rect: Rect::INVALID,
            oriented_display_space_rect: Rect::INVALID,
            width: 0,
            height: 0,
        },
    ];
    t.composer_states = vec![full_layer(0), full_layer(1)];
    t.input_window_commands = InputWindowCommands {
        focus_requests: vec![
            FocusRequest {
                token: bn(0xb01),
                window_name: "focus-1".into(),
                timestamp: 111,
                display_id: 0,
            },
            FocusRequest {
                token: StrongBinder::NULL,
                window_name: String::new(),
                timestamp: 222,
                display_id: 1,
            },
        ],
        // one: libgui holds these in an unordered_set, so the order of several is its hash order
        window_infos_reported_listeners: vec![bn(0xb02)],
    };
    t.uncache_buffers = vec![
        UncacheBuffer {
            token: bn(0xc01),
            id: 5,
        },
        UncacheBuffer {
            token: bn(0xc02),
            id: 6,
        },
    ];
    t.merged_transaction_ids = vec![5, 6, 7];
    t.has_listener_callbacks = true;
    t.listener_callbacks = vec![
        ListenerCallbacks {
            listener: bn(0xd01),
            callback_ids: vec![CallbackId { id: 9, ty: 0 }],
        },
        ListenerCallbacks {
            listener: bn(0xd02),
            callback_ids: vec![],
        },
    ];
    t
}

/// Java's Transaction parcel for `full_transaction` (int 1, state, empty listener suffix); scenario 1 drops the high half of the transaction id, shifting every field after it.
fn rust_transaction(scenario: i32) -> Result<Vec<u8>, String> {
    let p = encode_java_transaction(&full_transaction(), &Suffix::default())
        .map_err(|e| e.to_string())?;
    match scenario {
        0 => Ok(p.to_dump()),
        1 => {
            let drop_at = 4 + 4; // after the leading 1 and the id's low word
            let (mut data, objects) = p.into_parts();
            data.drain(drop_at..drop_at + 4);
            let objects = objects
                .into_iter()
                .map(|o| if o > drop_at { o - 4 } else { o })
                .collect();
            Ok(WireParcel::from_wire(data, objects)
                .map_err(|e| e.to_string())?
                .to_dump())
        }
        n => Err(format!("no scenario {n}")),
    }
}

/// Equal data and object offsets, except the fd number inside fd objects (a fresh descriptor per dup).
fn compare_dumps(a: &[u8], b: &[u8]) -> Result<String, String> {
    let (pa, pb) = (
        WireParcel::from_dump(a).map_err(|e| e.to_string())?,
        WireParcel::from_dump(b).map_err(|e| e.to_string())?,
    );
    if pa.objects() != pb.objects() {
        let only = |x: &WireParcel, y: &WireParcel| -> Vec<String> {
            x.objects()
                .iter()
                .filter(|o| !y.objects().contains(o))
                .map(|&o| {
                    let d = x.data();
                    let ty = u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
                    format!("{o}:{ty:#x}")
                })
                .collect()
        };
        return Ok(format!(
            "object offsets differ: only in the input {:?}; only in libgui's output {:?} (offset:type)",
            only(&pa, &pb),
            only(&pb, &pa)
        ));
    }
    if pa.data().len() != pb.data().len() {
        return Ok(format!(
            "data lengths differ: {} vs {}",
            pa.data().len(),
            pb.data().len()
        ));
    }
    let mut masked = vec![false; pa.data().len()];
    for &o in pa.objects() {
        let ty = u32::from_le_bytes([
            pa.data()[o],
            pa.data()[o + 1],
            pa.data()[o + 2],
            pa.data()[o + 3],
        ]);
        if ty == BINDER_TYPE_FD {
            masked[o + 8..o + 12].fill(true);
        }
    }
    match pa
        .data()
        .iter()
        .zip(pb.data())
        .enumerate()
        .find(|(i, (x, y))| x != y && !masked[*i])
    {
        Some((i, (x, y))) => {
            // where in the object table the difference sits, to name the field by counting the writer's objects
            let nth = pa.objects().iter().rposition(|&o| o <= i);
            let at = nth.map_or(String::from("before every object"), |n| {
                format!(
                    "object #{n} at {} + {}",
                    pa.objects()[n],
                    i - pa.objects()[n]
                )
            });
            Ok(format!(
                "data differ at byte {i}: {x:#04x} vs {y:#04x} ({at}); input words {:02x?} vs {:02x?}",
                &pa.data()[i.saturating_sub(4)..(i + 12).min(pa.data().len())],
                &pb.data()[i.saturating_sub(4)..(i + 12).min(pb.data().len())]
            ))
        }
        None => Ok("ok".to_owned()),
    }
}

fn completed_call(tx_dump: &[u8], latch: i64, frame: u64) -> Result<Vec<u8>, String> {
    let parcel = WireParcel::from_dump(tx_dump).map_err(|e| e.to_string())?;
    let (state, suffix, _) = decode_java_transaction(&parcel)?;
    let surface = state
        .composer_states
        .first()
        .ok_or("the transaction has no layer state")?
        .surface;
    let callback_ids: Vec<CallbackId> = suffix
        .entries
        .iter()
        .flat_map(|e| e.callback_ids.iter().copied())
        .collect();
    if callback_ids.is_empty() {
        return Err("the transaction registered no callback".to_owned());
    }
    let stats = ListenerStats {
        transaction_stats: vec![TransactionStats {
            callback_ids,
            latch_time: latch,
            present_fence: None,
            surface_stats: vec![SurfaceStats {
                surface_control: surface,
                acquire_time_or_fence: AcquireTimeOrFence::Time(100),
                previous_release_fence: None,
                transform_hint: Some(0),
                current_max_acquired_buffer_count: 2,
                event_stats: FrameEventHistoryStats {
                    frame_number: frame,
                    previous_frame_number: frame.saturating_sub(1),
                    gpu_composition_done_fence: None,
                    deadline: 1,
                    interval: 2,
                    present_latency: 3,
                    refresh_start_time: 4,
                    dequeue_ready_time: 5,
                },
                previous_release_callback_id: NO_RELEASE_CALLBACK,
            }],
        }],
    };
    Ok(stats.encode_call().map_err(|e| e.to_string())?.to_dump())
}

// ---- JNI entry points

/// Runs `f`, turning its error into a recorded one and a null result.
fn or_null(env: JniEnv, r: Result<Vec<u8>, String>) -> JObject {
    match r {
        // SAFETY: ART's JNIEnv of this native method
        Ok(b) => unsafe { byte_array(env, &b) },
        Err(e) => {
            set_error(e);
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_startReg(env: JniEnv, _class: JObject) -> i32 {
    // SAFETY: the caller's guarantee
    unsafe { start_reg(env) }
}

/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_lastError(env: JniEnv, _class: JObject) -> JObject {
    // SAFETY: ART's JNIEnv
    unsafe { java_string(env, &last_error()) }
}

/// `static native byte[] dump(Parcel p)`.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_dump(
    env: JniEnv,
    _class: JObject,
    parcel: JObject,
) -> JObject {
    // SAFETY: ART's arguments to this native method
    let r = unsafe { read_java_parcel(env, parcel) }.map(|p| p.to_dump());
    or_null(env, r)
}

/// `static native String checkTransaction(byte[] dump, String[] expected, int mode)`.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_checkTransaction(
    env: JniEnv,
    _class: JObject,
    dump: JObject,
    expected: JObject,
    mode: i32,
) -> JObject {
    // SAFETY: ART's arguments to this native method
    let (dump, pairs) = unsafe { (bytes(env, dump), string_pairs(env, expected)) };
    let result = match (dump, pairs) {
        (Some(d), Some(p)) => {
            check_transaction(&d, &p, mode).unwrap_or_else(|e| format!("fail {e}"))
        }
        _ => "fail null argument".to_owned(),
    };
    // SAFETY: ART's JNIEnv
    unsafe { java_string(env, &result) }
}

/// A `String[]` of name, decimal value pairs.
///
/// # Safety
/// `env` as for `slot`; `array` a local reference to a String[] or null.
unsafe fn string_pairs(env: JniEnv, array: JObject) -> Option<Vec<(String, i64)>> {
    if array.is_null() {
        return None;
    }
    // SAFETY: slot 171 is GetArrayLength, 173 GetObjectArrayElement, of these signatures
    unsafe {
        let len: unsafe extern "C" fn(JniEnv, JObject) -> i32 = std::mem::transmute(slot(env, 171));
        let get: unsafe extern "C" fn(JniEnv, JObject, i32) -> JObject =
            std::mem::transmute(slot(env, GET_OBJECT_ARRAY_ELEMENT));
        let n = len(env, array);
        let mut out = Vec::new();
        let mut i = 0;
        while i + 1 < n {
            let name = string(env, get(env, array, i))?;
            let value = string(env, get(env, array, i + 1))?.parse::<i64>().ok()?;
            out.push((name, value));
            i += 2;
        }
        Some(out)
    }
}

/// `static native byte[] rustTransaction(int scenario)`.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_rustTransaction(
    env: JniEnv,
    _class: JObject,
    scenario: i32,
) -> JObject {
    or_null(env, rust_transaction(scenario))
}

/// `static native String compareDumps(byte[] a, byte[] b)`.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_compareDumps(
    env: JniEnv,
    _class: JObject,
    a: JObject,
    b: JObject,
) -> JObject {
    // SAFETY: ART's arguments to this native method
    let (a, b) = unsafe { (bytes(env, a), bytes(env, b)) };
    let result = match (a, b) {
        (Some(a), Some(b)) => {
            compare_dumps(&a, &b).unwrap_or_else(|e| format!("cannot compare: {e}"))
        }
        _ => "null argument".to_owned(),
    };
    // SAFETY: ART's JNIEnv
    unsafe { java_string(env, &result) }
}

/// `static native int suffixOffset(byte[] dump)`.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_suffixOffset(
    env: JniEnv,
    _class: JObject,
    dump: JObject,
) -> i32 {
    // SAFETY: ART's argument to this native method
    let Some(d) = (unsafe { bytes(env, dump) }) else {
        set_error("null dump".to_owned());
        return -1;
    };
    match WireParcel::from_dump(&d)
        .map_err(|e| e.to_string())
        .and_then(|p| decode_java_transaction(&p))
    {
        Ok((_, _, at)) => i32::try_from(at).unwrap_or(-1),
        Err(e) => {
            set_error(e);
            -1
        }
    }
}

/// `static native byte[] rustCompleted(byte[] txDump, long latchTime, long frameNumber)`.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_rustCompleted(
    env: JniEnv,
    _class: JObject,
    dump: JObject,
    latch: i64,
    frame: i64,
) -> JObject {
    // SAFETY: ART's argument to this native method
    let d = unsafe { bytes(env, dump) };
    let r = d
        .ok_or_else(|| "null dump".to_owned())
        .and_then(|d| completed_call(&d, latch, frame as u64));
    or_null(env, r)
}

/// `static native byte[] rustRelease(long bufferId, long frameNumber, int maxAcquired)`.
///
/// # Safety
/// Called by ART as that native method.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_CompositorTruth_rustRelease(
    env: JniEnv,
    _class: JObject,
    buffer_id: i64,
    frame: i64,
    max: i32,
) -> JObject {
    let call = ReleaseBuffer {
        callback_id: ReleaseCallbackId {
            buffer_id: buffer_id as u64,
            frame_number: frame as u64,
        },
        release_fence: Fence(None),
        current_max_acquired_buffer_count: max as u32,
    };
    or_null(
        env,
        call.encode_call()
            .map(|p| p.to_dump())
            .map_err(|e| e.to_string()),
    )
}
