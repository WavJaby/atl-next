// SPDX-License-Identifier: MIT
//! The udmabuf tier: dma-bufs over memfd pages (ordinary cached system memory) through the kernel's `/dev/udmabuf` (`UDMABUF_CREATE`, linux/udmabuf.h).
//! atld asks for it only where no GPU in the process imports gralloc's buffers (node::choose_allocator_tier), so every access is a CPU access -- GDK's mmap download, the readback driver's writes, the mapper's locks -- and cached memory is what they read fast; a GPU tier's dumb buffers are write-combined (vgem: ~7 ms per 1440x900 read vs ~1.2 ms here).
//! What a buffer reports, and why it is honest: modifier LINEAR (plain rows, no tiling: the memory has no layout but the one we write down), pitch = row bytes rounded up to [`PITCH_ALIGN`], size = pitch x rows rounded up to a page (udmabuf maps whole pages).
//! Kernel rules (udmabuf.c `udmabuf_create`): the memfd must be shmem, sealed against shrinking (F_SEAL_SHRINK) and not against writes (F_SEAL_WRITE); offset and size page-aligned.
//! Unsafe: the one ioctl, and adopting the descriptor it returns.

use std::io;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::path::Path;

use atl_gralloc::descriptor::Checked;
use atl_gralloc::format::{BLOB, DRM_FORMAT_MOD_LINEAR};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};
use rustix::ioctl::{self, Ioctl, IoctlOutput, Opcode, opcode};

/// The kernel's udmabuf misc device.
pub const DEVICE: &str = atl_gralloc::node::UDMABUF_DEVICE;

/// Row pitch alignment: 256 bytes, the largest linear-pitch rule among Mesa's desktop drivers (radeonsi linear surfaces 256 B; iris 64 B), so a buffer of this tier stays importable if a GPU ever reads it; costs nothing at the window widths in use (1440 x 4 is a multiple).
pub const PITCH_ALIGN: u32 = 256;

/// The kernel's page size: udmabuf requires offset and size multiples of PAGE_SIZE (4 KiB on x86_64 / riscv64; 4, 16 or 64 KiB on aarch64 kernels).
pub fn page() -> u64 {
    rustix::param::page_size() as u64
}

/// `struct udmabuf_create` (linux/udmabuf.h): `__u32 memfd; __u32 flags; __u64 offset; __u64 size`.
#[repr(C)]
struct Create {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

/// `UDMABUF_FLAGS_CLOEXEC` (linux/udmabuf.h).
const FLAGS_CLOEXEC: u32 = 0x01;

/// `UDMABUF_CREATE = _IOW('u', 0x42, struct udmabuf_create)`; the new dma-buf descriptor is the ioctl's return value, which rustix's `Setter` drops, hence this impl.
struct CreateIoctl(Create);

// SAFETY: the opcode is UDMABUF_CREATE, whose argument the kernel only reads as `struct udmabuf_create` (the `#[repr(C)]` mirror above); the output is the return value, a descriptor.
unsafe impl Ioctl for CreateIoctl {
    type Output = IoctlOutput;
    const IS_MUTATING: bool = false;

    fn opcode(&self) -> Opcode {
        opcode::write::<Create>(b'u', 0x42)
    }

    fn as_ptr(&mut self) -> *mut std::ffi::c_void {
        std::ptr::from_mut(&mut self.0).cast()
    }

    unsafe fn output_from_ptr(
        out: IoctlOutput,
        _: *mut std::ffi::c_void,
    ) -> rustix::io::Result<IoctlOutput> {
        Ok(out)
    }
}

/// An open `/dev/udmabuf`.
pub struct Udmabuf {
    dev: OwnedFd,
}

/// (pitch, bytes) of `c` on this tier, bytes rounded up to the kernel's [`page`]; LINEAR only. BLOB: one row of `width` bytes, as the GBM tier reports it.
pub fn layout(c: &Checked) -> Result<(u32, u64), String> {
    layout_paged(c, page())
}

/// [`layout`] for pages of `page` bytes.
fn layout_paged(c: &Checked, page: u64) -> Result<(u32, u64), String> {
    if !c.modifiers.contains(&DRM_FORMAT_MOD_LINEAR) {
        return Err(format!(
            "udmabuf allocates LINEAR only; set {:#x?}",
            c.modifiers
        ));
    }
    let (pitch, rows) = if c.format.pixel_format == BLOB {
        // one row of `width` bytes (height is 1): contiguous by construction, unlike the GBM tier's BLOB_ROW_BYTES-wide R8 rows
        (c.width, 1)
    } else {
        let row = c
            .width
            .checked_mul(c.format.bytes_per_pixel)
            .ok_or("row bytes overflow")?;
        (row.next_multiple_of(PITCH_ALIGN), u64::from(c.height))
    };
    let bytes = (u64::from(pitch) * rows).next_multiple_of(page);
    Ok((pitch, bytes))
}

impl Udmabuf {
    /// Opens `path` read-write (UDMABUF_CREATE needs no more). EACCES: the user has neither a seat ACL nor `kvm` membership (systemd's 50-udev-default.rules: `GROUP="kvm"`, uaccess).
    pub fn open(path: &Path) -> io::Result<Self> {
        let dev = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        Ok(Self { dev: dev.into() })
    }

    /// A read-write, close-on-exec dma-buf of `bytes` (page-aligned) zero-filled bytes.
    pub fn create(&self, bytes: u64) -> io::Result<OwnedFd> {
        let memfd = memfd_create(
            "atl-gralloc-udmabuf",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?;
        ftruncate(&memfd, bytes)?;
        fcntl_add_seals(&memfd, SealFlags::SHRINK)?;
        let raw = std::os::fd::AsRawFd::as_raw_fd(&memfd);
        let req = CreateIoctl(Create {
            memfd: u32::try_from(raw).map_err(io::Error::other)?,
            flags: FLAGS_CLOEXEC,
            offset: 0,
            size: bytes,
        });
        // SAFETY: a well-formed UDMABUF_CREATE (above) on the udmabuf device; the kernel holds its own reference to the memfd's pages, so `memfd` may close after.
        let fd = unsafe { ioctl::ioctl(self.dev.as_fd(), req) }.map_err(io::Error::from)?;
        if fd < 0 {
            return Err(io::Error::other("UDMABUF_CREATE returned no descriptor"));
        }
        // SAFETY: on success the return value is a newly opened dma-buf descriptor nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(test)]
mod tests {
    use atl_gralloc::descriptor::Descriptor;
    use atl_gralloc::format::{DRM_FORMAT_MOD_LINEAR, RGBA_8888};
    use atl_gralloc::usage;

    use super::*;

    fn checked(width: i32, height: i32, modifiers: Vec<u64>) -> Checked {
        Descriptor {
            name: b"t".to_vec(),
            width,
            height,
            layer_count: 1,
            format: RGBA_8888,
            usage: usage::CPU_MASK,
            reserved_size: 0,
            option_names: vec![],
            modifiers,
        }
        .check()
        .expect("valid")
    }

    /// 1440 x 4 = 5760 = 22.5 x 256 -> 5888; 5888 x 900 = 5299200 = 1293.75 pages -> 1294 x 4096 = 5300224. 64 x 4 = 256 exactly; 256 x 48 = 12288 = 3 pages.
    #[test]
    fn pitch_and_size_follow_the_rules() {
        assert_eq!(
            layout_paged(&checked(1440, 900, vec![DRM_FORMAT_MOD_LINEAR]), 4096),
            Ok((5888, 5_300_224))
        );
        assert_eq!(
            layout_paged(&checked(64, 48, vec![DRM_FORMAT_MOD_LINEAR]), 4096),
            Ok((256, 12_288))
        );
        assert!(layout(&checked(64, 48, vec![1])).is_err());
    }

    /// Larger kernel pages (aarch64 16K / 64K): 5299200 bytes = 323.4 x 16384 -> 324 x 16384 = 5308416, = 80.9 x 65536 -> 81 x 65536 = 5308416; 12288 bytes -> one page each.
    #[test]
    fn size_rounds_to_the_kernel_page() {
        let big = checked(1440, 900, vec![DRM_FORMAT_MOD_LINEAR]);
        let small = checked(64, 48, vec![DRM_FORMAT_MOD_LINEAR]);
        assert_eq!(layout_paged(&big, 16_384), Ok((5888, 5_308_416)));
        assert_eq!(layout_paged(&big, 65_536), Ok((5888, 5_308_416)));
        assert_eq!(layout_paged(&small, 16_384), Ok((256, 16_384)));
        assert_eq!(layout_paged(&small, 65_536), Ok((256, 65_536)));
        assert_eq!(layout(&small).map(|(_, b)| b % page()), Ok(0));
    }

    /// A missing node is "unavailable", reported with the path's error kind (the tier choice reads it as a reason, not a failure).
    #[test]
    fn a_missing_device_is_an_error_not_a_panic() {
        let e = Udmabuf::open(Path::new("/nonexistent-udmabuf"))
            .err()
            .expect("no device");
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
    }

    /// Where this host offers udmabuf (WSL with the module, a distro kernel with a seat): the buffer has the asked size and maps writable; elsewhere nothing to check.
    #[test]
    fn creates_where_the_host_offers_it() {
        let Ok(u) = Udmabuf::open(Path::new(DEVICE)) else {
            return;
        };
        let fd = u.create(3 * page()).expect("create");
        let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0)).expect("size");
        assert_eq!(size, 3 * page());
        let mode = rustix::fs::fcntl_getfl(&fd).expect("flags") & rustix::fs::OFlags::ACCMODE;
        assert_ne!(mode, rustix::fs::OFlags::RDONLY);
    }
}
