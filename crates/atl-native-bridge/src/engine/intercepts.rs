// SPDX-License-Identifier: MIT
//! Host sides of the guest libc's pthread stubs (NBS `stubs_x86_64.cpp`): guest threads are glibc threads, as NBS makes them host threads.
//! Pass-through to glibc where the ABI is the same (build.rs `GLIBC` rows of the binding table): `pthread_t` (long), `pthread_key_t` (int), `pthread_attr_t` (56 bytes, only ever filled and read by glibc, since every `pthread_attr_*` is a glibc row too) -- sizes gated below against the generated bionic layout.
//! Own host sides (`Shim` rows) where the guest hands host code a guest function: `pthread_create` (start routine on the new thread's guest thread pointer, after attach) and `pthread_key_create` (destructor run by thread.rs `detach`).

use std::ffi::c_void;

use super::thread::layout;
use super::{arch, thread};

// the pass-through set's types: bionic guest view == glibc host view
const _: () = {
    assert!(layout::pthread_attr_t::SIZE == size_of::<libc::pthread_attr_t>());
    assert!(layout::pthread_attr_t::ALIGN == align_of::<libc::pthread_attr_t>());
    assert!(size_of::<libc::pthread_t>() == size_of::<std::ffi::c_long>());
    assert!(size_of::<libc::pthread_key_t>() == size_of::<std::ffi::c_int>());
};

struct Start {
    routine: usize,
    arg: usize,
}

/// `int pthread_create(pthread_t*, const pthread_attr_t*, void* (*)(void*), void*)`: a glibc thread whose start routine is the guest's, run after the thread attaches.
pub(super) extern "C" fn pthread_create(
    thread: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    routine: usize,
    arg: usize,
) -> i32 {
    let start = Box::into_raw(Box::new(Start { routine, arg }));
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
    let Start { routine, arg } = *unsafe { Box::from_raw(start.cast::<Start>()) };
    let ht = thread::attach_current();
    // SAFETY: the guest's `void* (*)(void*)`, this thread's record.
    unsafe { arch::call_guest(&mut *ht, routine, arg, 0, 0, 0) as *mut c_void }
}

/// `int pthread_key_create(pthread_key_t*, void (*)(void*))`: glibc's key without a destructor; the guest's runs at detach.
pub(super) extern "C" fn pthread_key_create(key: *mut libc::pthread_key_t, dtor: usize) -> i32 {
    // SAFETY: `key` is the guest's, same ABI (gated above).
    let rc = unsafe { libc::pthread_key_create(key, None) };
    if rc == 0 {
        // SAFETY: written by glibc just now.
        thread::set_guest_dtor(unsafe { *key }, dtor);
    }
    rc
}

pub(super) extern "C" fn pthread_key_delete(key: libc::pthread_key_t) -> i32 {
    thread::set_guest_dtor(key, 0);
    // SAFETY: a key of this process.
    unsafe { libc::pthread_key_delete(key) }
}
