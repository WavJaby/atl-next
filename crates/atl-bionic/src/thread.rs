// SPDX-License-Identifier: MIT
//! `pthread_gettid_np` where glibc lacks it (< 2.42): the kernel thread id of a `pthread_t`. glibc's
//! `pthread_t` is the address of its thread descriptor, whose `tid` field glibc publishes for debuggers:
//! libc.so exports `_thread_db_pthread_tid` (GLIBC_PRIVATE; nptl `db_info.c`
//! `DB_STRUCT_FIELD (pthread, tid)`), a `uint32_t[3]` of {field bits, element count, byte offset} --
//! what libthread_db reads the tid with, and the field glibc 2.42's own `pthread_gettid_np` returns.
//! The kernel writes the field (set at clone, cleared at exit), so it is read atomically.

use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

unsafe extern "C" {
    // glibc's libc (>= 2.34 holds libdl)
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// glibc's `RTLD_DEFAULT` (`<dlfcn.h>`: `((void *) 0)`): the global lookup scope.
const RTLD_DEFAULT: *mut c_void = core::ptr::null_mut();

/// The `tid` field's byte offset in glibc's thread descriptor; none when glibc publishes no descriptor,
/// or one that is not a single 32-bit field (`pid_t`).
fn tid_offset() -> Option<usize> {
    static OFFSET: OnceLock<Option<usize>> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        // SAFETY: a NUL-terminated symbol name in the global scope
        let desc = unsafe { dlsym(RTLD_DEFAULT, c"_thread_db_pthread_tid".as_ptr()) };
        if desc.is_null() {
            return None;
        }
        // SAFETY: glibc defines the symbol as `const uint32_t[3]`
        let [bits, count, offset] = unsafe { desc.cast::<[u32; 3]>().read() };
        (bits == i32::BITS && count == 1).then_some(offset as usize)
    })
}

/// bionic `pthread_gettid_np`: `thread`'s kernel thread id; -1 where glibc publishes no descriptor (as
/// bionic's for a thread it does not know).
///
/// # Safety
/// `thread` is a `pthread_t` of a thread of this process that has not been joined or detached-and-exited
/// (bionic's contract: its descriptor is mapped).
pub unsafe fn gettid_of(thread: usize) -> i32 {
    let Some(offset) = tid_offset() else {
        return -1;
    };
    // SAFETY: the caller's contract: the descriptor is live; glibc's offset lies inside it and the
    // field is an aligned pid_t
    unsafe { (*((thread + offset) as *const AtomicI32)).load(Ordering::Acquire) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::thread::JoinHandleExt;
    use std::sync::{Barrier, mpsc};

    unsafe extern "C" {
        fn pthread_self() -> usize;
    }

    fn kernel_tid() -> i32 {
        rustix::thread::gettid().as_raw_nonzero().get()
    }

    /// Expected values from the kernel: each thread's own gettid(2), for the caller and for another
    /// live thread.
    #[test]
    fn tid_of_self_and_of_another_thread() {
        // SAFETY: the calling thread is live
        assert_eq!(unsafe { gettid_of(pthread_self()) }, kernel_tid());
        let (tx, rx) = mpsc::channel();
        let hold = std::sync::Arc::new(Barrier::new(2));
        let held = hold.clone();
        let t = std::thread::spawn(move || {
            tx.send(kernel_tid()).expect("send");
            held.wait();
        });
        let theirs = rx.recv().expect("recv");
        assert_ne!(theirs, kernel_tid());
        // SAFETY: the thread waits on the barrier, so its descriptor is live
        let read = unsafe { gettid_of(t.as_pthread_t() as usize) };
        hold.wait();
        t.join().expect("join");
        assert_eq!(read, theirs);
    }
}
