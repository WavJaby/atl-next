// SPDX-License-Identifier: MIT
//! CPU access to a gralloc buffer's dma-buf: mapping it into the process and the bracket the kernel requires around the access (`DMA_BUF_IOCTL_SYNC`, linux/dma-buf.h). Users: the mapper (IMapper lock/unlock) and the Scene reader.
//! The bracket makes CPU caches coherent; it does not wait for GPU rendering (WSL software drivers return before the render lands) -- the producer must finish or fence before queueBuffer, and a reader waits on the acquire fence first.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};

use rustix::ioctl::{self, Setter, Updater, opcode};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};

/// `struct dma_buf_sync` (linux/dma-buf.h): `__u64 flags`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DmaBufSync {
    flags: u64,
}

/// `DMA_BUF_SYNC_READ`, `_WRITE`, `_RW`, `_START`, `_END` (linux/dma-buf.h: `1 << 0`, `2 << 0`, `READ | WRITE`, `0 << 2`, `1 << 2`).
pub const SYNC_READ: u64 = 1;
pub const SYNC_WRITE: u64 = 2;
const SYNC_START: u64 = 0;
const SYNC_END: u64 = 4;

/// `DMA_BUF_IOCTL_SYNC` = `_IOW('b', 0, struct dma_buf_sync)`.
fn sync(fd: BorrowedFd<'_>, flags: u64) -> io::Result<()> {
    // SAFETY: the opcode is DMA_BUF_IOCTL_SYNC (`_IOW('b', 0, struct dma_buf_sync)`), whose argument the kernel reads as a `struct dma_buf_sync` -- the `#[repr(C)]` mirror above.
    let req = unsafe {
        Setter::<{ opcode::write::<DmaBufSync>(b'b', 0) }, DmaBufSync>::new(DmaBufSync { flags })
    };
    // SAFETY: `req` is a well-formed DMA_BUF_IOCTL_SYNC request (above); the ioctl only reads it. A fd that is not a dma-buf fails with ENOTTY.
    unsafe { ioctl::ioctl(fd, req) }.map_err(io::Error::from)
}

/// Starts CPU access for `direction` (`SYNC_READ` / `SYNC_WRITE` bits): the kernel makes the buffer coherent for the CPU.
pub fn begin_cpu_access(fd: BorrowedFd<'_>, direction: u64) -> io::Result<()> {
    sync(fd, SYNC_START | direction)
}

/// Ends CPU access that began with `direction`: the kernel makes the CPU's writes visible to the device.
pub fn end_cpu_access(fd: BorrowedFd<'_>, direction: u64) -> io::Result<()> {
    sync(fd, SYNC_END | direction)
}

/// `struct dma_buf_export_sync_file` (linux/dma-buf.h, kernel 6.0+): `__u32 flags; __s32 fd;`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

/// The fences of `fd`'s implicit-sync reservation as one sync_file (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` = `_IOWR('b', 2, struct dma_buf_export_sync_file)`).
/// `direction` = the access the caller is about to make: `SYNC_WRITE` waits for every reader and writer (what a producer reusing the buffer needs), `SYNC_READ` only for writers. A dma-buf with no fences gives an already-signalled sync_file; a non-dma-buf fd fails with ENOTTY.
pub fn export_sync_file(fd: BorrowedFd<'_>, direction: u64) -> io::Result<OwnedFd> {
    let flags =
        u32::try_from(direction).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut arg = DmaBufExportSyncFile { flags, fd: -1 };
    // SAFETY: the opcode is DMA_BUF_IOCTL_EXPORT_SYNC_FILE (`_IOWR('b', 2, struct dma_buf_export_sync_file)`); the kernel reads `flags` and writes `fd` of the `#[repr(C)]` mirror above, which outlives the call.
    let req = unsafe {
        Updater::<{ opcode::read_write::<DmaBufExportSyncFile>(b'b', 2) }, DmaBufExportSyncFile>::new(
            &mut arg,
        )
    };
    // SAFETY: `req` is a well-formed request (above).
    unsafe { ioctl::ioctl(fd, req) }.map_err(io::Error::from)?;
    if arg.fd < 0 {
        return Err(io::Error::other("the kernel returned no sync_file"));
    }
    // SAFETY: on success the kernel installed a new descriptor `arg.fd` in this process that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(arg.fd) })
}

/// A shared, read-write mapping of a file descriptor, unmapped on drop.
#[derive(Debug)]
pub struct Mapping {
    ptr: *mut u8,
    len: usize,
    writable: bool,
}

// SAFETY: a `Mapping` is a pointer to process-wide mapped memory that no thread owns; the handle only unmaps it on drop. The bytes behind it are shared memory the kernel and other processes also touch, so every access is a raw-pointer access (`read`, `write`, `as_ptr`), never a Rust reference held across calls.
unsafe impl Send for Mapping {}
// SAFETY: see above; `&Mapping` gives only copies in and out through raw pointers.
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Maps the first `len` bytes of `fd` read-write and shared.
    pub fn new(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        Self::with(fd, len, ProtFlags::READ | ProtFlags::WRITE)
    }

    /// Maps the first `len` bytes of `fd` read-only and shared: a reader of another process's buffer; [`write`](Self::write) on it panics.
    pub fn read_only(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        Self::with(fd, len, ProtFlags::READ)
    }

    fn with(fd: BorrowedFd<'_>, len: usize, prot: ProtFlags) -> io::Result<Self> {
        // SAFETY: a null hint asks the kernel to choose the address; no existing mapping is touched. The result is checked by `mmap`'s `Result`.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                prot,
                MapFlags::SHARED,
                fd.as_fd(),
                0,
            )
        }?;
        Ok(Self {
            ptr: ptr.cast(),
            len,
            writable: prot.contains(ProtFlags::WRITE),
        })
    }

    /// The mapped address, handed to the client by `lock`.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// A copy of the mapped bytes (the metadata page is 4 KiB; copying is the point: no reference into shared memory outlives the call).
    pub fn read(&self) -> Vec<u8> {
        let mut v = vec![0u8; self.len];
        // SAFETY: `ptr` is valid for `len` bytes while `self` lives (mapped in `new`, unmapped in `drop`); `v` has `len` bytes; they cannot overlap (`v` is fresh). The copy is of plain bytes.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr, v.as_mut_ptr(), self.len) };
        v
    }

    /// Writes `bytes` at `offset` into the mapping. Panics when it does not fit (a caller bug: the offsets are constants of the page layout).
    pub fn write(&self, offset: usize, bytes: &[u8]) {
        assert!(self.writable, "write to a read-only mapping");
        assert!(
            offset
                .checked_add(bytes.len())
                .is_some_and(|end| end <= self.len),
            "write outside the mapping"
        );
        // SAFETY: the range is inside the mapping (asserted), `bytes` is a separate allocation; `ptr` is valid for `len` bytes while `self` lives.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(offset), bytes.len()) };
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what `mmap` returned for this mapping, unmapped once (here); nothing else holds a reference into it (see the `Send` justification).
        let _ = unsafe { munmap(self.ptr.cast(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};

    use super::*;

    fn memfd(len: u64) -> std::os::fd::OwnedFd {
        let fd = memfd_create("t", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&fd, len).expect("size");
        fd
    }

    /// Two mappings of one fd see each other's writes: the shared-page property `shared.rs` relies on.
    #[test]
    fn mappings_of_one_fd_share() {
        let fd = memfd(4096);
        let a = Mapping::new(fd.as_fd(), 4096).expect("map a");
        let b = Mapping::new(fd.as_fd(), 4096).expect("map b");
        a.write(100, b"hello");
        assert_eq!(&b.read()[100..105], b"hello");
        assert_eq!(a.read().len(), 4096);
    }

    /// A memfd is not a dma-buf: the sync ioctl must fail (ENOTTY), not pretend. (A real dma-buf is exercised by the gate.)
    #[test]
    fn sync_on_a_non_dmabuf_fails() {
        let fd = memfd(4096);
        let e = begin_cpu_access(fd.as_fd(), SYNC_READ | SYNC_WRITE).expect_err("not a dma-buf");
        assert_eq!(
            e.raw_os_error(),
            Some(rustix::io::Errno::NOTTY.raw_os_error())
        );
    }

    /// A read-only mapping refuses `write` before touching memory (the store would fault).
    #[test]
    #[should_panic(expected = "read-only")]
    fn write_to_a_read_only_mapping_panics() {
        let fd = memfd(4096);
        let m = Mapping::read_only(fd.as_fd(), 4096).expect("map");
        assert_eq!(m.read().len(), 4096);
        m.write(0, b"x");
    }

    #[test]
    #[should_panic(expected = "outside the mapping")]
    fn write_past_the_end_panics() {
        let fd = memfd(4096);
        Mapping::new(fd.as_fd(), 4096)
            .expect("map")
            .write(4094, b"abc");
    }
}
