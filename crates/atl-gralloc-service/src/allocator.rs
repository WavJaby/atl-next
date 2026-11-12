// SPDX-License-Identifier: MIT
//! One allocation: a checked descriptor becomes a dma-buf from the backend, the shared metadata page, and the handle parameters (`atl_gralloc::handle::Params`) the mapper parses in the importing process.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use atl_gralloc::descriptor::Checked;
use atl_gralloc::handle::Params;
use atl_gralloc::shared;
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};

use crate::backend::{Backend, BackendError};

/// One buffer: its two descriptors in handle order (`handle::FD_DMABUF`, `handle::FD_PAGE`) and the ints' source.
#[derive(Debug)]
pub struct Allocation {
    pub dmabuf: OwnedFd,
    pub page: OwnedFd,
    pub params: Params,
}

#[derive(Debug)]
pub enum AllocError {
    Backend(BackendError),
    /// The metadata page could not be made.
    Page(String),
    /// The device returned a buffer that contradicts the request (smaller pitch than a row).
    Inconsistent(&'static str),
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(e) => write!(f, "{e}"),
            Self::Page(why) => write!(f, "metadata page: {why}"),
            Self::Inconsistent(why) => write!(f, "device returned an inconsistent buffer: {why}"),
        }
    }
}

impl std::error::Error for AllocError {}

impl From<BackendError> for AllocError {
    fn from(e: BackendError) -> Self {
        Self::Backend(e)
    }
}

/// The allocator: a backend, and the counter that makes buffer ids unique.
pub struct Allocator {
    backend: Backend,
    counter: AtomicU32,
    /// Requests refused because the device policy left no modifier for their usage (`Refusal::NoModifierForUsage`).
    refused_no_modifier: AtomicU64,
    /// The [`Self::log_key`] last printed.
    counters_logged: Mutex<LogKey>,
    /// Modifiers already named by a `gralloc: modifier ... first used` line.
    modifiers_seen: Mutex<Vec<u64>>,
    pid: u32,
}

/// (modifier_invalid_linear, refused_no_modifier, alloc_linear != 0, alloc_modifier != 0)
type LogKey = (u64, u64, bool, bool);

impl Allocator {
    pub fn new(backend: Backend) -> Self {
        let a = Self {
            backend,
            counter: AtomicU32::new(1),
            refused_no_modifier: AtomicU64::new(0),
            counters_logged: Mutex::new((0, 0, false, false)),
            modifiers_seen: Mutex::new(Vec::new()),
            pid: std::process::id(),
        };
        // the probe allocation may already have counted; the start line prints that value
        let key = a.log_key();
        *a.counters_logged.lock().unwrap_or_else(|p| p.into_inner()) = key;
        a
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Counts one `NoModifierForUsage` refusal and prints the counters.
    pub fn count_no_modifier(&self) {
        self.refused_no_modifier.fetch_add(1, Ordering::Relaxed);
        self.log_counters_if_changed();
    }

    /// What decides a reprint: `modifier_invalid_linear` and `refused_no_modifier` by value, each tier counter by whether it is non-zero (so a tier's first buffer prints, its later ones do not).
    fn log_key(&self) -> LogKey {
        let c = self.backend.counters();
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        (
            load(&c.modifier_invalid_linear),
            load(&self.refused_no_modifier),
            load(&c.alloc_linear) != 0,
            load(&c.alloc_modifier) != 0,
        )
    }

    fn log_counters_if_changed(&self) {
        let key = self.log_key();
        let mut last = self
            .counters_logged
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if std::mem::replace(&mut *last, key) != key {
            // stderr is the allocator's log (service.rs `log`); a failed write loses one diagnostic line, not the buffer
            let _ = writeln!(std::io::stderr(), "{}", self.counters_line());
        }
    }

    /// `gralloc: counters <name>=<n> ...`: printed at start and whenever [`Self::log_key`] changes (the gate reads the last one).
    pub fn counters_line(&self) -> String {
        let c = self.backend.counters();
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        format!(
            "gralloc: counters modifier_invalid_linear={} alloc_linear={} alloc_modifier={} refused_no_modifier={}",
            load(&c.modifier_invalid_linear),
            load(&c.alloc_linear),
            load(&c.alloc_modifier),
            load(&self.refused_no_modifier),
        )
    }

    /// A zero-filled memfd page initialised with `name`. Written with `pwrite`, so this process never maps it.
    fn new_page(name: &str) -> Result<OwnedFd, AllocError> {
        let page = |why: &str, e: rustix::io::Errno| AllocError::Page(format!("{why}: {e}"));
        let fd =
            memfd_create("atl-gralloc-meta", MemfdFlags::CLOEXEC).map_err(|e| page("memfd", e))?;
        ftruncate(&fd, shared::PAGE_SIZE as u64).map_err(|e| page("size", e))?;
        let mut bytes = vec![0u8; shared::PAGE_SIZE];
        shared::init(&mut bytes, name).map_err(|e| AllocError::Page(e.to_string()))?;
        rustix::io::pwrite(&fd, &bytes, 0).map_err(|e| page("write", e))?;
        Ok(fd)
    }

    /// Allocates one buffer for `d`.
    pub fn allocate(&self, d: &Checked) -> Result<Allocation, AllocError> {
        let dmabuf = self.backend.allocate(d)?;
        self.log_counters_if_changed();
        {
            let mut seen = self
                .modifiers_seen
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !seen.contains(&dmabuf.modifier) {
                seen.push(dmabuf.modifier);
                // one line per distinct modifier: names which layout GBM picked, without a line per buffer
                let _ = writeln!(
                    std::io::stderr(),
                    "gralloc: modifier {:#x} first used ({}, usage {:#x}, set of {})",
                    dmabuf.modifier,
                    d.name,
                    d.usage,
                    d.modifiers.len()
                );
            }
        }
        let row = u64::from(d.width) * u64::from(d.format.bytes_per_pixel);
        if u64::from(dmabuf.pitch) < row || !dmabuf.pitch.is_multiple_of(d.format.bytes_per_pixel) {
            return Err(AllocError::Inconsistent("pitch"));
        }
        if dmabuf.size < u64::from(dmabuf.pitch) * u64::from(d.height) {
            return Err(AllocError::Inconsistent("size"));
        }
        let page = Self::new_page(&d.name)?;
        // pid in the high half, a per-process counter below: unique among live buffers of the system as long as pids of live allocators are (one allocator process per system)
        let id =
            (u64::from(self.pid) << 32) | u64::from(self.counter.fetch_add(1, Ordering::Relaxed));
        Ok(Allocation {
            dmabuf: dmabuf.fd,
            page,
            params: Params {
                width: d.width,
                height: d.height,
                pixel_format: d.format.pixel_format,
                fourcc: d.format.fourcc,
                pitch: dmabuf.pitch,
                size: dmabuf.size,
                usage: d.usage,
                modifier: dmabuf.modifier,
                id,
                device: self.backend.device(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use atl_gralloc::descriptor::Descriptor;
    use atl_gralloc::format::RGBA_8888;
    use atl_gralloc::handle;

    use super::*;
    use crate::backend::DRM_DIR;

    fn request() -> Checked {
        Descriptor {
            name: b"alloc-test\0".to_vec(),
            width: 64,
            height: 64,
            layer_count: 1,
            format: RGBA_8888,
            usage: 0x33,
            reserved_size: 0,
            option_names: vec![],
            modifiers: vec![atl_gralloc::format::DRM_FORMAT_MOD_LINEAR],
        }
        .check()
        .expect("valid")
    }

    #[test]
    fn page_holds_the_name() {
        let fd = Allocator::new_page("hello").expect("page");
        let mut buf = vec![0u8; shared::PAGE_SIZE];
        rustix::io::pread(&fd, &mut buf, 0).expect("read");
        assert_eq!(shared::validate(&buf), Ok(()));
        assert_eq!(shared::name(&buf), b"hello");
    }

    /// The round trip the importer does: ints from the allocation parse back, and ids differ per buffer. Needs a DRM node; without one the backend error must explain (see backend tests).
    #[test]
    fn allocation_round_trips_through_the_handle_ints() {
        let Ok(backend) = Backend::start(Path::new(DRM_DIR), None) else {
            return;
        };
        let a = Allocator::new(backend);
        let (x, y) = (
            a.allocate(&request()).expect("first"),
            a.allocate(&request()).expect("second"),
        );
        assert_ne!(x.params.id, y.params.id);
        assert_eq!(x.params.id >> 32, u64::from(std::process::id()));
        assert_eq!(
            handle::Params::from_handle(handle::NUM_FDS, &x.params.to_ints()),
            Ok(x.params)
        );
        assert_eq!(x.params.pixel_stride(), x.params.pitch / 4);
        // the handle names the node it came from (st_rdev, independent of the backend's own record)
        let rdev = std::os::unix::fs::MetadataExt::rdev(
            &std::fs::metadata(a.backend.node()).expect("node"),
        );
        assert_eq!(u64::from(x.params.device.0), rdev);
    }
}
