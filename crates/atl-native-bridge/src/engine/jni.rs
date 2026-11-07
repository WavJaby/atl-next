// SPDX-License-Identifier: MIT
//! The guest's `JNIEnv` / `JavaVM` (bridge1-step4-plan section 3, identity ABI): an object whose word 0 is a guest function table and word 1 the host object it stands for (`GuestObj`).
//! Table slots come from jni.h (build.rs `jni_tables`); each is a JNI stub (arch `jni_stub`) that crosses to the host object's own slot with only the object argument rewritten, so variadic and `va_list` entries need no per-signature code.
//! Specials, whose results hand an env/vm back to the guest, are host functions instead (the set Berberis `jni/jni_trampolines.cc` special-cases, minus `RegisterNatives`: ART asks `isNativeBridgeFunctionPointer`): `GetJavaVM`, `GetEnv`, `AttachCurrentThread`, `AttachCurrentThreadAsDaemon`.
//! Down calls rewrite the other way (arch `Down`): host env -> this thread's guest env, host vm -> the guest vm.

use std::cell::Cell;
use std::sync::OnceLock;

use super::arch;

mod slots {
    include!(concat!(env!("OUT_DIR"), "/jni_tables.rs"));
}
use slots::{env, vm};

const _: () = assert!(env::SLOTS.len() <= arch::JNI_STUBS && vm::SLOTS.len() <= arch::JNI_STUBS);

/// jni.h `JNI_OK`.
const JNI_OK: i32 = 0;

/// A guest `JNIEnv` / `JavaVM`: what guest code's `(*env)->Fn(env, ...)` dereferences. Word 1's offset is fixed by `atl_nb_jni_up` (`mov 8(%rdi)`).
#[repr(C)]
struct GuestObj {
    functions: *const usize,
    host: usize,
}
const _: () = assert!(std::mem::offset_of!(GuestObj, host) == 8);

/// A guest function table: slot i = JNI stub i, a special's host function, or null for jni.h's `reserved*`.
fn table(names: &[&str], specials: &[(usize, usize)], host: usize) -> Box<[usize]> {
    names
        .iter()
        .enumerate()
        .map(|(slot, name)| {
            if let Some(&(_, f)) = specials.iter().find(|&&(s, _)| s == slot) {
                return arch::bind(f)
                    .unwrap_or_else(|| super::unsupported("JNI specials: host thunks used up"));
            }
            if name.starts_with("reserved") {
                return 0;
            }
            direct_break(slot, host).unwrap_or_else(|| arch::jni_stub(slot))
        })
        .collect()
}

/// Deliberate break of gate 4.4 (`ATL_NB_BREAK=jni_direct`): the env's `NewStringUTF` slot is the host function itself, no crossing -> the guest calls ART on its own thread pointer with its own env.
#[cfg(atl_nb_jni_direct)]
fn direct_break(slot: usize, host: usize) -> Option<usize> {
    // SAFETY: `host` is a live host JNIEnv; slot within its table.
    (slot == env::NewStringUTF && host != 0).then(|| unsafe { host_slot(host, slot) })
}
#[cfg(not(atl_nb_jni_direct))]
fn direct_break(_slot: usize, _host: usize) -> Option<usize> {
    None
}

/// Slot `slot` of host JNI object `obj`'s own function table.
///
/// # Safety
/// `obj` is a live host `JNIEnv*` / `JavaVM*`, `slot` within its table.
unsafe fn host_slot(obj: usize, slot: usize) -> usize {
    // SAFETY: caller contract: word 0 of a JNI object is its function table.
    unsafe { *(*(obj as *const *const usize)).add(slot) }
}

static ENV_TABLE: OnceLock<Box<[usize]>> = OnceLock::new();
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
    let functions = ENV_TABLE
        .get_or_init(|| {
            table(
                &env::SLOTS,
                &[(env::GetJavaVM, get_java_vm as *const () as usize)],
                host,
            )
        })
        .as_ptr();
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
        let functions = Box::leak(table(
            &vm::SLOTS,
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
        ))
        .as_ptr();
        Box::into_raw(Box::new(GuestObj { functions, host })) as usize
    });
    // SAFETY: leaked above for the process's life.
    if unsafe { (*(p as *const GuestObj)).host } != host {
        super::unsupported("a second JavaVM");
    }
    p
}

/// # Safety
/// `obj` is a guest object made here.
unsafe fn host_of(obj: *const GuestObj) -> usize {
    // SAFETY: caller contract.
    unsafe { (*obj).host }
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
    unsafe { host_of(guest as *const GuestObj) }
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
        let host = host_of(env);
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
        let host = host_of(vm);
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
        let host = host_of(vm);
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
