// SPDX-License-Identifier: MIT
//! Host sides of the guest libc's pthread stubs (NBS `stubs_x86_64.cpp`): guest threads are glibc threads, as NBS makes them host threads.
//! Pass-through to glibc where the ABI is the same (build.rs `GLIBC` rows of the binding table): `pthread_key_t` (int), `pthread_attr_t` (56 bytes, only ever filled and read by glibc, since every `pthread_attr_*` is a glibc row too) -- sizes gated below against the generated bionic layout.
//! A `pthread_t` is the guest's own handle, the thread's bionic record (thread.rs `THREADS`): every row taking one maps it to glibc's (`pthread_join`, `pthread_detach`, `pthread_gettid_np`, `pthread_getattr_np`, [`BY_HANDLE`]); `pthread_self` and `pthread_equal` are guest bionic and agree with `pthread_create`'s answer.
//! Own host sides (`Shim` rows) where the guest hands host code a guest function: `pthread_create` (start routine on the new thread's guest thread pointer, after attach on a guest stack of the attr's size) and `pthread_key_create` (destructor run by thread.rs `detach`); and `pthread_getattr_np`, whose answer is the guest stack (thread.rs `stack_of`), not glibc's view of the host thread. And `pthread_exit`, which ends the start routine's guest call (`Engine::end_call`).

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
    /// thread.rs `prepare`'s record: the guest's handle of the new thread
    record: usize,
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

unsafe extern "C" {
    /// glibc's (POSIX); the libc crate binds only the setter.
    fn pthread_attr_getdetachstate(attr: *const libc::pthread_attr_t, state: *mut i32) -> i32;
}

/// Whether `attr` (null: default) leaves the thread joinable.
fn joinable(attr: *const libc::pthread_attr_t) -> bool {
    if attr.is_null() {
        return true;
    }
    let mut state = libc::PTHREAD_CREATE_JOINABLE;
    // SAFETY: the guest's initialised attr, glibc layout (gated above).
    unsafe { pthread_attr_getdetachstate(attr, &mut state) };
    state == libc::PTHREAD_CREATE_JOINABLE
}

/// `int pthread_create(pthread_t*, const pthread_attr_t*, void* (*)(void*), void*)`: a glibc thread whose start routine is the guest's, run after the thread attaches on a guest stack sized by `attr` (the glibc host thread gets `attr` too).
/// `*thread` is the new thread's bionic record (thread.rs `prepare`), the value its guest `pthread_self` answers, never glibc's handle: every `pthread_t` row maps it back (thread.rs `glibc_of`). Returns once the thread has written its tid, as bionic's `CLONE_PARENT_SETTID` has it at return.
pub(super) extern "C" fn pthread_create(
    thread: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    routine: usize,
    arg: usize,
) -> i32 {
    let stack = stack_req(attr);
    let record = thread::prepare(joinable(attr));
    let start = Box::into_raw(Box::new(Start {
        routine,
        arg,
        stack,
        record,
    }));
    let mut glibc: libc::pthread_t = 0;
    // SAFETY: `attr` is the guest's, same ABI (gated above); `start` is ours until `run` takes it.
    let rc = unsafe { libc::pthread_create(&mut glibc, attr, run, start.cast()) };
    if rc != 0 {
        // SAFETY: not handed to a thread.
        drop(unsafe { Box::from_raw(start) });
        thread::started(record, 0);
        return rc;
    }
    thread::started(record, glibc);
    while thread::tid_of(record).is_some_and(|t| t == 0) {
        std::thread::yield_now();
    }
    // deliberate break (`ATL_NB_BREAK=qt_handle_glibc`): the guest gets glibc's handle, as before tr-first blocker 4 (qemu-thread gate `q`)
    let handle = if cfg!(atl_nb_qt_handle_glibc) {
        glibc
    } else {
        record as libc::pthread_t
    };
    // SAFETY: the guest's out-parameter, `pthread_t` = long (gated above).
    unsafe { thread.write(handle) };
    0
}

extern "C" fn run(start: *mut c_void) -> *mut c_void {
    // SAFETY: the box `pthread_create` made for this thread.
    let Start {
        routine,
        arg,
        stack,
        record,
    } = *unsafe { Box::from_raw(start.cast::<Start>()) };
    thread::adopt_record(record);
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

/// `void pthread_exit(void*)`: ends the start routine's guest call at once with `value` as its result (`Engine::end_call`; QEMU answers it in x0, hence the return), so [`run`] returns it to glibc, whose join reports it; the host thread then exits as after a plain return.
/// bionic's order (pthread_exit.cpp): `__cxa_thread_finalize`, cleanup handlers, `pthread_key_clean_all`, then the stack goes. The first two are guest-libc NBS stubs with no host side here (an app that registers either traps first); key destructors and the guest stack are thread.rs `detach`'s, run by the host key at this thread's exit.
/// Only from the routine's own guest call on a [`run`] thread (the engine refuses a nested one: it would skip host frames between the calls); a host thread that merely attached has no routine to end. Both abort, named.
pub(super) extern "C" fn pthread_exit(value: usize) -> usize {
    if !ROUTINE.with(Cell::get) {
        super::unsupported("guest pthread_exit outside a guest thread's start routine");
    }
    // deliberate break (`ATL_NB_BREAK=qt_exit_resume`): the call is not ended, so the guest resumes after its noreturn pthread_exit (qemu-thread gate `x`)
    if cfg!(atl_nb_qt_exit_resume) {
        return value;
    }
    // SAFETY: this thread's record (a run thread attached before its routine), called from the guest's pthread_exit crossing; nothing here needs a drop.
    match unsafe { super::active().end_call(&mut *thread::attach_current(), value) } {
        Ok(v) => v,
        Err(e) => super::unsupported(&format!("guest pthread_exit: {e}")),
    }
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

/// `int pthread_join(pthread_t, void**)`: glibc's join of the handle's thread, then its record goes (thread.rs `joined`). ESRCH for a handle of no guest thread.
pub(super) extern "C" fn pthread_join(handle: usize, value: *mut *mut c_void) -> i32 {
    let Some(glibc) = thread::glibc_of(handle) else {
        return libc::ESRCH;
    };
    // SAFETY: a guest thread's glibc handle; `value` is the guest's out-parameter or null.
    let rc = unsafe { libc::pthread_join(glibc, value) };
    if rc == 0 {
        thread::joined(handle);
    }
    rc
}

/// `int pthread_detach(pthread_t)`: glibc's detach, then the record goes at the thread's exit, or now if it exited (thread.rs `detached`).
pub(super) extern "C" fn pthread_detach(handle: usize) -> i32 {
    let Some(glibc) = thread::glibc_of(handle) else {
        return libc::ESRCH;
    };
    // SAFETY: a guest thread's glibc handle.
    let rc = unsafe { libc::pthread_detach(glibc) };
    if rc == 0 {
        thread::detached(handle);
    }
    rc
}

/// `pid_t pthread_gettid_np(pthread_t)` (bionic-only, glibc before 2.42 has none): the record's tid; -1 for a handle of no guest thread (bionic answers -1 for an exited one's 0 too, as here once the thread is gone).
pub(super) extern "C" fn pthread_gettid_np(handle: usize) -> i32 {
    thread::tid_of(handle).filter(|&t| t != 0).unwrap_or(-1)
}

/// The glibc rows whose first parameter is a `pthread_t` and that leave the thread's life alone: the handle mapped (thread.rs `glibc_of`), the rest passed as they are. Every such function takes at most three more int lanes (`pthread_sigqueue`'s `union sigval` is one, an INTEGER-class union in SysV and one register in AAPCS64) and answers an `int` errno.
pub(super) const BY_HANDLE: [&str; 8] = [
    "pthread_getcpuclockid",
    "pthread_getname_np",
    "pthread_getschedparam",
    "pthread_kill",
    "pthread_setname_np",
    "pthread_setschedparam",
    "pthread_setschedprio",
    "pthread_sigqueue",
];

/// Host side of `BY_HANDLE[I]`: glibc's function on the handle's glibc `pthread_t`; ESRCH for a handle of no guest thread.
extern "C" fn by_handle<const I: usize>(handle: usize, a: usize, b: usize, c: usize) -> i32 {
    type F = unsafe extern "C" fn(libc::pthread_t, usize, usize, usize) -> i32;
    let Some(glibc) = thread::glibc_of(handle) else {
        return libc::ESRCH;
    };
    let f = super::bindings::glibc_symbol(BY_HANDLE[I])
        .unwrap_or_else(|| super::unsupported(&format!("glibc lacks {}", BY_HANDLE[I])));
    // SAFETY: glibc's export of that name, whose prototype is a pthread_t then at most three int lanes (BY_HANDLE's doc); lanes it does not take are ignored.
    let f: F = unsafe { std::mem::transmute(f) };
    // SAFETY: as above; the lanes are the guest's arguments, same ABI as glibc's (bionic and glibc agree on these prototypes on LP64).
    unsafe { f(glibc, a, b, c) }
}

const BY_HANDLE_FNS: [extern "C" fn(usize, usize, usize, usize) -> i32; 8] = [
    by_handle::<0>,
    by_handle::<1>,
    by_handle::<2>,
    by_handle::<3>,
    by_handle::<4>,
    by_handle::<5>,
    by_handle::<6>,
    by_handle::<7>,
];

/// The host side of `BY_HANDLE` row `name`, else None.
pub(super) fn by_handle_fn(name: &str) -> Option<usize> {
    let i = BY_HANDLE.iter().position(|n| *n == name)?;
    Some(BY_HANDLE_FNS[i] as usize)
}

pub(super) extern "C" fn pthread_key_delete(key: libc::pthread_key_t) -> i32 {
    thread::set_guest_dtor(key, 0);
    // SAFETY: a key of this process.
    unsafe { libc::pthread_key_delete(key) }
}
