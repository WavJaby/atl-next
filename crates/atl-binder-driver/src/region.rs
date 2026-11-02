// SPDX-License-Identifier: MIT
//! The process's binder region: anonymous memory this crate maps and alone writes (binderd's copies and fd numbers); libbinder reads delivered buffers from it.

use core::ffi::c_void;
use core::ptr::null_mut;

use rustix::io::Errno;
use rustix::mm::{MapFlags, ProtFlags, mmap_anonymous, munmap};

/// The mapped region: base address and length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    base: usize,
    len: usize,
}

impl Region {
    /// Maps `len` bytes, private, read-write (libbinder asks PROT_READ; binderd's bytes are written by this process).
    pub fn map(len: usize) -> Result<Self, Errno> {
        // SAFETY: a fresh anonymous mapping at an address the kernel picks overlaps nothing that exists
        let p = unsafe {
            mmap_anonymous(
                null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::PRIVATE | MapFlags::NORESERVE,
            )
        }?;
        Ok(Self {
            base: p as usize,
            len,
        })
    }

    pub fn base(&self) -> usize {
        self.base
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Unmaps it.
    pub fn unmap(self) -> Result<(), Errno> {
        // SAFETY: this region's own mapping; libbinder unmaps only at ProcessState teardown, when no buffer is in use
        unsafe { munmap(self.base as *mut c_void, self.len) }
    }

    /// Writes `bytes` at `offset`; false when they do not fit.
    pub fn write(&self, offset: u64, bytes: &[u8]) -> bool {
        let Ok(off) = usize::try_from(offset) else {
            return false;
        };
        if off
            .checked_add(bytes.len())
            .is_none_or(|end| end > self.len)
        {
            return false;
        }
        // SAFETY: inside the mapping (checked above), which stays mapped while the process uses the driver; binderd allocated this range to a buffer not yet delivered, so libbinder holds no reference into it
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (self.base + off) as *mut u8,
                bytes.len(),
            );
        }
        true
    }
}
