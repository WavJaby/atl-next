// SPDX-License-Identifier: MIT
//! Reading a Scene buffer's pixels on the CPU: the only `unsafe` of the crate.
//! A quad's buffer is a gralloc handle (`atl-gralloc` `handle`): fd 0 is the dma-buf (or, in tests and for software producers, any mappable file such as a memfd) holding linear RGBA_8888 or RGBX_8888 rows of `stride` pixels.
//! Step 1 maps it, brackets the read with `DMA_BUF_IOCTL_SYNC` as the kernel requires for CPU access to a dma-buf, and copies the rows out; step 2 hands the dma-buf to the compositor as a texture instead.

use std::io;
use std::os::fd::BorrowedFd;

use atl_compositor::scene::{SceneBuffer, SceneFd};
use atl_gralloc::format::{self, COMPONENT_A, RGBA_8888, RGBX_8888};
use atl_gralloc::handle::FD_DMABUF;
use atl_gralloc::layout::Layout;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::fstat;
use rustix::io::Errno;
use rustix::ioctl::{self, Setter, opcode};
use rustix::mm::{MapFlags, ProtFlags, mmap, munmap};
use rustix::time::Timespec;

/// A buffer's pixels copied out of the producer's memory.
#[derive(Debug)]
pub struct Pixels {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Distance between row starts, in bytes.
    pub stride_bytes: usize,
    /// `stride_bytes * height` bytes in memory order R, G, B, A; A is 255 when the buffer has no alpha (RGBX_8888, whose byte 3 is undefined).
    pub bytes: Vec<u8>,
}

/// `struct dma_buf_sync` (linux/dma-buf.h): `__u64 flags`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DmaBufSync {
    flags: u64,
}

/// `DMA_BUF_SYNC_READ` (`1 << 0`), `_START` (`0 << 2`), `_END` (`1 << 2`) of linux/dma-buf.h.
const SYNC_READ: u64 = 1;
const SYNC_START: u64 = 0;
const SYNC_END: u64 = 4;

/// `DMA_BUF_IOCTL_SYNC` = `_IOW('b', 0, struct dma_buf_sync)`.
fn sync(fd: BorrowedFd<'_>, flags: u64) -> io::Result<()> {
    // SAFETY: the opcode is DMA_BUF_IOCTL_SYNC (`_IOW('b', 0, struct dma_buf_sync)`), whose argument the kernel reads as a `struct dma_buf_sync` -- the `#[repr(C)]` mirror above.
    let request = unsafe {
        Setter::<{ opcode::write::<DmaBufSync>(b'b', 0) }, DmaBufSync>::new(DmaBufSync { flags })
    };
    // SAFETY: `request` is a well-formed DMA_BUF_IOCTL_SYNC request; the ioctl only reads it. A fd that is not a dma-buf fails with ENOTTY.
    unsafe { ioctl::ioctl(fd, request) }.map_err(io::Error::from)
}

/// CPU access to a dma-buf needs the sync bracket; a plain file (memfd, shm) is coherent already and answers ENOTTY, which is not an error.
fn sync_if_dmabuf(fd: BorrowedFd<'_>, flags: u64) -> io::Result<()> {
    match sync(fd, flags) {
        Err(e) if e.raw_os_error() == Some(Errno::NOTTY.raw_os_error()) => Ok(()),
        other => other,
    }
}

/// The descriptor of a quad's fd object as a borrow for the duration of one call.
fn borrow(obj: &SceneFd) -> Result<BorrowedFd<'_>, String> {
    let fd = obj.raw();
    if fd < 0 {
        return Err(format!("fd number {fd} is not a descriptor"));
    }
    // SAFETY: a `SceneFd` is open in this process while the `Scene` holding it lives (its doc); the caller holds that scene for the whole call, and the borrow does not outlive `obj`.
    Ok(unsafe { BorrowedFd::borrow_raw(fd) })
}

/// A buffer's dma-buf as an importer sees it: the plane fds and the layout gralloc's handle states (`atl_gralloc::layout`, the same values the mapper's metadata answers). For a dmabuf texture / EGL image import; the CPU reader is [`read_pixels`].
#[derive(Debug)]
pub struct DmabufView<'a> {
    /// The layout.
    pub layout: Layout,
    /// One fd per plane, in `layout.planes` order.
    pub fds: Vec<BorrowedFd<'a>>,
}

impl<'a> DmabufView<'a> {
    /// The view of `buffer`, whose handle came from another process: its ints are validated as the mapper's import does, and each plane fd must exist.
    pub fn of(buffer: &'a SceneBuffer) -> Result<Self, String> {
        let layout = atl_gralloc::layout::from_handle(buffer.fds.len(), &buffer.ints)
            .map_err(|e| format!("not a gralloc handle: {e:?}"))?;
        let fds = layout
            .planes
            .iter()
            .map(|p| {
                buffer
                    .fds
                    .get(p.fd_index)
                    .ok_or_else(|| format!("plane fd {} is missing", p.fd_index))
                    .and_then(borrow)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { layout, fds })
    }
}

/// Whether a sync_file fence has signalled: it polls readable once it has (linux/sync_file.h). Never waits.
pub fn fence_signalled(fence: &SceneFd) -> Result<bool, String> {
    let fd = borrow(fence)?;
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let no_wait = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ready = poll(&mut fds, Some(&no_wait)).map_err(|e| format!("poll fence: {e}"))?;
    Ok(ready > 0)
}

/// A read-only shared mapping, unmapped on drop.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn new(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        // SAFETY: a null hint lets the kernel choose the address, so no existing mapping is touched; `fd` is open (borrowed) and the caller checked the file holds at least `len` bytes, so reads stay inside the file.
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::SHARED,
                fd,
                0,
            )
        }?;
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }

    /// A copy of the mapped bytes. A copy, because the producer may write the shared memory at any time and no reference into it may outlive the call.
    fn copy(&self) -> Vec<u8> {
        let mut v = vec![0u8; self.len];
        // SAFETY: `ptr` is valid for `len` bytes while `self` lives (mapped in `new`, unmapped in `drop`); `v` is a fresh allocation of `len` bytes, so the ranges cannot overlap; the copy is of plain bytes.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr, v.as_mut_ptr(), self.len) };
        v
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` are exactly what `mmap` returned for this mapping and it is unmapped once, here; the only access, `copy`, has ended.
        let _ = unsafe { munmap(self.ptr.cast(), self.len) };
    }
}

/// Copies `buffer`'s pixels out of its dma-buf. Every field comes from another process's parcel, so the geometry is checked against the format table and the fd's real size before anything is mapped (a mapping past the end of a file faults on access).
pub fn read_pixels(buffer: &SceneBuffer) -> Result<Pixels, String> {
    let info = format::lookup(buffer.format)
        .filter(|f| matches!(f.pixel_format, RGBA_8888 | RGBX_8888))
        .ok_or_else(|| {
            format!(
                "pixel format {} is not RGBA_8888 or RGBX_8888",
                buffer.format
            )
        })?;
    let (width, height, stride) = (
        u32::try_from(buffer.width).ok().filter(|&v| v > 0),
        u32::try_from(buffer.height).ok().filter(|&v| v > 0),
        u32::try_from(buffer.stride).ok(),
    );
    let (Some(width), Some(height), Some(stride)) = (width, height, stride) else {
        return Err(format!(
            "bad geometry {}x{} stride {}",
            buffer.width, buffer.height, buffer.stride
        ));
    };
    if stride < width {
        return Err(format!("stride {stride} is less than width {width}"));
    }
    let stride_bytes = usize::try_from(stride)
        .ok()
        .and_then(|s| s.checked_mul(info.bytes_per_pixel as usize))
        .ok_or("stride overflows")?;
    let len = stride_bytes
        .checked_mul(height as usize)
        .ok_or("buffer size overflows")?;
    let obj = buffer
        .fds
        .get(FD_DMABUF)
        .ok_or("the handle has no dma-buf fd")?;
    let fd = borrow(obj)?;
    let file_size = fstat(fd).map_err(|e| format!("fstat: {e}"))?.st_size;
    if u64::try_from(file_size).map_or(true, |size| size < len as u64) {
        return Err(format!(
            "fd holds {file_size} bytes, the geometry needs {len}"
        ));
    }
    let mapping = Mapping::new(fd, len).map_err(|e| format!("mmap: {e}"))?;
    sync_if_dmabuf(fd, SYNC_START | SYNC_READ).map_err(|e| format!("sync start: {e}"))?;
    let mut bytes = mapping.copy();
    sync_if_dmabuf(fd, SYNC_END | SYNC_READ).map_err(|e| format!("sync end: {e}"))?;
    if !info.components.iter().any(|c| c.0 == COMPONENT_A) {
        bytes.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
    }
    Ok(Pixels {
        width,
        height,
        stride_bytes,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd, OwnedFd};

    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
    use rustix::io::write;
    use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

    use super::*;

    fn memfd_with(bytes: &[u8]) -> OwnedFd {
        let fd = memfd_create("t", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&fd, bytes.len() as u64).expect("size");
        assert_eq!(write(&fd, bytes).expect("write"), bytes.len());
        fd
    }

    fn buffer(fd: &OwnedFd, width: i32, height: i32, stride: i32) -> SceneBuffer {
        SceneBuffer {
            width,
            height,
            stride,
            format: RGBA_8888,
            usage: 0,
            id: 1,
            fds: vec![SceneFd::new(fd.as_raw_fd())],
            ints: vec![],
        }
    }

    /// The view carries the allocator's fd and pitch from the handle ints, not the GraphicBuffer's own fields; a foreign handle is refused.
    #[test]
    fn dmabuf_view_reads_the_handle_layout() {
        use atl_gralloc::format::{DRM_FORMAT_ABGR8888, DRM_FORMAT_MOD_LINEAR};
        use atl_gralloc::handle::Params;
        let dmabuf = memfd_with(&[0; 64]);
        let page = memfd_with(&[0; 8]);
        // pitch 80 bytes for width 16: a padded row the allocator chose (20 pixels), different from the 16 a caller would assume
        let params = Params {
            width: 16,
            height: 2,
            pixel_format: RGBA_8888,
            fourcc: DRM_FORMAT_ABGR8888,
            pitch: 80,
            size: 160,
            usage: 0,
            modifier: DRM_FORMAT_MOD_LINEAR,
            id: 7,
        };
        let mut b = buffer(&dmabuf, 16, 2, 16);
        b.fds.push(SceneFd::new(page.as_raw_fd()));
        b.ints = params.to_ints().to_vec();
        let v = DmabufView::of(&b).expect("view");
        assert_eq!(v.fds.len(), 1);
        assert_eq!(v.fds[0].as_raw_fd(), dmabuf.as_raw_fd());
        assert_eq!(v.layout.planes[0].stride_bytes, 80);
        assert_eq!(v.layout.pixel_stride(), 20);
        assert_eq!(
            (v.layout.fourcc, v.layout.modifier),
            (DRM_FORMAT_ABGR8888, DRM_FORMAT_MOD_LINEAR)
        );
        b.ints[0] = 0;
        assert!(DmabufView::of(&b).is_err(), "foreign magic");
    }

    /// 2x2 pixels in rows of 3 pixels (stride 12 bytes): the padding pixel of each row is part of the copy, so the row pitch survives.
    #[test]
    fn copies_rows_with_their_stride() {
        let bytes: Vec<u8> = (0..24).collect();
        let fd = memfd_with(&bytes);
        let p = read_pixels(&buffer(&fd, 2, 2, 3)).expect("readable");
        assert_eq!((p.width, p.height, p.stride_bytes), (2, 2, 12));
        assert_eq!(p.bytes, bytes);
    }

    /// RGBX_8888: byte 3 of every pixel is padding, whatever the producer left there; it reads as opaque, colours untouched.
    #[test]
    fn rgbx_reads_opaque() {
        let fd = memfd_with(&[1, 2, 3, 0, 4, 5, 6, 77]);
        let mut b = buffer(&fd, 2, 1, 2);
        b.format = RGBX_8888;
        let p = read_pixels(&b).expect("readable");
        assert_eq!(p.bytes, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    /// A parcel can claim any geometry; a file shorter than it needs must be refused, not mapped (access past the end of a file faults).
    #[test]
    fn refuses_geometry_larger_than_the_file() {
        let fd = memfd_with(&[0u8; 16]);
        let e = read_pixels(&buffer(&fd, 2, 2, 3)).expect_err("needs 24 bytes");
        assert!(e.contains("holds 16 bytes"), "{e}");
    }

    #[test]
    fn refuses_foreign_input() {
        let fd = memfd_with(&[0u8; 64]);
        let mut b = buffer(&fd, 2, 2, 2);
        b.format = 4; // RGB_565: no row in the format table
        assert!(read_pixels(&b).expect_err("format").contains("RGBA_8888"));
        assert!(read_pixels(&buffer(&fd, 0, 2, 2)).is_err());
        assert!(
            read_pixels(&buffer(&fd, 4, 2, 3))
                .expect_err("stride")
                .contains("stride")
        );
        let mut none = buffer(&fd, 2, 2, 2);
        none.fds.clear();
        assert!(read_pixels(&none).expect_err("fd").contains("no dma-buf"));
        none.fds.push(SceneFd::new(-1));
        assert!(
            read_pixels(&none)
                .expect_err("fd")
                .contains("not a descriptor")
        );
    }

    /// A socket polls readable only once something is written: the sync_file fence contract (signalled = readable).
    #[test]
    fn fence_signalled_follows_readability() {
        let (r, w) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair");
        let obj = SceneFd::new(r.as_raw_fd());
        assert!(!fence_signalled(&obj).expect("poll"));
        write(&w, b"x").expect("write");
        assert!(fence_signalled(&obj).expect("poll"));
    }
}
