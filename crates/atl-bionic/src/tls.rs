// SPDX-License-Identifier: MIT
//! bionic's thread-pointer slots (`__get_tls()`, build.rs `TLS_HEADER`): one array per thread, owned here.
//! bionic's `__get_tls()` returns the thread pointer, with slots MIN_TLS_SLOT..=MAX_TLS_SLOT around it; on
//! glibc the thread pointer is glibc's TCB, so the slots live in this library's `thread_local` instead.
//! Exported from the one atl-bionic library -> every DSO of the process reads and writes the same array.
//! Slot numbers are bionic's per-arch tls_defines.h (generated); some arches use negative slots, so the
//! returned pointer is the slot-0 position inside the array, as bionic's.

use core::cell::Cell;
use core::ffi::c_void;

use crate::generated::tls_defines::{MAX_TLS_SLOT, MIN_TLS_SLOT};

/// bionic's BIONIC_TLS_SLOTS.
const SLOTS: usize = (MAX_TLS_SLOT - MIN_TLS_SLOT + 1) as usize;

thread_local! {
    /// Const-initialised, no destructor: lives in the thread's static TLS block until the thread exits.
    static TLS: [Cell<*mut c_void>; SLOTS] = const { [const { Cell::new(core::ptr::null_mut()) }; SLOTS] };
}

/// This thread's slot 0 (may lie outside the array for arches whose slots are all negative, as bionic's).
pub fn slot_zero() -> *mut *mut c_void {
    TLS.with(|slots| {
        slots
            .as_ptr()
            .cast::<*mut c_void>()
            .cast_mut()
            .wrapping_offset(-(MIN_TLS_SLOT as isize))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tls_defines::TLS_SLOT_OPENGL_API;

    #[test]
    fn opengl_slot_inside_array() {
        let base = slot_zero() as usize;
        let start = TLS.with(|s| s.as_ptr() as usize);
        let slot =
            base.wrapping_add_signed(TLS_SLOT_OPENGL_API as isize * size_of::<usize>() as isize);
        assert!(slot >= start && slot < start + SLOTS * size_of::<usize>());
    }

    #[test]
    fn per_thread() {
        let here = slot_zero() as usize;
        let there = std::thread::spawn(|| slot_zero() as usize).join().unwrap();
        assert_ne!(here, there);
    }
}
