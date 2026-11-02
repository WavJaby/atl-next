// SPDX-License-Identifier: MIT
//! JNI shim of `xtask test native-load`: lets NativeLoad.java call libandroid_runtime's registration entry points, and plays a library loaded after ART that installs its own SIGSEGV handler (stage 5); built by xtask with cargo at test time, never shipped.
//! `startReg` is `android::AndroidRuntime::startReg(JNIEnv*)`, a static member; the register_* functions are free `int f(JNIEnv*)`.
//! Itanium C++ ABI: both take and return exactly as a C function would -> callable as `extern "C"`, no C++ needed. Only the mangled name is C++.
//! register_* are hidden (absent from .dynsym): xtask reads their vaddrs from .symtab; runtime address = vaddr + (runtime startReg - startReg's vaddr), one load bias per object.
#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicI32, Ordering};

type JniEnv = *mut c_void;
type RegFn = unsafe extern "C" fn(JniEnv) -> c_int;

const RTLD_NOW: c_int = 2;
const RTLD_NOLOAD: c_int = 4;
// matched by soname: NativeLoad.java has System.load-ed it by full path just before
const RUNTIME: &[u8] = b"libandroid_runtime.so\0";
const START_REG: &[u8] = b"_ZN7android14AndroidRuntime8startRegEP7_JNIEnv\0";

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
