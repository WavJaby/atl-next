// SPDX-License-Identifier: MIT
//! JNI shim of `xtask test native-load`: lets NativeLoad.java call libandroid_runtime's registration entry points; compiled by xtask with plain `rustc` at test time, never shipped.
//! `startReg` is `android::AndroidRuntime::startReg(JNIEnv*)`, a static member; the register_* functions are free `int f(JNIEnv*)`.
//! Itanium C++ ABI: both take and return exactly as a C function would -> callable as `extern "C"`, no C++ needed. Only the mangled name is C++.
//! register_* are hidden (absent from .dynsym): xtask reads their vaddrs from .symtab; runtime address = vaddr + (runtime startReg - startReg's vaddr), one load bias per object.
#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_void};

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
