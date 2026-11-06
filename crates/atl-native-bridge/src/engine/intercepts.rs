// SPDX-License-Identifier: MIT
//! Host sides of the guest libc's pthread stubs (NBS `stubs_x86_64.cpp`): guest threads are glibc threads, as NBS makes them host threads.
//! Pass-through to glibc by name where the ABI is the same: `pthread_t` (long), `pthread_key_t` (int), `pthread_attr_t` (56 bytes, only ever filled and read by glibc, since every `pthread_attr_*` is intercepted too) -- sizes gated below against the generated bionic layout.
//! Own host sides where the guest hands host code a guest function: `pthread_create` (start routine on the new thread's guest thread pointer, after attach) and `pthread_key_create` (destructor run by thread.rs `detach`).

use std::ffi::c_void;
use std::sync::OnceLock;

use super::thread::layout;
use super::{arch, thread};

// the pass-through set's types: bionic guest view == glibc host view
const _: () = {
    assert!(layout::pthread_attr_t::SIZE == size_of::<libc::pthread_attr_t>());
    assert!(layout::pthread_attr_t::ALIGN == align_of::<libc::pthread_attr_t>());
    assert!(size_of::<libc::pthread_t>() == size_of::<std::ffi::c_long>());
    assert!(size_of::<libc::pthread_key_t>() == size_of::<std::ffi::c_int>());
};

/// Stubs glibc serves with the same ABI.
const GLIBC_SAME: &[&str] = &[
    "pthread_attr_destroy",
    "pthread_attr_getdetachstate",
    "pthread_attr_getguardsize",
    "pthread_attr_getinheritsched",
    "pthread_attr_getschedparam",
    "pthread_attr_getschedpolicy",
    "pthread_attr_getscope",
    "pthread_attr_getstack",
    "pthread_attr_getstacksize",
    "pthread_attr_init",
    "pthread_attr_setdetachstate",
    "pthread_attr_setguardsize",
    "pthread_attr_setinheritsched",
    "pthread_attr_setschedparam",
    "pthread_attr_setschedpolicy",
    "pthread_attr_setscope",
    "pthread_attr_setstack",
    "pthread_attr_setstacksize",
    "pthread_detach",
    "pthread_getattr_np",
    "pthread_getcpuclockid",
    "pthread_getname_np",
    "pthread_getschedparam",
    "pthread_join",
    "pthread_kill",
    "pthread_setname_np",
    "pthread_setschedparam",
    "pthread_setschedprio",
    "pthread_sigqueue",
    "pthread_getspecific",
    "pthread_setspecific",
];

/// glibc's own definition of `name` (its `libc.so.6`, not whatever the global scope finds first: libatl_bionic exports bionic names).
fn glibc(name: &str) -> Option<usize> {
    static LIBC: OnceLock<usize> = OnceLock::new();
    let h = *LIBC.get_or_init(|| {
        // SAFETY: dlopen of an already loaded library (RTLD_NOLOAD): no constructors run.
        unsafe { libc::dlopen(c"libc.so.6".as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) as usize }
    });
    if h == 0 {
        return None;
    }
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: dlsym on a live handle with a NUL-terminated name.
    let p = unsafe { libc::dlsym(h as *mut c_void, c.as_ptr()) };
    (!p.is_null()).then_some(p as usize)
}

/// The host side of guest pthread stub `name`, if this module serves it.
pub fn host_fn(name: &str) -> Option<usize> {
    match name {
        "pthread_create" => Some(pthread_create as *const () as usize),
        "pthread_key_create" => Some(pthread_key_create as *const () as usize),
        "pthread_key_delete" => Some(pthread_key_delete as *const () as usize),
        _ if GLIBC_SAME.contains(&name) => glibc(name),
        _ => None,
    }
}

struct Start {
    routine: usize,
    arg: usize,
}

/// `int pthread_create(pthread_t*, const pthread_attr_t*, void* (*)(void*), void*)`: a glibc thread whose start routine is the guest's, run after the thread attaches.
extern "C" fn pthread_create(
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
extern "C" fn pthread_key_create(key: *mut libc::pthread_key_t, dtor: usize) -> i32 {
    // SAFETY: `key` is the guest's, same ABI (gated above).
    let rc = unsafe { libc::pthread_key_create(key, None) };
    if rc == 0 {
        // SAFETY: written by glibc just now.
        thread::set_guest_dtor(unsafe { *key }, dtor);
    }
    rc
}

extern "C" fn pthread_key_delete(key: libc::pthread_key_t) -> i32 {
    thread::set_guest_dtor(key, 0);
    // SAFETY: a key of this process.
    unsafe { libc::pthread_key_delete(key) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_pass_through_name_is_glibcs() {
        for name in super::GLIBC_SAME {
            assert!(super::glibc(name).is_some(), "glibc lacks {name}");
        }
    }
}
