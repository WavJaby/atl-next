// SPDX-License-Identifier: MIT
//! Host sides of the guest libc's pthread stubs (NBS `stubs_x86_64.cpp`): guest threads are glibc threads, as NBS makes them host threads.
//! Pass-through to glibc where the ABI is the same (build.rs `GLIBC` rows of the binding table): `pthread_t` (long), `pthread_key_t` (int), `pthread_attr_t` (56 bytes, only ever filled and read by glibc, since every `pthread_attr_*` is a glibc row too) -- sizes gated below against the generated bionic layout.
//! Own host sides (`Shim` rows) where the guest hands host code a guest function: `pthread_create` (start routine on the new thread's guest thread pointer, after attach on a guest stack of the attr's size) and `pthread_key_create` (destructor run by thread.rs `detach`); and `pthread_getattr_np`, whose answer is the guest stack (thread.rs `stack_of`), not glibc's view of the host thread. QEMU only: `pthread_exit`, which ends the start routine's guest call.

use std::cell::Cell;
use std::ffi::c_void;

use super::thread::layout::{identity, qemu};
use super::{StackReq, call_guest, thread};

// the pass-through set's types: bionic guest view == glibc host view, for both engines' guests
const _: () = {
    assert!(identity::pthread_attr_t::SIZE == size_of::<libc::pthread_attr_t>());
    assert!(identity::pthread_attr_t::ALIGN == align_of::<libc::pthread_attr_t>());
    assert!(qemu::pthread_attr_t::SIZE == size_of::<libc::pthread_attr_t>());
    assert!(qemu::pthread_attr_t::ALIGN == align_of::<libc::pthread_attr_t>());
    assert!(size_of::<libc::pthread_t>() == size_of::<std::ffi::c_long>());
    assert!(size_of::<libc::pthread_key_t>() == size_of::<std::ffi::c_int>());
};

struct Start {
    routine: usize,
    arg: usize,
    stack: StackReq,
}

/// The guest stack `attr` asks for: its stack and guard size (glibc getters: the attr is glibc's, gated above); null = the engine's default. A caller-supplied stack (`pthread_attr_setstack`) is sized like any other: guest code runs on the engine's stack, never on the caller's memory.
fn stack_req(attr: *const libc::pthread_attr_t) -> StackReq {
    if attr.is_null() {
        return StackReq::default();
    }
    let (mut size, mut guard) = (0usize, 0usize);
    // SAFETY: the guest's initialised attr, glibc layout (gated above); out parameters are locals.
    unsafe {
        libc::pthread_attr_getstacksize(attr, &mut size);
        libc::pthread_attr_getguardsize(attr, &mut guard);
    }
    // deliberate break (`ATL_NB_BREAK=qt_attr_size`): the attr's size never reaches attach (qemu-thread gate `g`)
    if cfg!(atl_nb_qt_attr_size) {
        size = 0;
    }
    StackReq { size, guard }
}

/// `int pthread_create(pthread_t*, const pthread_attr_t*, void* (*)(void*), void*)`: a glibc thread whose start routine is the guest's, run after the thread attaches on a guest stack sized by `attr` (the glibc host thread gets `attr` too).
pub(super) extern "C" fn pthread_create(
    thread: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    routine: usize,
    arg: usize,
) -> i32 {
    let stack = stack_req(attr);
    let start = Box::into_raw(Box::new(Start {
        routine,
        arg,
        stack,
    }));
    // SAFETY: `thread` / `attr` are the guest's, same ABI (gated above); `start` is ours until `run` takes it.
    let rc = unsafe { libc::pthread_create(thread, attr, run, start.cast()) };
    if rc != 0 {
        // SAFETY: not handed to a thread.
        drop(unsafe { Box::from_raw(start) });
    }
    rc
}

extern "C" fn run(start: *mut c_void) -> *mut c_void {
    // SAFETY: the box `pthread_create` made for this thread.
    let Start {
        routine,
        arg,
        stack,
    } = *unsafe { Box::from_raw(start.cast::<Start>()) };
    let ht = thread::attach_with(stack);
    ROUTINE.with(|r| r.set(true));
    // deliberate break (`ATL_NB_BREAK=qt_routine_arg`): the routine gets the wrong argument (qemu-thread gate)
    let arg = if cfg!(atl_nb_qt_routine_arg) {
        arg + 1
    } else {
        arg
    };
    // SAFETY: the guest's `void* (*)(void*)`, this thread's record.
    unsafe { call_guest(&mut *ht, routine, arg, 0, 0, 0) as *mut c_void }
}

thread_local! {
    /// This thread runs a guest `pthread_create` start routine ([`run`]): the one place a guest `pthread_exit` can end.
    static ROUTINE: Cell<bool> = const { Cell::new(false) };
}

/// `void pthread_exit(void*)` (QEMU table only, build.rs `QEMU_SHIMS`): ends the start routine's guest call at once with `value` as its result (answered in x0, hence the return), so [`run`] returns it to glibc, whose join reports it; the host thread then exits as after a plain return.
/// bionic's order (pthread_exit.cpp): `__cxa_thread_finalize`, cleanup handlers, `pthread_key_clean_all`, then the stack goes. The first two are guest-libc NBS stubs with no host side here (an app that registers either traps first); key destructors and the guest stack are thread.rs `detach`'s, run by the host key at this thread's exit.
/// Only from the routine's own guest call (call depth 1 on a [`run`] thread): a deeper exit would skip host frames between the calls, and a host thread that merely attached has no routine to end. Both abort, named.
pub(super) extern "C" fn pthread_exit(value: usize) -> usize {
    let (routine, depth) = (ROUTINE.with(Cell::get), super::qemu::call_depth());
    if !routine || depth != 1 {
        super::unsupported(&format!(
            "guest pthread_exit outside a guest thread's start routine (routine thread {routine}, guest call depth {depth})"
        ));
    }
    // deliberate break (`ATL_NB_BREAK=qt_exit_resume`): the call is not ended, so the guest resumes after its noreturn pthread_exit (qemu-thread gate `x`)
    if !cfg!(atl_nb_qt_exit_resume)
        && let Err(e) = super::qemu::end_call()
    {
        super::unsupported(&format!("guest pthread_exit: {e}"));
    }
    value
}

/// `int pthread_key_create(pthread_key_t*, void (*)(void*))`: glibc's key without a destructor; the guest's is stored here and runs at thread exit (thread.rs `detach`). The store is the same under QEMU; the destructor crosses on the exiting thread (thread.rs `detach`).
pub(super) extern "C" fn pthread_key_create(key: *mut libc::pthread_key_t, dtor: usize) -> i32 {
    // SAFETY: `key` is the guest's, same ABI (gated above).
    let rc = unsafe { libc::pthread_key_create(key, None) };
    if rc == 0 {
        // SAFETY: written by glibc just now.
        thread::set_guest_dtor(unsafe { *key }, dtor);
    }
    rc
}

/// `int pthread_getattr_np(pthread_t, pthread_attr_t*)`: `attr` initialised and set to the stack the thread's guest code runs on (bionic's contract: sp inside `[base, base + size)`, guard size below), for either handle the guest holds (its `pthread_self` record or its `pthread_create` out-handle; thread.rs `stack_of`). ESRCH for a handle of no attached thread. Other attr fields stay glibc's defaults.
pub(super) extern "C" fn pthread_getattr_np(
    thread: libc::pthread_t,
    attr: *mut libc::pthread_attr_t,
) -> i32 {
    let Some(s) = thread::stack_of(thread as usize) else {
        return libc::ESRCH;
    };
    // SAFETY: the guest's attr, glibc layout (gated above); glibc initialises it first.
    unsafe {
        let rc = libc::pthread_attr_init(attr);
        if rc != 0 {
            return rc;
        }
        let rc = libc::pthread_attr_setstack(attr, s.base as *mut c_void, s.len);
        if rc != 0 {
            return rc;
        }
        libc::pthread_attr_setguardsize(attr, s.guard)
    }
}

pub(super) extern "C" fn pthread_key_delete(key: libc::pthread_key_t) -> i32 {
    thread::set_guest_dtor(key, 0);
    // SAFETY: a key of this process.
    unsafe { libc::pthread_key_delete(key) }
}
