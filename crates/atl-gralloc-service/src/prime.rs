// SPDX-License-Identifier: MIT
//! Exporting a GEM buffer as a dma-buf the way the CPU path needs it: close-on-exec and read-write (`DRM_IOCTL_PRIME_HANDLE_TO_FD`, drm.h).
//! GBM's own export (`gbm_bo_get_fd`) is read-only on Mesa's software (KMS dumb-buffer) driver, the one behind vgem: it calls `drmPrimeHandleToFD(..., DRM_CLOEXEC)` without `DRM_RDWR` (measured 2026-11-04: the descriptor is O_RDONLY, and a dma-buf cannot be reopened through /proc: ENXIO).
//! The kernel refuses a shared writable `mmap` of a descriptor opened read-only (EACCES), and the mapper maps what it is sent. Hardware drivers export read-write; exporting the GEM handle ourselves makes every driver alike.
//! The only `unsafe` of the service: the ioctl and the adoption of the descriptor it returns, and the GEM handle read out of GBM's handle union.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};

use gbm::BufferObject;
use rustix::fs::OFlags;
use rustix::ioctl::{self, Updater, opcode};

/// `struct drm_prime_handle` (drm.h): `__u32 handle; __u32 flags; __s32 fd` (fd is the result).
#[repr(C)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

/// `DRM_CLOEXEC | DRM_RDWR` (drm.h: `O_CLOEXEC`, `O_RDWR`).
const EXPORT_FLAGS: u32 = OFlags::CLOEXEC.bits() | OFlags::RDWR.bits();

/// The GEM handle of `bo`, valid on the device descriptor `bo` was created from.
pub fn gem_handle<T: 'static>(bo: &BufferObject<T>) -> u32 {
    // SAFETY: `gbm_bo_get_handle` returns a union of which the DRM backends set `u32_`, the GEM handle (gbm.h `union gbm_bo_handle`: "u32" is what drivers fill for a DRM device); reading it as `u32` is reading the initialised member.
    unsafe { bo.handle().u32_ }
}

/// A read-write, close-on-exec dma-buf descriptor for the GEM `handle` of the DRM device `dev`.
pub fn export_dmabuf(dev: BorrowedFd<'_>, handle: u32) -> io::Result<OwnedFd> {
    let mut req = PrimeHandle {
        handle,
        flags: EXPORT_FLAGS,
        fd: -1,
    };
    // SAFETY: the opcode is DRM_IOCTL_PRIME_HANDLE_TO_FD (`_IOWR('d', 0x2d, struct drm_prime_handle)`), whose argument the kernel reads and writes as a `struct drm_prime_handle` -- the `#[repr(C)]` mirror above.
    let ioc = unsafe {
        Updater::<{ opcode::read_write::<PrimeHandle>(b'd', 0x2d) }, PrimeHandle>::new(&mut req)
    };
    // SAFETY: `ioc` is a well-formed request (above) on a DRM device descriptor; a non-DRM descriptor fails with ENOTTY.
    unsafe { ioctl::ioctl(dev.as_fd(), ioc) }.map_err(io::Error::from)?;
    if req.fd < 0 {
        return Err(io::Error::other(
            "PRIME_HANDLE_TO_FD returned no descriptor",
        ));
    }
    // SAFETY: on success the kernel stored a newly opened descriptor in `fd`, which nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(req.fd) })
}
