// SPDX-License-Identifier: MIT
//! JNI shim of `xtask test native-load`: lets NativeLoad.java call libandroid_runtime's registration entry points, and plays a library loaded after ART that installs its own SIGSEGV handler (stage 5); built by xtask with cargo at test time, never shipped.
//! `startReg` is `android::AndroidRuntime::startReg(JNIEnv*)`, a static member; the register_* functions are free `int f(JNIEnv*)`.
//! Itanium C++ ABI: both take and return exactly as a C function would -> callable as `extern "C"`, no C++ needed. Only the mangled name is C++.
//! register_* are hidden (absent from .dynsym): xtask reads their vaddrs from .symtab; runtime address = vaddr + (runtime startReg - startReg's vaddr), one load bias per object.
#![allow(non_snake_case)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicI32, Ordering};

type JniEnv = *mut c_void;
type RegFn = unsafe extern "C" fn(JniEnv) -> c_int;

const RTLD_NOW: c_int = 2;
const RTLD_NOLOAD: c_int = 4;
// matched by soname: NativeLoad.java has System.load-ed it by full path just before
const RUNTIME: &[u8] = b"libandroid_runtime.so\0";
const START_REG: &[u8] = b"_ZN7android14AndroidRuntime8startRegEP7_JNIEnv\0";
// NEEDED by libandroid_runtime: loaded with it
const MEDIANDK: &[u8] = b"libmediandk.so\0";

unsafe extern "C" {
    fn dlopen(file: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
}

fn start_reg() -> Option<RegFn> {
    // SAFETY: NUL-terminated constants; RTLD_NOLOAD only looks up an already loaded object.
    let sym = unsafe {
        let h = dlopen(RUNTIME.as_ptr().cast(), RTLD_NOW | RTLD_NOLOAD);
        if h.is_null() {
            return None;
        }
        dlsym(h, START_REG.as_ptr().cast())
    };
    if sym.is_null() {
        return None;
    }
    // SAFETY: the symbol is AndroidRuntime::startReg, of type int(JNIEnv*).
    Some(unsafe { std::mem::transmute::<*mut c_void, RegFn>(sym) })
}

/// `static native int startReg()`: -100 when libandroid_runtime or its startReg is not found.
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_startReg(env: JniEnv, _class: *mut c_void) -> c_int {
    match start_reg() {
        // SAFETY: env is the calling thread's JNIEnv, as startReg expects.
        Some(f) => unsafe { f(env) },
        None => -100,
    }
}

/// Runtime address of the object's vaddr `vaddr`; None when libandroid_runtime or its startReg is not found.
fn runtime_addr(start_reg_vaddr: i64, vaddr: i64) -> Option<usize> {
    let f = start_reg()?;
    let bias = (f as usize).wrapping_sub(start_reg_vaddr as usize);
    Some(bias.wrapping_add(vaddr as usize))
}

/// `static native int call(long startRegVaddr, long vaddr)`: runs the int(JNIEnv*) function at .symtab vaddr `vaddr`; -100 = not found.
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_call(
    env: JniEnv,
    _class: *mut c_void,
    start_reg_vaddr: i64,
    vaddr: i64,
) -> c_int {
    let Some(addr) = runtime_addr(start_reg_vaddr, vaddr) else {
        return -100;
    };
    // SAFETY: xtask passes the .symtab vaddr of a register_* function of this same object, type int(JNIEnv*).
    unsafe { std::mem::transmute::<usize, RegFn>(addr)(env) }
}

/// `static native int callSlot(long startRegVaddr, long slotVaddr)`: runs the function a gRegJNI mProc slot points to, as relocated by the loader; -100 = not found, -101 = null slot.
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_callSlot(
    env: JniEnv,
    _class: *mut c_void,
    start_reg_vaddr: i64,
    slot_vaddr: i64,
) -> c_int {
    let Some(slot) = runtime_addr(start_reg_vaddr, slot_vaddr) else {
        return -100;
    };
    // SAFETY: xtask passes the vaddr of an mProc slot of gRegJNI (read-only after relocation, pointer-aligned) in this same object.
    let f = unsafe { *(slot as *const Option<RegFn>) };
    match f {
        // SAFETY: mProc is int(*)(JNIEnv*); env is the calling thread's JNIEnv.
        Some(f) => unsafe { f(env) },
        None => -101,
    }
}

// ---- stage 5: a later library's own SIGSEGV handler, as game engines and crash reporters install one

/// The status the handler ends the process with; set by `installSegv` (xtask picks it).
static EXIT_STATUS: AtomicI32 = AtomicI32::new(0);

/// Ends the process with [`EXIT_STATUS`]: only async-signal-safe calls.
extern "C" fn on_segv(_sig: c_int, _info: *mut libc::siginfo_t, _ucontext: *mut c_void) {
    // SAFETY: _exit is async-signal-safe and ends the process
    unsafe { libc::_exit(EXIT_STATUS.load(Ordering::Relaxed)) }
}

/// `static native int installSegv(int exitStatus)`: `sigaction(SIGSEGV)` called directly, as the library would; its return value.
/// The call binds by the process's global lookup: to libsigchain when the executable links it first (it then chains this handler behind ART's), else to libc (it replaces ART's).
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_installSegv(
    _env: JniEnv,
    _class: *mut c_void,
    exit_status: c_int,
) -> c_int {
    EXIT_STATUS.store(exit_status, Ordering::Relaxed);
    // SAFETY: all-zero is a valid `struct sigaction` (no handler, empty flags)
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_segv as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
    // SAFETY: sa is initialised; on_segv has the SA_SIGINFO signature; the old action is not asked for
    unsafe {
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut())
    }
}

/// `static native void nativeFault()`: a SIGSEGV outside ART's generated code, which ART's fault handler declines.
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_nativeFault(_env: JniEnv, _class: *mut c_void) {
    // an address in the never-mapped first page
    let p = std::ptr::without_provenance_mut::<c_int>(16);
    // SAFETY: deliberately faults: the point of the call; the handler ends the process
    unsafe { p.write_volatile(1) }
}

// ---- stage 6: a native abort

/// `static native int abortSignal()`: SIGABRT's number, which the process must die of after `raiseAbort`.
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_abortSignal(_env: JniEnv, _class: *mut c_void) -> c_int {
    libc::SIGABRT
}

/// `static native void raiseAbort()`: SIGABRT to this thread, abort()'s first step; returns only if every handler returned without ending the process.
/// Not `abort()`: glibc's abort re-raises with SIG_DFL itself once a handler returns, which would hide a handler that never resends.
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_raiseAbort(_env: JniEnv, _class: *mut c_void) {
    // SAFETY: raise only sends a signal
    unsafe {
        libc::raise(libc::SIGABRT);
    }
}

// ---- stage 8: media probe

type GetUtf = unsafe extern "C" fn(JniEnv, *mut c_void, *mut u8) -> *const c_char;
type ReleaseUtf = unsafe extern "C" fn(JniEnv, *mut c_void, *const c_char);
type CreateDecoder = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type DeleteCodec = unsafe extern "C" fn(*mut c_void) -> c_int;
/// JNINativeInterface slots of GetStringUTFChars / ReleaseStringUTFChars (jni.h function table order).
const GET_STRING_UTF_CHARS: usize = 169;
const RELEASE_STRING_UTF_CHARS: usize = 170;

fn mediandk_sym(name: &[u8]) -> *mut c_void {
    // SAFETY: NUL-terminated constants; RTLD_NOLOAD only looks up an already loaded object.
    unsafe {
        let h = dlopen(MEDIANDK.as_ptr().cast(), RTLD_NOW | RTLD_NOLOAD);
        if h.is_null() {
            return std::ptr::null_mut();
        }
        dlsym(h, name.as_ptr().cast())
    }
}

/// `static native int createDecoder(String mime)`: AMediaCodec_createDecoderByType(mime); 1 = created (and deleted), 0 = null, -100 = libmediandk or its symbols not found.
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_createDecoder(
    env: JniEnv,
    _class: *mut c_void,
    mime: *mut c_void,
) -> c_int {
    let create = mediandk_sym(b"AMediaCodec_createDecoderByType\0");
    let delete = mediandk_sym(b"AMediaCodec_delete\0");
    if create.is_null() || delete.is_null() {
        return -100;
    }
    // SAFETY: env is the calling thread's JNIEnv (a pointer to the function table); mime is a live jstring; the symbols have the NDK signatures above.
    unsafe {
        let table = *(env as *const *const *mut c_void);
        let get: GetUtf = std::mem::transmute(*table.add(GET_STRING_UTF_CHARS));
        let release: ReleaseUtf = std::mem::transmute(*table.add(RELEASE_STRING_UTF_CHARS));
        let create: CreateDecoder = std::mem::transmute(create);
        let delete: DeleteCodec = std::mem::transmute(delete);
        let s = get(env, mime, std::ptr::null_mut());
        let codec = create(s);
        release(env, mime, s);
        if codec.is_null() {
            0
        } else {
            delete(codec);
            1
        }
    }
}

// ---- stage 9: one clip through the NDK extractor and decoder (audio M1 gate)

const BINDER_NDK: &[u8] = b"libbinder_ndk.so\0";
const IFACE: &[u8] = b"android.hardware.media.c2.IComponentStore\0";
const STORE: &[u8] = b"android.hardware.media.c2.IComponentStore/default\0";
/// JNINativeInterface slot of NewStringUTF (jni.h order), beside GetStringUTFChars (169).
const NEW_STRING_UTF: usize = 167;
/// AMEDIACODEC_BUFFER_FLAG_END_OF_STREAM.
const FLAG_EOS: u32 = 4;
/// Dequeue timeout per poll; empty output polls in a row before a stuck codec is an error (100 x 100 ms), not a wait for the gate's kill.
const TIMEOUT_US: i64 = 100_000;
const IDLE_POLLS: u32 = 100;
/// AudioFormat ENCODING_PCM_16BIT and ENCODING_PCM_FLOAT: the output encodings accepted, with their bytes per sample.
const PCM_16: i32 = 2;
const PCM_FLOAT: i32 = 4;

type NewStringUtf = unsafe extern "C" fn(JniEnv, *const c_char) -> *mut c_void;
type ForEachDeclared =
    unsafe extern "C" fn(*const c_char, *mut c_void, unsafe extern "C" fn(*const c_char, *mut c_void));
type DeclaredFn = unsafe extern "C" fn(*const c_char) -> bool;
type CheckService = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type IsRemote = unsafe extern "C" fn(*const c_void) -> bool;
type DecStrong = unsafe extern "C" fn(*mut c_void);

// NdkMediaExtractor.h / NdkMediaCodec.h / NdkMediaFormat.h; ssize_t as isize, off64_t as i64
type ExtractorNew = unsafe extern "C" fn() -> *mut c_void;
type ExtractorSetFd = unsafe extern "C" fn(*mut c_void, c_int, i64, i64) -> c_int;
type ExtractorTracks = unsafe extern "C" fn(*mut c_void) -> usize;
type ExtractorFormat = unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void;
type ExtractorSelect = unsafe extern "C" fn(*mut c_void, usize) -> c_int;
type ExtractorRead = unsafe extern "C" fn(*mut c_void, *mut u8, usize) -> isize;
type ExtractorTime = unsafe extern "C" fn(*mut c_void) -> i64;
type ExtractorAdvance = unsafe extern "C" fn(*mut c_void) -> bool;
type FormatString = unsafe extern "C" fn(*mut c_void, *const c_char, *mut *const c_char) -> bool;
type FormatInt = unsafe extern "C" fn(*mut c_void, *const c_char, *mut i32) -> bool;
type FormatDelete = unsafe extern "C" fn(*mut c_void) -> c_int;
type CodecCreate = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type CodecConfigure = unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void, *mut c_void, u32) -> c_int;
type CodecStatus = unsafe extern "C" fn(*mut c_void) -> c_int;
type CodecDequeueIn = unsafe extern "C" fn(*mut c_void, i64) -> isize;
type CodecBuffer = unsafe extern "C" fn(*mut c_void, usize, *mut usize) -> *mut u8;
type CodecQueueIn = unsafe extern "C" fn(*mut c_void, usize, i64, usize, u64, u32) -> c_int;
type CodecDequeueOut = unsafe extern "C" fn(*mut c_void, *mut BufferInfo, i64) -> isize;
type CodecOutFormat = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type CodecRelease = unsafe extern "C" fn(*mut c_void, usize, bool) -> c_int;

/// AMediaCodecBufferInfo.
#[repr(C)]
#[derive(Default)]
struct BufferInfo {
    offset: i32,
    size: i32,
    presentation_time_us: i64,
    flags: u32,
}

/// The function `name` of the already loaded `lib` as the function type `T`; None when either is absent.
fn lib_sym<T: Copy>(lib: &[u8], name: &[u8]) -> Option<T> {
    // SAFETY: NUL-terminated constants; RTLD_NOLOAD only looks up an already loaded object.
    let p = unsafe {
        let h = dlopen(lib.as_ptr().cast(), RTLD_NOW | RTLD_NOLOAD);
        if h.is_null() {
            return None;
        }
        dlsym(h, name.as_ptr().cast())
    };
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller names a function whose C signature is T; T is a function pointer, as p is.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

/// An NDK function from libmediandk as `$ty`, or the decode step's error when the library lacks it.
macro_rules! ndk {
    ($name:literal, $ty:ty) => {
        lib_sym::<$ty>(MEDIANDK, concat!($name, "\0").as_bytes()).ok_or(concat!("error=symbol:", $name))?
    };
}

const CUTILS: &[u8] = b"libcutils.so\0";
/// Region size of the ashmem probe: several pages plus a remainder, so a size rounded to pages or read from the wrong place shows. `ashmem=` reports it back; xtask compares with ASHMEM_PROBE_SIZE in native_load/audio.rs.
const ASHMEM_PROBE_SIZE: usize = 3 * 4096 + 100;
type AshmemCreate = unsafe extern "C" fn(*const c_char, usize) -> c_int;
type AshmemSize = unsafe extern "C" fn(c_int) -> c_int;

/// libcutils' shared memory as Codec2 uses it (FMQ, linear blocks): create a region, map it, write + read back, size from ashmem_get_size_region. `ashmem=<size>`, else `error=ashmem:<step>`.
fn ashmem_probe(out: &mut Vec<String>) -> Result<(), String> {
    let create = lib_sym::<AshmemCreate>(CUTILS, b"ashmem_create_region\0").ok_or("error=ashmem:symbol")?;
    let size_of = lib_sym::<AshmemSize>(CUTILS, b"ashmem_get_size_region\0").ok_or("error=ashmem:symbol")?;
    // SAFETY: NUL-terminated name; the libcutils functions have the signatures above; the mapping is private to this block and unmapped before the fd closes.
    unsafe {
        let fd = create(c"nlt-ashmem".as_ptr(), ASHMEM_PROBE_SIZE);
        if fd < 0 {
            return Err("error=ashmem:create".into());
        }
        let size = size_of(fd);
        let p = libc::mmap(std::ptr::null_mut(), ASHMEM_PROBE_SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
        let mapped = p != libc::MAP_FAILED && {
            let last = p.cast::<u8>().add(ASHMEM_PROBE_SIZE - 1);
            last.write_volatile(0x5a);
            let ok = last.read_volatile() == 0x5a;
            libc::munmap(p, ASHMEM_PROBE_SIZE);
            ok
        };
        libc::close(fd);
        if !mapped {
            return Err("error=ashmem:map".into());
        }
        out.push(format!("ashmem={size}"));
    }
    Ok(())
}

/// AMediaFormat's int32 `key`, None when absent.
fn int_key(get: FormatInt, fmt: *mut c_void, key: &CStr) -> Option<i32> {
    let mut v = 0i32;
    // SAFETY: fmt is a live AMediaFormat (the callers' contract); key is NUL-terminated; v is a valid out-pointer.
    let found = unsafe { get(fmt, key.as_ptr(), &mut v) };
    found.then_some(v)
}

/// The process's view of the Codec2 store: whether it is declared, the instances its interface lists, and what checkService returns (`local` = the in-process view, `remote` = servicemanager's).
fn store_probe(out: &mut Vec<String>) {
    let (Some(declared), Some(for_each), Some(check), Some(is_remote), Some(dec)) = (
        lib_sym::<DeclaredFn>(BINDER_NDK, b"AServiceManager_isDeclared\0"),
        lib_sym::<ForEachDeclared>(BINDER_NDK, b"AServiceManager_forEachDeclaredInstance\0"),
        lib_sym::<CheckService>(BINDER_NDK, b"AServiceManager_checkService\0"),
        lib_sym::<IsRemote>(BINDER_NDK, b"AIBinder_isRemote\0"),
        lib_sym::<DecStrong>(BINDER_NDK, b"AIBinder_decStrong\0"),
    ) else {
        out.push("store=unavailable".to_owned());
        return;
    };
    let mut names: Vec<String> = Vec::new();
    // SAFETY: IFACE is NUL-terminated; `collect` gets &mut names as its context, used only during this call.
    unsafe { for_each(IFACE.as_ptr().cast(), (&mut names as *mut Vec<String>).cast(), collect) };
    // SAFETY: STORE is NUL-terminated.
    let is_decl = unsafe { declared(STORE.as_ptr().cast()) };
    // SAFETY: STORE is NUL-terminated; a non-null result holds one strong reference, released below.
    let binder = unsafe { check(STORE.as_ptr().cast()) };
    let store = if binder.is_null() {
        "none"
    } else {
        // SAFETY: binder is non-null, so live.
        let remote = unsafe { is_remote(binder) };
        // SAFETY: releases the reference checkService returned.
        unsafe { dec(binder) };
        if remote { "remote" } else { "local" }
    };
    out.push(format!("declared={}", u8::from(is_decl)));
    out.push(format!(
        "instances={}",
        if names.is_empty() { "-".to_owned() } else { names.join(",") }
    ));
    out.push(format!("store={store}"));
}

/// `AServiceManager_forEachDeclaredInstance`'s callback: appends the instance to the Vec behind `context`.
unsafe extern "C" fn collect(instance: *const c_char, context: *mut c_void) {
    // SAFETY: context is the &mut Vec<String> store_probe passed; instance is NUL-terminated for this call.
    unsafe {
        let names = &mut *context.cast::<Vec<String>>();
        names.push(CStr::from_ptr(instance).to_string_lossy().into_owned());
    }
}

/// Decodes the clip `input` to raw PCM at `output`, pushing key=value pairs to `out`; an Err (`error=<step>...`) names the step that failed.
fn decode(input: &str, output: &str, out: &mut Vec<String>) -> Result<(), String> {
    let file = File::open(input).map_err(|e| format!("error=open:{e}"))?;
    let size = file.metadata().map_err(|e| format!("error=open:{e}"))?.len();
    let mut pcm = File::create(output).map_err(|e| format!("error=output:{e}"))?;
    let new_extractor = ndk!("AMediaExtractor_new", ExtractorNew);
    let set_fd = ndk!("AMediaExtractor_setDataSourceFd", ExtractorSetFd);
    let tracks = ndk!("AMediaExtractor_getTrackCount", ExtractorTracks);
    let track_format = ndk!("AMediaExtractor_getTrackFormat", ExtractorFormat);
    let select = ndk!("AMediaExtractor_selectTrack", ExtractorSelect);
    let read = ndk!("AMediaExtractor_readSampleData", ExtractorRead);
    let sample_time = ndk!("AMediaExtractor_getSampleTime", ExtractorTime);
    let advance = ndk!("AMediaExtractor_advance", ExtractorAdvance);
    let format_string = ndk!("AMediaFormat_getString", FormatString);
    let format_int = ndk!("AMediaFormat_getInt32", FormatInt);
    let format_delete = ndk!("AMediaFormat_delete", FormatDelete);
    let create = ndk!("AMediaCodec_createDecoderByType", CodecCreate);
    let configure = ndk!("AMediaCodec_configure", CodecConfigure);
    let start = ndk!("AMediaCodec_start", CodecStatus);
    let dequeue_in = ndk!("AMediaCodec_dequeueInputBuffer", CodecDequeueIn);
    let input_buffer = ndk!("AMediaCodec_getInputBuffer", CodecBuffer);
    let queue_in = ndk!("AMediaCodec_queueInputBuffer", CodecQueueIn);
    let dequeue_out = ndk!("AMediaCodec_dequeueOutputBuffer", CodecDequeueOut);
    let output_buffer = ndk!("AMediaCodec_getOutputBuffer", CodecBuffer);
    let output_format = ndk!("AMediaCodec_getOutputFormat", CodecOutFormat);
    let release_out = ndk!("AMediaCodec_releaseOutputBuffer", CodecRelease);

    // SAFETY: every pointer is the NDK function its alias names; each handle is checked for null before use and used only while the step that made it runs.
    unsafe {
        let ex = new_extractor();
        if ex.is_null() {
            return Err("error=extractor:null".to_owned());
        }
        let st = set_fd(ex, file.as_raw_fd(), 0, size as i64);
        out.push(format!("setDataSource={st}"));
        if st != 0 {
            return Err(format!("error=setDataSource:{st}"));
        }
        let n = tracks(ex);
        out.push(format!("tracks={n}"));
        if n == 0 {
            return Err("error=tracks:0".to_owned());
        }
        let fmt = track_format(ex, 0);
        if fmt.is_null() {
            return Err("error=format:null".to_owned());
        }
        let mut mime: *const c_char = std::ptr::null();
        if !format_string(fmt, c"mime".as_ptr(), &mut mime) || mime.is_null() {
            return Err("error=format:mime".to_owned());
        }
        let mime_name = CStr::from_ptr(mime).to_string_lossy().into_owned();
        out.push(format!("mime={mime_name}"));
        let mut channels = int_key(format_int, fmt, c"channel-count").unwrap_or(0);
        let mut rate = int_key(format_int, fmt, c"sample-rate").unwrap_or(0);
        out.push(format!("in_channels={channels} in_rate={rate}"));
        let st = select(ex, 0);
        if st != 0 {
            return Err(format!("error=select:{st}"));
        }

        let codec = create(mime);
        if codec.is_null() {
            return Err(format!("error=create:null mime={mime_name}"));
        }
        let st = configure(codec, fmt, std::ptr::null_mut(), std::ptr::null_mut(), 0);
        if st != 0 {
            return Err(format!("error=configure:{st}"));
        }
        let st = start(codec);
        if st != 0 {
            return Err(format!("error=start:{st}"));
        }

        let mut enc = PCM_16;
        let mut bytes: u64 = 0;
        let mut fed = false;
        let mut done = false;
        let mut idle = 0u32;
        while !done {
            if !fed {
                let idx = dequeue_in(codec, TIMEOUT_US);
                if idx >= 0 {
                    let mut cap = 0usize;
                    let buf = input_buffer(codec, idx as usize, &mut cap);
                    if buf.is_null() {
                        return Err("error=input:buffer".to_owned());
                    }
                    let n = read(ex, buf, cap);
                    if n < 0 {
                        queue_in(codec, idx as usize, 0, 0, 0, FLAG_EOS);
                        fed = true;
                    } else {
                        let t = sample_time(ex);
                        queue_in(codec, idx as usize, 0, n as usize, t as u64, 0);
                        advance(ex);
                    }
                }
            }
            let mut info = BufferInfo::default();
            let idx = dequeue_out(codec, &mut info, TIMEOUT_US);
            match idx {
                -1 => {
                    idle += 1;
                    if idle > IDLE_POLLS {
                        return Err(format!("error=output:no output for {IDLE_POLLS} polls, bytes={bytes}"));
                    }
                }
                -2 => {
                    // INFO_OUTPUT_FORMAT_CHANGED: the decoder's own channel count, rate and encoding
                    let f = output_format(codec);
                    if !f.is_null() {
                        channels = int_key(format_int, f, c"channel-count").unwrap_or(channels);
                        rate = int_key(format_int, f, c"sample-rate").unwrap_or(rate);
                        enc = int_key(format_int, f, c"pcm-encoding").unwrap_or(PCM_16);
                        format_delete(f);
                    }
                }
                -3 => {}
                i if i >= 0 => {
                    idle = 0;
                    let mut cap = 0usize;
                    let buf = output_buffer(codec, i as usize, &mut cap);
                    let off = info.offset.max(0) as usize;
                    let len = info.size.max(0) as usize;
                    if buf.is_null() || off + len > cap {
                        return Err(format!("error=output:buffer offset={off} size={len} cap={cap}"));
                    }
                    if len > 0 {
                        // SAFETY: buf is the codec's output buffer of cap bytes; off + len <= cap was checked above.
                        let data = std::slice::from_raw_parts(buf.add(off), len);
                        pcm.write_all(data).map_err(|e| format!("error=output:{e}"))?;
                    }
                    bytes += len as u64;
                    done = info.flags & FLAG_EOS != 0;
                    release_out(codec, i as usize, false);
                }
                other => return Err(format!("error=output:dequeue:{other}")),
            }
        }
        let bps = match enc {
            PCM_16 => 2,
            PCM_FLOAT => 4,
            other => return Err(format!("error=pcm-encoding:{other}")),
        };
        let frames = bytes / (channels.max(1) as u64 * bps);
        out.push(format!("frames={frames} bytes={bytes} channels={channels} rate={rate} pcm={enc}"));
    }
    Ok(())
}

/// Java string as UTF-8 (GetStringUTFChars / ReleaseStringUTFChars, as createDecoder does).
fn java_string(env: JniEnv, s: *mut c_void) -> String {
    // SAFETY: env is the calling thread's JNIEnv; s is a live jstring for the call.
    unsafe {
        let table = *(env as *const *const *mut c_void);
        let get: GetUtf = std::mem::transmute(*table.add(GET_STRING_UTF_CHARS));
        let release: ReleaseUtf = std::mem::transmute(*table.add(RELEASE_STRING_UTF_CHARS));
        let c = get(env, s, std::ptr::null_mut());
        let text = CStr::from_ptr(c).to_string_lossy().into_owned();
        release(env, s, c);
        text
    }
}

/// `static native String decodeFile(String input, String output)`: stage 9's clip, returned as the line `nlt decode` prints.
#[unsafe(no_mangle)]
pub extern "C" fn Java_NativeLoad_decodeFile(
    env: JniEnv,
    _class: *mut c_void,
    input: *mut c_void,
    output: *mut c_void,
) -> *mut c_void {
    let (input, output) = (java_string(env, input), java_string(env, output));
    let mut out = Vec::new();
    store_probe(&mut out);
    if let Err(e) = ashmem_probe(&mut out).and_then(|()| decode(&input, &output, &mut out)) {
        out.push(e);
    }
    let text = CString::new(out.join(" ")).unwrap_or_default();
    // SAFETY: env is the calling thread's JNIEnv; NewStringUTF copies the NUL-terminated text into a new local reference.
    unsafe {
        let table = *(env as *const *const *mut c_void);
        let new_string: NewStringUtf = std::mem::transmute(*table.add(NEW_STRING_UTF));
        new_string(env, text.as_ptr())
    }
}
