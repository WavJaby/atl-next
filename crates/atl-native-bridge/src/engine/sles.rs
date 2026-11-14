// SPDX-License-Identifier: MIT
//! OpenSL ES over the proxy object tables (opensl-es plan 2.2 items 2-4): the guest holds guest proxies (objects.rs `GuestObj`), the host library (AOSP wilhelm, resolved as the binding table's `libOpenSLES.so`) holds the real interfaces.
//! The map: host interface + table -> guest proxy (callbacks, out-parameters), guest proxy -> host interface (validated by membership before word 1 is trusted). A proxy made for an object (`slCreateEngine`, `Create*`) or by its `GetInterface` is owned by that object and freed after the host `Destroy`. A proxy first met as a callback's caller is owned by nobody and kept: it is (table, host address), so a later interface at a reused address of the same table is the same proxy.
//! Specials (host functions bound into a table, as jni.rs `GetJavaVM`): `slCreateEngine` (a binding shim), Object `GetInterface` / `Destroy` / `RegisterCallback`, Engine `CreateOutputMix` / `CreateAudioPlayer`, `RegisterCallback` of BufferQueue, AndroidSimpleBufferQueue, Play and PrefetchStatus (guest callback -> down shell of kind `Down::Itf`).
//! Outside the proxy scope (plan section 3): refused with `SL_RESULT_FEATURE_UNSUPPORTED` and counted by name (atl-diag `unimplemented sles.<method>.<name>`), never a fake success.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::identity::Down;
use super::objects::{
    self, GuestObj, TableId, host_slot, sl_android_configuration, sl_android_simple_buffer_queue,
    sl_buffer_queue, sl_engine, sl_object, sl_output_mix, sl_play, sl_prefetch_status, sl_seek,
    sl_volume,
};
use super::{active, bindings};

/// The host library's soname, as the guest proxy's stubs name it (bindings.rs `Host::System`).
const LIB: &str = "libOpenSLES.so";

/// OpenSLES.h result codes.
const SL_RESULT_SUCCESS: u32 = 0;
const SL_RESULT_PARAMETER_INVALID: u32 = 2;
const SL_RESULT_FEATURE_UNSUPPORTED: u32 = 0x0C;
/// OpenSLES.h / OpenSLES_Android.h data locator types.
const SL_DATALOCATOR_OUTPUTMIX: u32 = 0x0000_0004;
const SL_DATALOCATOR_BUFFERQUEUE: u32 = 0x0000_0006;
const SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE: u32 = 0x8000_07BD;

/// The interfaces a guest may hold (plan section 3, proxy scope): `GetInterface` of any other IID is refused.
const SCOPE: [TableId; 10] = [
    sl_object::TABLE,
    sl_engine::TABLE,
    sl_output_mix::TABLE,
    sl_play::TABLE,
    sl_buffer_queue::TABLE,
    sl_android_simple_buffer_queue::TABLE,
    sl_volume::TABLE,
    sl_seek::TABLE,
    sl_android_configuration::TABLE,
    sl_prefetch_status::TABLE,
];

/// Callback shorties (jni-style, result first; the interface argument is implicit, dispatch.rs `down_params`): OpenSLES.h `slBufferQueueCallback` / `slAndroidSimpleBufferQueueCallback` (caller, context), `slPlayCallback` / `slPrefetchCallback` (caller, context, SLuint32 event), `slObjectCallback` (caller, context, event, result, param, pInterface).
const QUEUE_CALLBACK: &[u8] = b"VL";
const EVENT_CALLBACK: &[u8] = b"VLI";
const OBJECT_CALLBACK: &[u8] = b"VLIIIL";

/// OpenSLES.h `SLDataLocator_OutputMix`: the sink locator whose object `CreateAudioPlayer` rewrites (layout checked against the header at build time, build.rs `PROTOTYPE_HEADERS`).
#[repr(C)]
#[derive(Clone, Copy)]
struct OutputMixLocator {
    locator_type: u32,
    output_mix: usize,
}

/// OpenSLES.h `SLDataSink` / `SLDataSource`: a locator and a format.
#[repr(C)]
#[derive(Clone, Copy)]
struct DataEnds {
    locator: *const u32,
    format: usize,
}

// the Rust mirrors against the header's host view (build.rs `sl_layout_rs`; guest == host is checked there too)
const _: () = {
    use super::bindings::identity::{SL_DATA_SINK, SL_OUTPUT_MIX};
    use std::mem::{offset_of, size_of};
    assert!(size_of::<DataEnds>() as u64 == SL_DATA_SINK[0]);
    assert!(offset_of!(DataEnds, locator) as u64 == SL_DATA_SINK[1]);
    assert!(offset_of!(DataEnds, format) as u64 == SL_DATA_SINK[2]);
    assert!(size_of::<OutputMixLocator>() as u64 == SL_OUTPUT_MIX[0]);
    assert!(offset_of!(OutputMixLocator, locator_type) as u64 == SL_OUTPUT_MIX[1]);
    assert!(offset_of!(OutputMixLocator, output_mix) as u64 == SL_OUTPUT_MIX[2]);
};

/// One guest proxy: the host interface it stands for, its table, and the host object whose `Destroy` frees it (None: kept for the process).
struct Proxy {
    host: usize,
    table: TableId,
    owner: Option<usize>,
}

#[derive(Default)]
struct Map {
    by_host: HashMap<(usize, TableId), usize>,
    proxies: HashMap<usize, Proxy>,
}

static MAP: OnceLock<Mutex<Map>> = OnceLock::new();

fn map() -> std::sync::MutexGuard<'static, Map> {
    MAP.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The guest proxy of host interface `host` in table `t`, made on a miss with owner `owner`.
fn wrap(t: TableId, host: usize, owner: Option<usize>) -> usize {
    if host == 0 {
        return 0;
    }
    let mut m = map();
    if let Some(&p) = m.by_host.get(&(host, t)) {
        return p;
    }
    let functions = objects::functions(t, &specials(t), host);
    let p = Box::into_raw(Box::new(GuestObj { functions, host })) as usize;
    m.by_host.insert((host, t), p);
    m.proxies.insert(
        p,
        Proxy {
            host,
            table: t,
            owner,
        },
    );
    p
}

/// The host interface behind guest proxy `guest`, when it is one of ours (R-SL-5: membership before word 1).
fn host_of(guest: usize) -> Option<usize> {
    map().proxies.get(&guest).map(|p| p.host)
}

/// Frees every proxy owned by host object `object` (its own and its interfaces').
fn release(object: usize) {
    let mut m = map();
    let gone: Vec<usize> = m
        .proxies
        .iter()
        .filter(|(_, p)| p.owner == Some(object))
        .map(|(&g, _)| g)
        .collect();
    for g in gone {
        if let Some(p) = m.proxies.remove(&g) {
            m.by_host.remove(&(p.host, p.table));
            // SAFETY: made by `wrap` with Box::into_raw and removed from the map just now, so freed once.
            drop(unsafe { Box::from_raw(g as *mut GuestObj) });
        }
    }
}

/// The guest arguments of an SL callback (identity `Down::guest_args`): the caller interface of table `t` -> its proxy; an `slObjectCallback`'s 6th (`pInterface`, any table) -> its proxy when the guest has one, else 0 (the guest has not asked for that interface: plan section 3 refuses the dynamic interfaces that report it).
pub(super) fn down_args(t: TableId, first: &mut usize, sixth: &mut usize) {
    *first = wrap(t, *first, None);
    if t == sl_object::TABLE && *sixth != 0 {
        let m = map();
        *sixth = m
            .by_host
            .iter()
            .find(|((h, _), _)| *h == *sixth)
            .map_or(0, |(_, &p)| p);
    }
}

/// Live proxies owned by no object (first met as a callback's caller): never freed, so this is the leak, for the gates.
pub fn ownerless() -> usize {
    map().proxies.values().filter(|p| p.owner.is_none()).count()
}

/// Counts refusal `what` as the API `sles.<what>` atl-diag's `unimplemented` (the process's one counter family); answers `SL_RESULT_FEATURE_UNSUPPORTED`, a legal SL result.
fn refuse(what: &str) -> u32 {
    atl_diag::unimplemented_api!(&format!("sles.{what}"));
    SL_RESULT_FEATURE_UNSUPPORTED
}

/// OpenSLES.h `struct SLInterfaceID_`: 16 bytes, compared by content (wilhelm does too).
type IidBytes = [u8; 16];

/// Every SL IID of the host library: its bytes, its symbol name, its table.
fn iids() -> &'static [(IidBytes, &'static str, TableId)] {
    static IIDS: OnceLock<Vec<(IidBytes, &'static str, TableId)>> = OnceLock::new();
    IIDS.get_or_init(|| {
        objects::SL_IIDS
            .iter()
            .filter_map(|&(name, t)| {
                let var = bindings::system_symbol(LIB, name)?;
                // SAFETY: `SL_IID_<X>` is a `const SLInterfaceID` (a pointer to a 16-byte struct) in the host library, which stays loaded.
                let bytes = unsafe { *(*(var as *const *const IidBytes)) };
                Some((bytes, name, t))
            })
            .collect()
    })
}

/// The table of the interface `iid` names and its symbol, None when the host library has no such IID.
///
/// # Safety
/// `iid` is null or points at 16 readable bytes.
unsafe fn iid_table(iid: *const IidBytes) -> Option<(&'static str, TableId)> {
    if iid.is_null() {
        return None;
    }
    // SAFETY: caller contract.
    let bytes = unsafe { *iid };
    iids()
        .iter()
        .find(|(b, _, _)| *b == bytes)
        .map(|&(_, name, t)| (name, t))
}

/// Host function `slot` of host interface `host`, as `F`.
///
/// # Safety
/// `host` is a live host interface whose table has `slot`, of C type `F`.
unsafe fn host_fn<F: Copy>(host: usize, slot: usize) -> F {
    // SAFETY: caller contract; F is a function pointer type of pointer size.
    unsafe { std::mem::transmute_copy(&host_slot(host, slot)) }
}

/// `slCreateEngine` (the binding shim of `libOpenSLES.so:slCreateEngine`): the host engine, its object wrapped.
///
/// # Safety
/// As OpenSLES.h `slCreateEngine`: `out` is null or writable, the arrays hold their counts.
pub(super) unsafe extern "C" fn create_engine(
    out: *mut usize,
    num_options: u32,
    options: usize,
    num_interfaces: u32,
    ids: usize,
    required: usize,
) -> u32 {
    type F = unsafe extern "C" fn(*mut usize, u32, usize, u32, usize, usize) -> u32;
    let Some(f) = bindings::system_symbol(LIB, "slCreateEngine") else {
        return refuse("slCreateEngine.no-host-library");
    };
    // SAFETY: OpenSLES.h slCreateEngine; the guest's arguments as it passed them (the IIDs are host pointers: variable rows).
    let rc = unsafe {
        std::mem::transmute::<usize, F>(f)(out, num_options, options, num_interfaces, ids, required)
    };
    if rc == SL_RESULT_SUCCESS && !out.is_null() {
        // SAFETY: the host wrote its engine object there.
        unsafe { *out = wrap(sl_object::TABLE, *out, Some(*out)) };
    }
    rc
}

/// The host interface behind guest proxy `itf`; aborts by name for anything else (a forged interface, as dispatch.rs refuses a non-proxy).
fn host_itf(itf: usize, what: &str) -> usize {
    host_of(itf).unwrap_or_else(|| {
        super::unsupported(&format!(
            "OpenSL ES {what} on {itf:#x}, which is no guest proxy"
        ))
    })
}

/// Object `GetInterface(self, iid, pInterface)`: in-scope IIDs only; the out interface wrapped, owned by the object.
extern "C" fn get_interface(itf: usize, iid: *const IidBytes, out: *mut usize) -> u32 {
    let host = host_itf(itf, "GetInterface");
    // SAFETY: the guest passes an SLInterfaceID (null or a 16-byte struct).
    let found = unsafe { iid_table(iid) };
    let Some((_, t)) = found.filter(|(_, t)| SCOPE.contains(t)) else {
        let name = match found {
            Some((name, _)) => name.to_owned(),
            // SAFETY: as above; null was answered by `iid_table`.
            None if !iid.is_null() => hex(unsafe { &*iid }),
            None => "null".to_owned(),
        };
        return refuse(&format!("GetInterface.{name}"));
    };
    type F = unsafe extern "C" fn(usize, *const IidBytes, *mut usize) -> u32;
    // SAFETY: SLObjectItf_ GetInterface.
    let rc = unsafe { host_fn::<F>(host, sl_object::GetInterface)(host, iid, out) };
    if rc == SL_RESULT_SUCCESS && !out.is_null() {
        // SAFETY: the host wrote the interface there.
        unsafe { *out = wrap(t, *out, Some(host)) };
    }
    rc
}

/// An unnamed IID as its bytes.
fn hex(b: &IidBytes) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Object `Destroy(self)`: the host's, then every proxy the object owned.
extern "C" fn destroy(itf: usize) {
    let host = host_itf(itf, "Destroy");
    type F = unsafe extern "C" fn(usize);
    // SAFETY: SLObjectItf_ Destroy.
    unsafe { host_fn::<F>(host, sl_object::Destroy)(host) };
    release(host);
}

/// A guest callback as a host one: the down shell of kind `Down::Itf(t)` (0 stays 0: unregister). None when the shells are used up.
fn shell(cb: usize, t: TableId, shorty: &[u8]) -> Option<usize> {
    if cb == 0 {
        return Some(0);
    }
    active().down(cb, Down::Itf(t), Some(shorty)).ok()
}

/// `RegisterCallback(self, callback, context)` of table `t` at `slot`: the callback through a down shell, the context as it is.
fn register(itf: usize, t: TableId, slot: usize, shorty: &[u8], cb: usize, ctx: usize) -> u32 {
    let host = host_itf(itf, "RegisterCallback");
    let Some(cb) = shell(cb, t, shorty) else {
        return refuse(&format!(
            "{}.RegisterCallback.no-down-shell",
            objects::TABLES[t].0
        ));
    };
    type F = unsafe extern "C" fn(usize, usize, usize) -> u32;
    // SAFETY: the table's RegisterCallback(self, callback, context).
    unsafe { host_fn::<F>(host, slot)(host, cb, ctx) }
}

extern "C" fn object_register(itf: usize, cb: usize, ctx: usize) -> u32 {
    register(
        itf,
        sl_object::TABLE,
        sl_object::RegisterCallback,
        OBJECT_CALLBACK,
        cb,
        ctx,
    )
}
extern "C" fn buffer_queue_register(itf: usize, cb: usize, ctx: usize) -> u32 {
    register(
        itf,
        sl_buffer_queue::TABLE,
        sl_buffer_queue::RegisterCallback,
        QUEUE_CALLBACK,
        cb,
        ctx,
    )
}
extern "C" fn simple_buffer_queue_register(itf: usize, cb: usize, ctx: usize) -> u32 {
    let t = sl_android_simple_buffer_queue::TABLE;
    register(
        itf,
        t,
        sl_android_simple_buffer_queue::RegisterCallback,
        QUEUE_CALLBACK,
        cb,
        ctx,
    )
}
extern "C" fn play_register(itf: usize, cb: usize, ctx: usize) -> u32 {
    register(
        itf,
        sl_play::TABLE,
        sl_play::RegisterCallback,
        EVENT_CALLBACK,
        cb,
        ctx,
    )
}
extern "C" fn prefetch_register(itf: usize, cb: usize, ctx: usize) -> u32 {
    let t = sl_prefetch_status::TABLE;
    register(
        itf,
        t,
        sl_prefetch_status::RegisterCallback,
        EVENT_CALLBACK,
        cb,
        ctx,
    )
}

/// Engine `CreateOutputMix(self, pMix, numInterfaces, ids, required)`: the out object wrapped, owning itself.
extern "C" fn create_output_mix(
    itf: usize,
    out: *mut usize,
    n: u32,
    ids: usize,
    req: usize,
) -> u32 {
    let host = host_itf(itf, "CreateOutputMix");
    type F = unsafe extern "C" fn(usize, *mut usize, u32, usize, usize) -> u32;
    // SAFETY: SLEngineItf_ CreateOutputMix.
    let rc = unsafe { host_fn::<F>(host, sl_engine::CreateOutputMix)(host, out, n, ids, req) };
    if rc == SL_RESULT_SUCCESS && !out.is_null() {
        // SAFETY: the host wrote its object there.
        unsafe { *out = wrap(sl_object::TABLE, *out, Some(*out)) };
    }
    rc
}

/// The locator type at `ends.locator`, 0 for a null locator.
///
/// # Safety
/// `ends` is null or a guest `SLDataSource` / `SLDataSink` whose locator is null or starts with its SLuint32 type.
unsafe fn locator_type(ends: *const DataEnds) -> u32 {
    // SAFETY: caller contract.
    unsafe { ends.as_ref() }.map_or(0, |e| {
        // SAFETY: caller contract.
        unsafe { e.locator.as_ref() }.copied().unwrap_or(0)
    })
}

/// Engine `CreateAudioPlayer(self, pPlayer, pAudioSrc, pAudioSnk, numInterfaces, ids, required)`: buffer-queue sources and an OutputMix sink only; the sink's `outputMix` (a guest proxy) rewritten to its host object in a copy; the out object wrapped.
/// Break `sles_raw_sink`: the sink passes as the guest made it.
extern "C" fn create_audio_player(
    itf: usize,
    out: *mut usize,
    src: *const DataEnds,
    snk: *const DataEnds,
    n: u32,
    ids: usize,
    req: usize,
) -> u32 {
    let host = host_itf(itf, "CreateAudioPlayer");
    // SAFETY: the guest's SLDataSource / SLDataSink, as OpenSLES.h lays them out.
    let (source, sink) = unsafe { (locator_type(src), locator_type(snk)) };
    if source != SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE && source != SL_DATALOCATOR_BUFFERQUEUE {
        return refuse(&format!("CreateAudioPlayer.source-{source:#x}"));
    }
    if sink != SL_DATALOCATOR_OUTPUTMIX {
        return refuse(&format!("CreateAudioPlayer.sink-{sink:#x}"));
    }
    // SAFETY: `snk` is a sink whose locator is an SLDataLocator_OutputMix (type read above).
    let (mut ends, mut mix) = unsafe { (*snk, *ends_locator(snk)) };
    #[cfg(not(atl_nb_sles_raw_sink))]
    {
        let Some(h) = host_of(mix.output_mix) else {
            return SL_RESULT_PARAMETER_INVALID;
        };
        mix.output_mix = h;
    }
    ends.locator = (&raw const mix).cast();
    type F = unsafe extern "C" fn(
        usize,
        *mut usize,
        *const DataEnds,
        *const DataEnds,
        u32,
        usize,
        usize,
    ) -> u32;
    // SAFETY: SLEngineItf_ CreateAudioPlayer with the copied sink, live for the call.
    let rc = unsafe {
        host_fn::<F>(host, sl_engine::CreateAudioPlayer)(host, out, src, &ends, n, ids, req)
    };
    if rc == SL_RESULT_SUCCESS && !out.is_null() {
        // SAFETY: the host wrote its object there.
        unsafe { *out = wrap(sl_object::TABLE, *out, Some(*out)) };
    }
    rc
}

/// The OutputMix locator of sink `snk`.
///
/// # Safety
/// As `create_audio_player`'s read: `snk` and its locator are valid, the locator an OutputMix one.
unsafe fn ends_locator(snk: *const DataEnds) -> *const OutputMixLocator {
    // SAFETY: caller contract.
    unsafe { (*snk).locator.cast() }
}

/// A refusing special: answers FEATURE_UNSUPPORTED for its method, counted by name.
macro_rules! refuser {
    ($f:ident, $what:literal) => {
        extern "C" fn $f() -> u32 {
            refuse($what)
        }
    };
}
refuser!(create_led, "CreateLEDDevice");
refuser!(create_vibra, "CreateVibraDevice");
refuser!(create_recorder, "CreateAudioRecorder");
refuser!(create_midi, "CreateMidiPlayer");
refuser!(create_listener, "CreateListener");
refuser!(create_3d_group, "Create3DGroup");
refuser!(create_metadata, "CreateMetadataExtractor");
refuser!(create_extension, "CreateExtensionObject");
refuser!(device_change, "OutputMix.RegisterDeviceChangeCallback");
refuser!(java_proxy, "AndroidConfiguration.AcquireJavaProxy");

/// Table `t`'s specials: (slot, host function).
fn specials(t: TableId) -> Vec<(usize, usize)> {
    let f = |p: *const ()| p as usize;
    match t {
        sl_object::TABLE => vec![
            (sl_object::GetInterface, f(get_interface as *const ())),
            (sl_object::RegisterCallback, f(object_register as *const ())),
            (sl_object::Destroy, f(destroy as *const ())),
        ],
        sl_engine::TABLE => vec![
            (
                sl_engine::CreateOutputMix,
                f(create_output_mix as *const ()),
            ),
            (
                sl_engine::CreateAudioPlayer,
                f(create_audio_player as *const ()),
            ),
            (sl_engine::CreateLEDDevice, f(create_led as *const ())),
            (sl_engine::CreateVibraDevice, f(create_vibra as *const ())),
            (
                sl_engine::CreateAudioRecorder,
                f(create_recorder as *const ()),
            ),
            (sl_engine::CreateMidiPlayer, f(create_midi as *const ())),
            (sl_engine::CreateListener, f(create_listener as *const ())),
            (sl_engine::Create3DGroup, f(create_3d_group as *const ())),
            (
                sl_engine::CreateMetadataExtractor,
                f(create_metadata as *const ()),
            ),
            (
                sl_engine::CreateExtensionObject,
                f(create_extension as *const ()),
            ),
        ],
        sl_buffer_queue::TABLE => vec![(
            sl_buffer_queue::RegisterCallback,
            f(buffer_queue_register as *const ()),
        )],
        sl_android_simple_buffer_queue::TABLE => vec![(
            sl_android_simple_buffer_queue::RegisterCallback,
            f(simple_buffer_queue_register as *const ()),
        )],
        sl_play::TABLE => vec![(sl_play::RegisterCallback, f(play_register as *const ()))],
        sl_prefetch_status::TABLE => vec![(
            sl_prefetch_status::RegisterCallback,
            f(prefetch_register as *const ()),
        )],
        sl_output_mix::TABLE => vec![(
            sl_output_mix::RegisterDeviceChangeCallback,
            f(device_change as *const ()),
        )],
        sl_android_configuration::TABLE => vec![(
            sl_android_configuration::AcquireJavaProxy,
            f(java_proxy as *const ()),
        )],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every in-scope table's IID is a known SL IID (plan section 3's ten interfaces), and the callback registrations of the scope have a special.
    #[test]
    fn scope_tables_have_iids_and_callbacks() {
        for t in SCOPE {
            assert!(
                objects::SL_IIDS.iter().any(|&(_, i)| i == t),
                "{}",
                objects::TABLES[t].0
            );
            let slots = objects::TABLES[t].1;
            if let Some(k) = slots.iter().position(|s| *s == "RegisterCallback") {
                assert!(
                    specials(t).iter().any(|&(s, _)| s == k),
                    "{}",
                    objects::TABLES[t].0
                );
            }
        }
    }

    /// The Engine's ten Create* entries all have a special: two wrap, eight refuse (OpenSLES.h `SLEngineItf_` lists ten Create* methods).
    #[test]
    fn every_create_is_special() {
        let creates = sl_engine::SLOTS
            .iter()
            .filter(|s| s.starts_with("Create"))
            .count();
        assert_eq!(creates, 10);
        assert_eq!(specials(sl_engine::TABLE).len(), 10);
    }
}
