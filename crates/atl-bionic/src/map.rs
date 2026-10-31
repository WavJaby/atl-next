// SPDX-License-Identifier: MIT
//! The property-area file, mapped read-only and shared for the rest of the process.

use std::io;
use std::path::Path;
use std::sync::atomic::AtomicU32;

use rustix::fs::{Mode, OFlags};
use rustix::mm::{MapFlags, ProtFlags};

/// `path`'s bytes as words. Never unmapped: `prop_info` pointers handed to C stay valid for the
/// process lifetime, as bionic's do.
pub fn map_shared(path: &Path) -> io::Result<&'static [AtomicU32]> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let len = usize::try_from(rustix::fs::fstat(&fd)?.st_size)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    let words = len / size_of::<AtomicU32>();
    if words == 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // SAFETY: a new read-only shared mapping at an address the kernel chooses: it aliases no Rust
    // object. `fd` may close afterwards; the mapping stays.
    let ptr = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            len,
            ProtFlags::READ,
            MapFlags::SHARED,
            &fd,
            0,
        )
    }?;
    // SAFETY: `ptr` is page-aligned (so aligned for AtomicU32) and `words * 4 <= len` bytes are mapped
    // until the process ends, so `'static` holds. Other processes write the file concurrently; every
    // access here is an atomic load (Relaxed / Acquire, lock-free, 4 bytes), which Rust permits on
    // shared and on read-only memory.
    Ok(unsafe { std::slice::from_raw_parts(ptr.cast::<AtomicU32>(), words) })
}
