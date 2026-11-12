// SPDX-License-Identifier: MIT
//! Recorder OpenSL ES library for gate `sles-bridge` (opensl-es plan slice O1): a host `libOpenSLES.so` stand-in that records what the bridge hands it. Test-only: a cargo example (cdylib), never staged by `xtask native`; the harness copies it to its bwrap `/system/lib64/libOpenSLES.so`.
//! Objects: engine (Object + Engine), output mix (Object + OutputMix), audio player (Object + Play + AndroidSimpleBufferQueue + BufferQueue + Volume). An interface is the address of one `*const Slot` field of its object (OpenSLES.h: `SLxxxItf` = `const struct SLxxxItf_ * const *`).
//! Every slot has one C type here (`Slot`: seven word arguments, SLresult): SysV passes the first six in registers and the caller cleans the stack, so a method taking fewer arguments reads garbage only in words it ignores, and a `void` method's caller ignores eax.
//! Records: each call's arg 0 (counted as foreign when it is no interface of a live recorder object), the last `CreateAudioPlayer` sink's `outputMix`, buffer-queue callbacks fired on the recorder's own pthread and `Enqueue` calls re-entered from it. The test reads them through the `rec_*` exports.
#![allow(
    clippy::missing_safety_doc,
    reason = "C ABI entries called by the bridge and the gate, as OpenSLES.h defines them"
)]

use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

type Slot = unsafe extern "C" fn(usize, usize, usize, usize, usize, usize, usize) -> u32;

/// OpenSLES.h result codes and constants.
const SUCCESS: u32 = 0;
const PARAMETER_INVALID: u32 = 2;
const FEATURE_UNSUPPORTED: u32 = 0x0C;
const PLAYSTATE_PLAYING: u32 = 3;

/// OpenSLES.h `struct SLInterfaceID_`.
#[repr(C)]
#[derive(PartialEq, Eq)]
pub struct IidData {
    time_low: u32,
    time_mid: u16,
    time_hi_and_version: u16,
    clock_seq: u16,
    node: [u8; 6],
}

/// `const SLInterfaceID`: a pointer to the IID's data.
#[repr(transparent)]
pub struct Iid(&'static IidData);

/// wilhelm `src/OpenSLES_IID.cpp` values (node shared by every Khronos IID).
const NODE: [u8; 6] = [0x00, 0x02, 0xa5, 0xd5, 0xc5, 0x1b];
const fn iid(time_low: u32, time_mid: u16, time_hi_and_version: u16, clock_seq: u16) -> IidData {
    IidData {
        time_low,
        time_mid,
        time_hi_and_version,
        clock_seq,
        node: NODE,
    }
}
static IID_OBJECT: IidData = iid(0x7921_6360, 0xddd7, 0x11db, 0xac16);
static IID_ENGINE: IidData = iid(0x8d97_c260, 0xddd4, 0x11db, 0x958f);
static IID_OUTPUTMIX: IidData = iid(0x9775_0f60, 0xddd7, 0x11db, 0x92b1);
static IID_PLAY: IidData = iid(0xef0b_d9c0, 0xddd7, 0x11db, 0xbf49);
static IID_BUFFERQUEUE: IidData = iid(0x2bc9_9cc0, 0xddd4, 0x11db, 0x8d99);
static IID_VOLUME: IidData = iid(0x09e8_ede0, 0xddde, 0x11db, 0xb4f6);
static IID_ANDROIDSIMPLEBUFFERQUEUE: IidData = iid(0x198e_4940, 0xc5d7, 0x11df, 0xa2a6);

#[unsafe(no_mangle)]
pub static SL_IID_OBJECT: Iid = Iid(&IID_OBJECT);
#[unsafe(no_mangle)]
pub static SL_IID_ENGINE: Iid = Iid(&IID_ENGINE);
#[unsafe(no_mangle)]
pub static SL_IID_OUTPUTMIX: Iid = Iid(&IID_OUTPUTMIX);
#[unsafe(no_mangle)]
pub static SL_IID_PLAY: Iid = Iid(&IID_PLAY);
#[unsafe(no_mangle)]
pub static SL_IID_BUFFERQUEUE: Iid = Iid(&IID_BUFFERQUEUE);
#[unsafe(no_mangle)]
pub static SL_IID_VOLUME: Iid = Iid(&IID_VOLUME);
#[unsafe(no_mangle)]
pub static SL_IID_ANDROIDSIMPLEBUFFERQUEUE: Iid = Iid(&IID_ANDROIDSIMPLEBUFFERQUEUE);

/// Interface field index in [`Obj::itfs`] per IID; the object's own interface is field 0.
const OBJECT: usize = 0;
const SECOND: usize = 1;
const SIMPLE_QUEUE: usize = 2;
const QUEUE: usize = 3;
const VOLUME: usize = 4;
const FIELDS: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Engine,
    Mix,
    Player,
}

/// One recorder object: its interface fields (null where it has none) and its player state.
#[repr(C)]
struct Obj {
    itfs: [*const Slot; FIELDS],
    kind: Kind,
    queue_cb: usize,
    queue_ctx: usize,
    queued: u32,
}

/// What the recorder saw; one lock, never held across a callback.
struct Rec {
    /// Boxed: an interface is an address inside its object, so objects must not move when the Vec grows.
    #[allow(clippy::vec_box, reason = "stable object addresses, see above")]
    objs: Vec<Box<Obj>>,
    calls: u64,
    foreign: u64,
    engine: usize,
    realize_engine_arg0: usize,
    mix: usize,
    sink_mix: usize,
    /// Callbacks to fire per `SetPlayState(PLAYING)` (`rec_set_callbacks`).
    callbacks: u32,
    fired: u32,
    reentered: u32,
    underruns: u32,
    thread: Option<JoinHandle<()>>,
}

// SAFETY: the raw vtable pointers in `Obj` point at statics.
unsafe impl Send for Rec {}

static REC: Mutex<Rec> = Mutex::new(Rec {
    objs: Vec::new(),
    calls: 0,
    foreign: 0,
    engine: 0,
    realize_engine_arg0: 0,
    mix: 0,
    sink_mix: 0,
    callbacks: 0,
    fired: 0,
    reentered: 0,
    underruns: 0,
    thread: None,
});

fn rec() -> MutexGuard<'static, Rec> {
    REC.lock().unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    static ON_RECORDER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Rec {
    /// The object owning interface `itf` and the field `itf` is; counts the call, and a foreign arg 0.
    fn seen(&mut self, itf: usize) -> Option<(usize, usize)> {
        self.calls += 1;
        let hit = self.objs.iter().enumerate().find_map(|(i, o)| {
            let base = o.itfs.as_ptr() as usize;
            let field = itf.checked_sub(base)? / size_of::<*const Slot>();
            (itf >= base
                && field < FIELDS
                && itf.is_multiple_of(size_of::<usize>())
                && !o.itfs[field].is_null())
            .then_some((i, field))
        });
        if hit.is_none() {
            self.foreign += 1;
        }
        hit
    }

    fn add(&mut self, kind: Kind, itfs: [*const Slot; FIELDS]) -> usize {
        let o = Box::new(Obj {
            itfs,
            kind,
            queue_cb: 0,
            queue_ctx: 0,
            queued: 0,
        });
        let itf = o.itfs.as_ptr() as usize;
        self.objs.push(o);
        itf
    }
}

unsafe extern "C" fn nop(
    a0: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    rec().seen(a0);
    SUCCESS
}

unsafe extern "C" fn realize(
    a0: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    let mut r = rec();
    r.seen(a0);
    if a0 == r.engine && r.realize_engine_arg0 == 0 {
        r.realize_engine_arg0 = a0;
    }
    SUCCESS
}

/// Object `GetInterface(self, iid, pInterface)`: the IIDs of the object's kind.
unsafe extern "C" fn get_interface(
    a0: usize,
    iid: usize,
    out: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    let mut r = rec();
    let Some((i, _)) = r.seen(a0) else {
        return PARAMETER_INVALID;
    };
    if iid == 0 || out == 0 {
        return PARAMETER_INVALID;
    }
    // SAFETY: OpenSLES.h: `iid` points at an SLInterfaceID_.
    let want = unsafe { &*(iid as *const IidData) };
    let o = &r.objs[i];
    let field = match o.kind {
        _ if *want == IID_OBJECT => Some(OBJECT),
        Kind::Engine if *want == IID_ENGINE => Some(SECOND),
        Kind::Mix if *want == IID_OUTPUTMIX => Some(SECOND),
        Kind::Player if *want == IID_PLAY => Some(SECOND),
        Kind::Player if *want == IID_ANDROIDSIMPLEBUFFERQUEUE => Some(SIMPLE_QUEUE),
        Kind::Player if *want == IID_BUFFERQUEUE => Some(QUEUE),
        Kind::Player if *want == IID_VOLUME => Some(VOLUME),
        _ => None,
    };
    let Some(field) = field else {
        return FEATURE_UNSUPPORTED;
    };
    let itf = &raw const o.itfs[field] as usize;
    // SAFETY: OpenSLES.h: `pInterface` is writable.
    unsafe { *(out as *mut usize) = itf };
    SUCCESS
}

/// Object `Destroy(self)`: the object is freed (a later object may reuse its address, as wilhelm's may).
unsafe extern "C" fn destroy(
    a0: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    let mut r = rec();
    if let Some((i, _)) = r.seen(a0) {
        r.objs.swap_remove(i);
    }
    SUCCESS
}

/// Engine `CreateOutputMix(self, pMix, numInterfaces, ids, required)`.
unsafe extern "C" fn create_output_mix(
    a0: usize,
    out: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    let mut r = rec();
    if r.seen(a0).is_none() || out == 0 {
        return PARAMETER_INVALID;
    }
    let itf = r.add(
        Kind::Mix,
        [OBJECT_VT.as_ptr(), MIX_VT.as_ptr(), null(), null(), null()],
    );
    r.mix = itf;
    // SAFETY: OpenSLES.h: `pMix` is writable.
    unsafe { *(out as *mut usize) = itf };
    SUCCESS
}

/// Engine `CreateAudioPlayer(self, pPlayer, pAudioSrc, pAudioSnk, ...)`: records the sink locator's `outputMix` (OpenSLES.h `SLDataSink { void *pLocator; void *pFormat; }`, `SLDataLocator_OutputMix { SLuint32 locatorType; SLObjectItf outputMix; }`).
unsafe extern "C" fn create_audio_player(
    a0: usize,
    out: usize,
    _src: usize,
    snk: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    let mut r = rec();
    if r.seen(a0).is_none() || out == 0 || snk == 0 {
        return PARAMETER_INVALID;
    }
    // SAFETY: OpenSLES.h layouts above; the bridge passes an OutputMix sink.
    r.sink_mix = unsafe { *((*(snk as *const usize) + size_of::<usize>()) as *const usize) };
    let itf = r.add(
        Kind::Player,
        [
            OBJECT_VT.as_ptr(),
            PLAY_VT.as_ptr(),
            QUEUE_VT.as_ptr(),
            QUEUE_VT.as_ptr(),
            VOLUME_VT.as_ptr(),
        ],
    );
    // SAFETY: OpenSLES.h: `pPlayer` is writable.
    unsafe { *(out as *mut usize) = itf };
    SUCCESS
}

/// Play `SetPlayState(self, state)`: PLAYING starts the recorder thread, which fires the queue callback once per queued buffer, `callbacks` times.
unsafe extern "C" fn set_play_state(
    a0: usize,
    state: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    let mut r = rec();
    let Some((i, _)) = r.seen(a0) else {
        return PARAMETER_INVALID;
    };
    if state as u32 != PLAYSTATE_PLAYING {
        return SUCCESS;
    }
    let obj = r.objs[i].itfs.as_ptr() as usize;
    let n = r.callbacks;
    r.thread = Some(std::thread::spawn(move || fire(obj, n)));
    SUCCESS
}

/// The recorder thread: `n` buffer-queue callbacks on player `obj`, each after taking one queued buffer.
fn fire(obj: usize, n: u32) {
    ON_RECORDER_THREAD.with(|t| t.set(true));
    for _ in 0..n {
        let (cb, itf, ctx) = {
            let mut r = rec();
            let Some(o) = r.objs.iter_mut().find(|o| o.itfs.as_ptr() as usize == obj) else {
                return;
            };
            if o.queued == 0 || o.queue_cb == 0 {
                r.underruns += 1;
                return;
            }
            o.queued -= 1;
            (
                o.queue_cb,
                &raw const o.itfs[SIMPLE_QUEUE] as usize,
                o.queue_ctx,
            )
        };
        // SAFETY: OpenSLES_Android.h `slAndroidSimpleBufferQueueCallback(caller, pContext)`, registered by RegisterCallback.
        unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(usize, usize)>(cb)(itf, ctx) };
        rec().fired += 1;
    }
}

/// BufferQueue / AndroidSimpleBufferQueue `Enqueue(self, pBuffer, size)`.
unsafe extern "C" fn enqueue(
    a0: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    let mut r = rec();
    let Some((i, _)) = r.seen(a0) else {
        return PARAMETER_INVALID;
    };
    r.objs[i].queued += 1;
    if ON_RECORDER_THREAD.with(std::cell::Cell::get) {
        r.reentered += 1;
    }
    SUCCESS
}

/// BufferQueue / AndroidSimpleBufferQueue `RegisterCallback(self, callback, pContext)`.
unsafe extern "C" fn queue_register(
    a0: usize,
    cb: usize,
    ctx: usize,
    _: usize,
    _: usize,
    _: usize,
    _: usize,
) -> u32 {
    let mut r = rec();
    let Some((i, _)) = r.seen(a0) else {
        return PARAMETER_INVALID;
    };
    r.objs[i].queue_cb = cb;
    r.objs[i].queue_ctx = ctx;
    SUCCESS
}

fn null() -> *const Slot {
    std::ptr::null()
}

/// OpenSLES.h `SLObjectItf_`: Realize, Resume, GetState, GetInterface, RegisterCallback, AbortAsyncOperation, Destroy, SetPriority, GetPriority, SetLossOfControlInterfaces.
static OBJECT_VT: [Slot; 10] = [
    realize,
    nop,
    nop,
    get_interface,
    nop,
    nop,
    destroy,
    nop,
    nop,
    nop,
];
/// `SLEngineItf_`: 15 methods; CreateAudioPlayer is 2, CreateOutputMix 7.
static ENGINE_VT: [Slot; 15] = [
    nop,
    nop,
    create_audio_player,
    nop,
    nop,
    nop,
    nop,
    create_output_mix,
    nop,
    nop,
    nop,
    nop,
    nop,
    nop,
    nop,
];
/// `SLOutputMixItf_`: GetDestinationOutputDeviceIDs, RegisterDeviceChangeCallback, ReRoute.
static MIX_VT: [Slot; 3] = [nop; 3];
/// `SLPlayItf_`: SetPlayState first, then 10 more (RegisterCallback is 4).
static PLAY_VT: [Slot; 11] = [
    set_play_state,
    nop,
    nop,
    nop,
    nop,
    nop,
    nop,
    nop,
    nop,
    nop,
    nop,
];
/// `SLBufferQueueItf_` / `SLAndroidSimpleBufferQueueItf_`: Enqueue, Clear, GetState, RegisterCallback.
static QUEUE_VT: [Slot; 4] = [enqueue, nop, nop, queue_register];
/// `SLVolumeItf_`: 10 methods.
static VOLUME_VT: [Slot; 10] = [nop; 10];

/// OpenSLES.h `slCreateEngine(pEngine, numOptions, pEngineOptions, numInterfaces, pInterfaceIds, pInterfaceRequired)`.
#[unsafe(no_mangle)]
#[allow(non_snake_case, reason = "the OpenSLES.h name")]
pub unsafe extern "C" fn slCreateEngine(
    out: *mut usize,
    _: u32,
    _: usize,
    _: u32,
    _: usize,
    _: usize,
) -> u32 {
    if out.is_null() {
        return PARAMETER_INVALID;
    }
    let mut r = rec();
    let itf = r.add(
        Kind::Engine,
        [
            OBJECT_VT.as_ptr(),
            ENGINE_VT.as_ptr(),
            null(),
            null(),
            null(),
        ],
    );
    r.engine = itf;
    // SAFETY: caller contract (OpenSLES.h).
    unsafe { *out = itf };
    SUCCESS
}

/// Sets the callbacks the next `SetPlayState(PLAYING)` fires.
#[unsafe(no_mangle)]
pub extern "C" fn rec_set_callbacks(n: u32) {
    rec().callbacks = n;
}

/// Waits for the recorder thread, if one runs.
#[unsafe(no_mangle)]
pub extern "C" fn rec_join() {
    let t = rec().thread.take();
    if let Some(t) = t {
        let _ = t.join();
    }
}

/// One recorded value by name (0 for an unknown name): `engine`, `realize_engine_arg0`, `mix`, `sink_mix`, `calls`, `foreign`, `fired`, `reentered`, `underruns`, `objects`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rec_get(name: *const std::ffi::c_char) -> u64 {
    // SAFETY: caller passes a NUL-terminated name.
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes();
    let r = rec();
    match name {
        b"engine" => r.engine as u64,
        b"realize_engine_arg0" => r.realize_engine_arg0 as u64,
        b"mix" => r.mix as u64,
        b"sink_mix" => r.sink_mix as u64,
        b"calls" => r.calls,
        b"foreign" => r.foreign,
        b"fired" => u64::from(r.fired),
        b"reentered" => u64::from(r.reentered),
        b"underruns" => u64::from(r.underruns),
        b"objects" => r.objs.len() as u64,
        _ => 0,
    }
}

/// 1 when `itf` is an interface of a live recorder object (not counted as a call).
#[unsafe(no_mangle)]
pub extern "C" fn rec_is_own(itf: *const c_void) -> u32 {
    let mut r = rec();
    let (calls, foreign) = (r.calls, r.foreign);
    let own = r.seen(itf as usize).is_some();
    (r.calls, r.foreign) = (calls, foreign);
    u32::from(own)
}
