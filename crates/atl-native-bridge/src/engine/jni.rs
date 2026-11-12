// SPDX-License-Identifier: MIT
//! The guest's `JNIEnv` / `JavaVM` (bridge1-step4-plan section 3, identity ABI): an object whose word 0 is a guest function table and word 1 the host object it stands for (`GuestObj`).
//! Two rows of the proxy object tables (objects.rs): slots from jni.h; each is an object stub that crosses to the host object's own slot with only the object argument rewritten, so variadic entries need no per-signature code. Under QEMU a `va_list` entry crosses to its `...A` sibling instead, its arguments read by the method's shorty (dispatch.rs `call_object`, va_list.rs).
//! Specials, whose results hand an env/vm back to the guest, are host functions instead (the set Berberis `jni/jni_trampolines.cc` special-cases, minus `RegisterNatives`: ART asks `isNativeBridgeFunctionPointer`): `GetJavaVM`, `GetEnv`, `AttachCurrentThread`, `AttachCurrentThreadAsDaemon`.
//! Down calls rewrite the other way (arch `Down`): host env -> this thread's guest env, host vm -> the guest vm.

use std::cell::Cell;
use std::sync::OnceLock;

use super::objects::{self, ENV as ENV_TABLE, GuestObj, VM as VM_TABLE, env, vm};
use objects::host_slot;

/// jni.h `JNI_OK`.
const JNI_OK: i32 = 0;

/// The jni.h entries at JNI slot `slot` for the crossing counter's dump: [env entry, vm entry], "" where a table is shorter. One stub pool serves both tables, so a counted slot is either.
#[cfg(feature = "crossing-count")]
pub(crate) fn slot_names(slot: usize) -> [&'static str; 2] {
    [env::SLOTS.get(slot), vm::SLOTS.get(slot)].map(|n| n.copied().unwrap_or(""))
}

/// Deliberate break `jni_swap` (dispatch.rs `call_object`): the one entry whose variadic lanes it exchanges. Other entries keep their order: GetStaticMethodID's x3 is its signature.
#[cfg(atl_nb_jni_swap)]
pub(super) const CALL_STATIC_INT_METHOD: usize = env::CallStaticIntMethod;

static VM: OnceLock<usize> = OnceLock::new();

/// This thread's guest env object, freed at thread exit.
struct ThreadEnv(Cell<*mut GuestObj>);
impl Drop for ThreadEnv {
    fn drop(&mut self) {
        let p = self.0.get();
        if !p.is_null() {
            // SAFETY: made by `guest_env` with Box::into_raw on this thread; the thread runs no guest code after its TLS destructors.
            drop(unsafe { Box::from_raw(p) });
        }
    }
}
thread_local! {
    static ENV: ThreadEnv = const { ThreadEnv(Cell::new(std::ptr::null_mut())) };
}

/// The guest env standing for host env `host` on this thread (one per thread, as ART's).
pub fn guest_env(host: usize) -> usize {
    if host == 0 {
        return 0;
    }
    let functions = objects::functions(
        ENV_TABLE,
        &[(env::GetJavaVM, get_java_vm as *const () as usize)],
        host,
    );
    ENV.with(|e| {
        let mut p = e.0.get();
        if p.is_null() {
            p = Box::into_raw(Box::new(GuestObj { functions, host }));
            e.0.set(p);
        }
        // SAFETY: this thread's object.
        unsafe { (*p).host = host };
        p as usize
    })
}

/// The guest vm standing for host vm `host` (ART has one per process).
pub fn guest_vm(host: usize) -> usize {
    if host == 0 {
        return 0;
    }
    let p = *VM.get_or_init(|| {
        let functions = objects::functions(
            VM_TABLE,
            &[
                (vm::GetEnv, get_env as *const () as usize),
                (
                    vm::AttachCurrentThread,
                    attach_current_thread as *const () as usize,
                ),
                (
                    vm::AttachCurrentThreadAsDaemon,
                    attach_current_thread_as_daemon as *const () as usize,
                ),
            ],
            host,
        );
        Box::into_raw(Box::new(GuestObj { functions, host })) as usize
    });
    // SAFETY: leaked above for the process's life.
    if unsafe { (*(p as *const GuestObj)).host } != host {
        super::unsupported("a second JavaVM");
    }
    p
}

use objects::host_of;

/// ART's `NativeBridgeRuntimeCallbacks::getMethodShorty(env, mid)` (native_bridge.h): a method's shorty, result first.
pub type MethodShorty = unsafe extern "C" fn(usize, usize) -> *const std::ffi::c_char;
static METHOD_SHORTY: OnceLock<MethodShorty> = OnceLock::new();

/// Keeps the runtime's shorty callback (exports.rs `initialize`): a guest `...V` call reads its `va_list` by the method's shorty (dispatch.rs `call_object`). The first one set stays.
pub fn set_method_shorty(f: MethodShorty) {
    let _ = METHOD_SHORTY.set(f);
}

/// The parameter characters of method `mid`'s shorty (its result character dropped), asked of the runtime on host env `host`; `None` when no callback was set or it answered null.
///
/// # Safety
/// `host` is a host JNIEnv and `mid` a jmethodID of it; the shorty the runtime returns lives as long as the method.
pub(super) unsafe fn method_params(host: usize, mid: usize) -> Option<&'static [u8]> {
    let f = METHOD_SHORTY.get()?;
    // SAFETY: caller contract; native_bridge.h's getMethodShorty.
    let p = unsafe { f(host, mid) };
    // SAFETY: a non-null shorty is NUL-terminated and outlives the method (caller contract).
    let s = (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) })?;
    s.to_bytes().get(1..)
}

/// The `...A` sibling of `...V` slot `id`: jni.h declares each `XV` directly before `XA` (unit test `a_slot_follows_each_v_slot`).
pub(super) const fn a_slot(id: usize) -> usize {
    id + 1
}

/// The host env behind guest env `guest` (0 = 0), for the NDK shims (ndk.rs): an NDK function takes a `JNIEnv*` and calls the host library through it.
/// Deliberate break `ATL_NB_BREAK=ndk_env`: the guest env itself, whose function table is the guest's.
///
/// # Safety
/// `guest` is 0 or a guest env made by `guest_env`.
#[cfg(not(atl_nb_ndk_env))]
pub(super) unsafe fn host_env(guest: usize) -> usize {
    if guest == 0 {
        return 0;
    }
    // SAFETY: caller contract.
    unsafe { host_of(guest) }
}

/// # Safety
/// As the non-break `host_env`; ignores its argument.
#[cfg(atl_nb_ndk_env)]
pub(super) unsafe fn host_env(guest: usize) -> usize {
    guest
}

/// `jint GetJavaVM(JNIEnv*, JavaVM**)`, on the host thread pointer (a bound thunk).
extern "C" fn get_java_vm(env: *const GuestObj, out: *mut usize) -> i32 {
    // SAFETY: the guest's own env and out-parameter, as jni.h's GetJavaVM takes them.
    unsafe {
        let host = host_of(env as usize);
        let f: extern "C" fn(usize, *mut usize) -> i32 =
            std::mem::transmute(host_slot(host, env::GetJavaVM));
        let rc = f(host, out);
        if rc == JNI_OK {
            *out = guest_vm(*out);
        }
        rc
    }
}

/// `jint GetEnv(JavaVM*, void**, jint)`.
extern "C" fn get_env(vm: *const GuestObj, out: *mut usize, version: i32) -> i32 {
    // SAFETY: as jni.h's GetEnv.
    unsafe {
        let host = host_of(vm as usize);
        let f: extern "C" fn(usize, *mut usize, i32) -> i32 =
            std::mem::transmute(host_slot(host, vm::GetEnv));
        let rc = f(host, out, version);
        if rc == JNI_OK {
            *out = guest_env(*out);
        }
        rc
    }
}

/// `AttachCurrentThread(AsDaemon)(JavaVM*, JNIEnv**, void*)`: the host attach, then this thread's guest env.
///
/// # Safety
/// As jni.h's AttachCurrentThread.
unsafe fn attach(vm: *const GuestObj, out: *mut usize, args: usize, slot: usize) -> i32 {
    // SAFETY: caller contract.
    unsafe {
        let host = host_of(vm as usize);
        let f: extern "C" fn(usize, *mut usize, usize) -> i32 =
            std::mem::transmute(host_slot(host, slot));
        let rc = f(host, out, args);
        if rc == JNI_OK {
            *out = guest_env(*out);
        }
        rc
    }
}

extern "C" fn attach_current_thread(vm: *const GuestObj, out: *mut usize, args: usize) -> i32 {
    // SAFETY: the guest's arguments to jni.h's AttachCurrentThread.
    unsafe { attach(vm, out, args, vm::AttachCurrentThread) }
}

extern "C" fn attach_current_thread_as_daemon(
    vm: *const GuestObj,
    out: *mut usize,
    args: usize,
) -> i32 {
    // SAFETY: as above.
    unsafe { attach(vm, out, args, vm::AttachCurrentThreadAsDaemon) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `va_list` slot `XV` has `XA` at `a_slot`, with the same parameters up to the `va_list` (a `jvalue*` there) and the same result. jni.h has 31 V slots (10 types x Call, CallNonvirtual, CallStatic, plus NewObject).
    #[test]
    fn a_slot_follows_each_v_slot() {
        let mut n = 0;
        for (id, lane) in objects::LANES[ENV_TABLE].iter().enumerate() {
            let Some(v) = lane else { continue };
            let Some(k) = v.params.iter().position(|c| *c == objects::Class::VaList) else {
                continue;
            };
            n += 1;
            let a = objects::LANES[ENV_TABLE][a_slot(id)].expect("an A slot");
            let stem = v.name.strip_suffix('V').expect("a V name");
            assert_eq!(a.name.strip_suffix('A'), Some(stem), "{}", v.name);
            assert_eq!(a.params.len(), k + 1, "{}", v.name);
            assert_eq!(a.params[..k], v.params[..k], "{}", v.name);
            assert_eq!(a.ret, v.ret, "{}", v.name);
        }
        assert_eq!(n, 31);
    }
}
