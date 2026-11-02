// SPDX-License-Identifier: MIT
//! The C ABI of `BinderDriver.h` (signatures checked by `crate::generated`): each shell turns its raw arguments into safe values at once and calls `crate::process`. Failures return -1 (MAP_FAILED for mmap) with errno set, as the system calls they replace.

use core::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use core::mem::{offset_of, size_of};
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::OnceLock;

use atl_binder_wire::uapi::*;
use rustix::io::{Errno, fcntl_dupfd_cloexec};

use crate::process::{DEVICE, Process};

/// The environment variable naming the inherited control connection's fd (set by the broker / test harness).
pub const CONTROL_ENV: &str = "ATL_BINDER_FD";

/// The process's connection or why there is none, decided once: the inherited fd is taken exactly once, so a failed handshake is not retried on an fd number since reused.
static PROCESS: OnceLock<Result<Process, Errno>> = OnceLock::new();

fn connect() -> Result<Process, Errno> {
    let raw: RawFd = std::env::var(CONTROL_ENV)
        .map_err(|_| Errno::NOENT)?
        .parse()
        .map_err(|_| Errno::BADF)?;
    if raw < 0 {
        return Err(Errno::BADF);
    }
    // SAFETY: the broker passes this fd for the driver alone and nothing else in the process uses it; OnceLock runs this once, so this is its only owner. A wrong fd fails the socket-type check in `connect` and is closed.
    let control = unsafe { OwnedFd::from_raw_fd(raw) };
    Process::connect(control)
}

/// The process's connection, handshaken on first use.
fn process() -> Result<&'static Process, Errno> {
    PROCESS.get_or_init(connect).as_ref().map_err(|e| *e)
}

fn fail<T>(e: Errno, value: T) -> T {
    errno::set_errno(errno::Errno(e.raw_os_error()));
    value
}

fn int(r: Result<c_int, Errno>) -> c_int {
    r.unwrap_or_else(|e| fail(e, -1))
}

#[unsafe(no_mangle)]
pub extern "C" fn binder_driver_open(path: *const c_char, _flags: c_int) -> c_int {
    if path.is_null() {
        return fail(Errno::FAULT, -1);
    }
    // SAFETY: open(2)'s contract: a NUL-terminated path
    let path = unsafe { CStr::from_ptr(path) };
    if path.to_bytes() != DEVICE.as_bytes() {
        return fail(Errno::NOENT, -1);
    }
    int(process()
        .and_then(Process::dup)
        .map(std::os::fd::IntoRawFd::into_raw_fd))
}

/// `arg` as a `T` to read, or EFAULT when null.
///
/// # Safety
/// A non-null `arg` points to a readable `T`, as the ioctl's definition requires.
unsafe fn read_arg<T: Copy>(arg: *mut c_void) -> Result<T, Errno> {
    if arg.is_null() {
        return Err(Errno::FAULT);
    }
    // SAFETY: the caller's contract; unaligned reads tolerated
    Ok(unsafe { (arg as *const T).read_unaligned() })
}

#[unsafe(no_mangle)]
pub extern "C" fn binder_driver_ioctl(fd: c_int, request: c_ulong, arg: *mut c_void) -> c_int {
    if fd < 0 {
        return fail(Errno::BADF, -1);
    }
    let p = match process() {
        Ok(p) => p,
        Err(e) => return fail(e, -1),
    };
    let Ok(request) = u32::try_from(request) else {
        return fail(Errno::INVAL, -1);
    };
    let r = match request {
        // SAFETY: BINDER_WRITE_READ's argument is a binder_write_read
        BINDER_WRITE_READ => unsafe { write_read(p, arg) },
        BINDER_VERSION => {
            if arg.is_null() {
                Err(Errno::FAULT)
            } else {
                let v = binder_version {
                    protocol_version: p.binder_version(),
                };
                // SAFETY: BINDER_VERSION's argument is a writable binder_version
                unsafe { (arg as *mut binder_version).write_unaligned(v) };
                Ok(())
            }
        }
        BINDER_SET_MAX_THREADS => {
            // SAFETY: BINDER_SET_MAX_THREADS's argument is a __u32
            unsafe { read_arg::<u32>(arg) }.and_then(|n| p.set_max_threads(n))
        }
        BINDER_SET_CONTEXT_MGR_EXT => {
            // SAFETY: BINDER_SET_CONTEXT_MGR_EXT's argument is a flat_binder_object
            unsafe { read_arg::<[u8; size_of::<flat_binder_object>()]>(arg) }.and_then(|b| {
                let u64_at =
                    |o: usize| u64::from_ne_bytes(b[o..o + 8].try_into().expect("8 bytes"));
                let flags = offset_of!(flat_binder_object, flags);
                p.set_context_mgr(
                    u64_at(
                        offset_of!(flat_binder_object, anon0)
                            + offset_of!(flat_binder_object_anon0, binder),
                    ),
                    u64_at(offset_of!(flat_binder_object, cookie)),
                    u32::from_ne_bytes(b[flags..flags + 4].try_into().expect("4 bytes")),
                )
            })
        }
        BINDER_SET_CONTEXT_MGR => p.set_context_mgr(0, 0, 0),
        BINDER_THREAD_EXIT => {
            p.thread_exit();
            Ok(())
        }
        other => p.unsupported(other).and(Err(Errno::INVAL)),
    };
    int(r.map(|()| 0))
}

/// `BINDER_WRITE_READ`.
///
/// # Safety
/// `arg` points to a binder_write_read whose buffers are valid for their sizes, as the ioctl requires.
unsafe fn write_read(p: &Process, arg: *mut c_void) -> Result<(), Errno> {
    if arg.is_null() {
        return Err(Errno::FAULT);
    }
    // SAFETY: the caller's contract; libbinder passes an aligned stack binder_write_read it does not touch during the call
    let bwr = unsafe { &mut *(arg as *mut binder_write_read) };
    let span = |buf: u64, consumed: u64, size: u64| -> Result<(usize, usize), Errno> {
        let len = size.checked_sub(consumed).ok_or(Errno::FAULT)?;
        if len > 0 && buf == 0 {
            return Err(Errno::FAULT);
        }
        Ok(((buf + consumed) as usize, len as usize))
    };
    let (wp, wlen) = span(bwr.write_buffer, bwr.write_consumed, bwr.write_size)?;
    let (rp, rlen) = span(bwr.read_buffer, bwr.read_consumed, bwr.read_size)?;
    let commands: &[u8] = if wlen == 0 {
        &[]
    } else {
        // SAFETY: write_buffer[write_consumed..write_size] is the caller's readable command stream
        unsafe { core::slice::from_raw_parts(wp as *const u8, wlen) }
    };
    let read_buf: &mut [u8] = if rlen == 0 {
        &mut []
    } else {
        // SAFETY: read_buffer[read_consumed..read_size] is the caller's writable buffer, disjoint from the command stream
        unsafe { core::slice::from_raw_parts_mut(rp as *mut u8, rlen) }
    };
    let mut read_mem = |ptr: u64, len: usize| -> Vec<u8> {
        if len == 0 || ptr == 0 {
            return Vec::new();
        }
        // SAFETY: a transaction's data and offsets pointers name `len` readable bytes of the sending Parcel, which lives across the call
        unsafe { core::slice::from_raw_parts(ptr as *const u8, len) }.to_vec()
    };
    let mut dup_fd = |fd: i32| -> Result<OwnedFd, Errno> {
        if fd < 0 {
            return Err(Errno::BADF);
        }
        // SAFETY: an fd the Parcel holds open for the call (a BINDER_TYPE_FD object); borrowed only to duplicate it
        fcntl_dupfd_cloexec(unsafe { BorrowedFd::borrow_raw(fd) }, 0)
    };
    match p.write_read(commands, read_buf, &mut read_mem, &mut dup_fd) {
        Ok((w, r)) => {
            bwr.write_consumed += w as u64;
            bwr.read_consumed += r as u64;
            Ok(())
        }
        Err(e) => {
            bwr.write_consumed += e.write_consumed as u64;
            Err(e.errno)
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn binder_driver_mmap(
    _addr: *mut c_void,
    length: usize,
    _prot: c_int,
    _flags: c_int,
    fd: c_int,
    offset: i64,
) -> *mut c_void {
    const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;
    if fd < 0 || offset != 0 {
        return fail(Errno::INVAL, MAP_FAILED);
    }
    match process().and_then(|p| p.mmap(length)) {
        Ok(base) => base as *mut c_void,
        Err(e) => fail(e, MAP_FAILED),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn binder_driver_munmap(addr: *mut c_void, length: usize) -> c_int {
    int(process()
        .and_then(|p| p.munmap(addr as usize, length))
        .map(|()| 0))
}

#[unsafe(no_mangle)]
pub extern "C" fn binder_driver_close(fd: c_int) -> c_int {
    if fd < 0 {
        return fail(Errno::BADF, -1);
    }
    // SAFETY: close(2)'s contract: the caller owns `fd` (a driver fd from binder_driver_open) and gives it up
    drop(unsafe { OwnedFd::from_raw_fd(fd) });
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn binder_driver_poll_fd(fd: c_int) -> c_int {
    if fd < 0 {
        return fail(Errno::BADF, -1);
    }
    int(process().and_then(Process::poll_fd))
}
