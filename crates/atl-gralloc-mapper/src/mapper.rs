// SPDX-License-Identifier: MIT
//! The mapper's logic, in safe Rust: the table of imported buffers and what IMapper v5 does with them (import, free, lock / unlock around the dma-buf mapping, metadata).
//! The C ABI shells (`exports`) turn raw pointers into the arguments here and the results back; this module never sees a raw pointer from the caller.
//! An imported buffer is identified by the address of its handle (`buffer_handle_t`): a number looked up in the table, never dereferenced, so a stale or foreign handle is `BAD_BUFFER`, not undefined behaviour.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use atl_gralloc::handle::{self, Params};
use atl_gralloc::metadata::{self, SetError, View};
use atl_gralloc::{shared, usage};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{SeekFrom, fstat, seek};
use rustix::io::fcntl_dupfd_cloexec;
use rustix::time::Timespec;

use atl_gralloc_dmabuf::{self as dmabuf, Mapping, SYNC_READ, SYNC_WRITE};

/// `AIMapper_Error` values (IMapper.h): the ones the interface lets each function return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadBuffer,
    BadValue,
    NoResources,
    Unsupported,
}

impl Error {
    /// The `AIMapper_Error` number.
    pub fn code(self) -> i32 {
        match self {
            Self::BadBuffer => 2,
            Self::BadValue => 3,
            Self::NoResources => 5,
            Self::Unsupported => 7,
        }
    }
}

/// `ARect` of a lock's access region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The kernel operations a lock needs, so the table logic is tested without a dma-buf (a memfd is not one). The product uses [`Kernel::DMABUF`].
#[derive(Clone, Copy)]
pub struct Kernel {
    pub begin: fn(BorrowedFd<'_>, u64) -> io::Result<()>,
    pub end: fn(BorrowedFd<'_>, u64) -> io::Result<()>,
}

impl Kernel {
    pub const DMABUF: Kernel = Kernel {
        begin: dmabuf::begin_cpu_access,
        end: dmabuf::end_cpu_access,
    };
}

/// How long `lock` waits for an acquire fence: a fence is an fd that turns readable when its work is done. We hand out none, so a fence arrives only from a producer in this system; one that has not signalled after 3 s is stuck, and `NO_RESOURCES` ("locking may succeed at a later time", IMapper.h) lets the caller retry rather than block a UI thread forever.
const FENCE_WAIT: Duration = Duration::from_secs(3);

/// What one imported buffer is.
struct Buffer {
    params: Params,
    dmabuf: OwnedFd,
    /// Held to keep the descriptor in `handle` open; the page is read through `page`.
    _page_fd: OwnedFd,
    page: Mapping,
    /// The imported `native_handle_t`: version, numFds, numInts, then the two fds and the ints. Its address is the `buffer_handle_t`; the boxed slice does not move.
    handle: Box<[i32]>,
    state: Mutex<LockState>,
}

#[derive(Default)]
struct LockState {
    /// The CPU mapping of the dma-buf, made by the first lock and kept until the buffer is freed.
    data: Option<Mapping>,
    /// The direction bits of each outstanding lock, innermost last, so unlock ends the access it began.
    open: Vec<u64>,
}

/// `sizeof(native_handle_t)`, the `version` of every handle (cutils/native_handle.h `native_handle_create`); the generated layout assert pins it to the header.
const HANDLE_HEADER_INTS: usize = 3;

/// The table of imported buffers of this process.
pub struct Mapper {
    kernel: Kernel,
    buffers: Mutex<HashMap<usize, Arc<Buffer>>>,
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Mapper {
    pub fn new(kernel: Kernel) -> Self {
        Self {
            kernel,
            buffers: Mutex::new(HashMap::new()),
        }
    }

    fn find(&self, key: usize) -> Result<Arc<Buffer>, Error> {
        guard(&self.buffers)
            .get(&key)
            .cloned()
            .ok_or(Error::BadBuffer)
    }

    /// `importBuffer`: validates the raw handle (our layout, the dma-buf and the page big enough for what the ints claim -- a handle crosses processes, and a short object would fault the first access), duplicates its descriptors and returns the new handle's address.
    pub fn import(&self, fds: &[BorrowedFd<'_>], ints: &[i32]) -> Result<usize, Error> {
        let params = Params::from_handle(fds.len(), ints).map_err(|_| Error::BadBuffer)?;
        let dup = |fd: BorrowedFd<'_>| fcntl_dupfd_cloexec(fd, 0).map_err(|_| Error::NoResources);
        let dmabuf = dup(fds[handle::FD_DMABUF])?;
        let page_fd = dup(fds[handle::FD_PAGE])?;
        // a dma-buf reports its size by seeking to its end; short means the ints lie
        let size = seek(&dmabuf, SeekFrom::End(0)).map_err(|_| Error::BadBuffer)?;
        if size < params.size {
            return Err(Error::BadBuffer);
        }
        let page_len = fstat(&page_fd).map_err(|_| Error::BadBuffer)?.st_size;
        if u64::try_from(page_len) != Ok(shared::PAGE_SIZE as u64) {
            return Err(Error::BadBuffer);
        }
        let page =
            Mapping::new(page_fd.as_fd(), shared::PAGE_SIZE).map_err(|_| Error::NoResources)?;
        shared::validate(&page.read()).map_err(|_| Error::BadBuffer)?;
        let mut h = vec![0i32; HANDLE_HEADER_INTS + handle::NUM_FDS + handle::NUM_INTS];
        h[0] = i32::try_from(HANDLE_HEADER_INTS * 4).expect("12");
        h[1] = i32::try_from(handle::NUM_FDS).expect("2");
        h[2] = i32::try_from(handle::NUM_INTS).expect("15");
        h[HANDLE_HEADER_INTS + handle::FD_DMABUF] = dmabuf.as_raw_fd();
        h[HANDLE_HEADER_INTS + handle::FD_PAGE] = page_fd.as_raw_fd();
        h[HANDLE_HEADER_INTS + handle::NUM_FDS..].copy_from_slice(ints);
        let buffer = Arc::new(Buffer {
            params,
            dmabuf,
            _page_fd: page_fd,
            page,
            handle: h.into_boxed_slice(),
            state: Mutex::new(LockState::default()),
        });
        let key = buffer.handle.as_ptr() as usize;
        guard(&self.buffers).insert(key, buffer);
        Ok(key)
    }

    /// `freeBuffer`: forgets the buffer; its descriptors close and its mappings go with it.
    pub fn free(&self, key: usize) -> Result<(), Error> {
        guard(&self.buffers)
            .remove(&key)
            .map(|_| ())
            .ok_or(Error::BadBuffer)
    }

    /// `getTransportSize`: fds and ints of the raw handle (the imported one adds none of its own).
    pub fn transport_size(&self, key: usize) -> Result<(u32, u32), Error> {
        self.find(key)?;
        Ok((handle::NUM_FDS as u32, handle::NUM_INTS as u32))
    }

    /// `lock`: waits for the acquire fence, maps the dma-buf (first lock) and brackets the CPU access. Returns the address of the top-left of the whole buffer.
    pub fn lock(
        &self,
        key: usize,
        cpu_usage: u64,
        region: Rect,
        acquire_fence: Option<OwnedFd>,
    ) -> Result<*mut u8, Error> {
        let b = self.find(key)?;
        // IMapper.h lock: BAD_VALUE if cpuUsage is 0, has non-CPU bits, is incompatible with the buffer, or the region leaves the buffer
        if cpu_usage == 0 || cpu_usage & !usage::CPU_MASK != 0 {
            return Err(Error::BadValue);
        }
        if (usage::cpu_reads(cpu_usage) && !usage::cpu_reads(b.params.usage))
            || (usage::cpu_writes(cpu_usage) && !usage::cpu_writes(b.params.usage))
        {
            return Err(Error::BadValue);
        }
        let (w, h) = (b.params.width as i32, b.params.height as i32);
        let whole = region
            == Rect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
        if !whole
            && !(0 <= region.left
                && region.left < region.right
                && region.right <= w
                && 0 <= region.top
                && region.top < region.bottom
                && region.bottom <= h)
        {
            return Err(Error::BadValue);
        }
        if let Some(fence) = acquire_fence {
            let mut fds = [PollFd::new(&fence, PollFlags::IN)];
            let timeout = Timespec {
                tv_sec: i64::try_from(FENCE_WAIT.as_secs()).expect("3"),
                tv_nsec: 0,
            };
            match poll(&mut fds, Some(&timeout)) {
                Ok(n) if n > 0 => {}
                _ => return Err(Error::NoResources),
            }
        }
        let direction = (if usage::cpu_reads(cpu_usage) {
            SYNC_READ
        } else {
            0
        }) | (if usage::cpu_writes(cpu_usage) {
            SYNC_WRITE
        } else {
            0
        });
        let mut st = guard(&b.state);
        if st.data.is_none() {
            let len = usize::try_from(b.params.size).map_err(|_| Error::NoResources)?;
            st.data = Some(Mapping::new(b.dmabuf.as_fd(), len).map_err(|_| Error::NoResources)?);
        }
        (self.kernel.begin)(b.dmabuf.as_fd(), direction).map_err(|_| Error::NoResources)?;
        st.open.push(direction);
        Ok(st.data.as_ref().expect("mapped above").as_ptr())
    }

    /// `unlock`: ends the innermost lock's CPU access. We produce no fence.
    pub fn unlock(&self, key: usize) -> Result<(), Error> {
        let b = self.find(key)?;
        let mut st = guard(&b.state);
        let direction = st.open.pop().ok_or(Error::BadBuffer)?;
        (self.kernel.end)(b.dmabuf.as_fd(), direction).map_err(|_| Error::NoResources)
    }

    /// `flushLockedBuffer` / `rereadLockedBuffer`: end and begin the innermost lock's access again, which publishes the CPU's writes and refetches the device's.
    pub fn resync(&self, key: usize) -> Result<(), Error> {
        let b = self.find(key)?;
        let st = guard(&b.state);
        let direction = *st.open.last().ok_or(Error::BadBuffer)?;
        (self.kernel.end)(b.dmabuf.as_fd(), direction).map_err(|_| Error::NoResources)?;
        (self.kernel.begin)(b.dmabuf.as_fd(), direction).map_err(|_| Error::NoResources)
    }

    /// `getStandardMetadata`: the encoded value, copied into `dest` when it fits; returns its size (so a too-small or absent `dest` still learns it), or the error as a negative `AIMapper_Error`.
    pub fn get_standard(&self, key: usize, ty: i64, dest: Option<&mut [u8]>) -> i32 {
        let Ok(b) = self.find(key) else {
            return -Error::BadBuffer.code();
        };
        let page = b.page.read();
        let view = View {
            params: &b.params,
            page: &page,
        };
        match metadata::get_standard(&view, ty) {
            Err(_) => -Error::Unsupported.code(),
            Ok(bytes) => {
                if let Some(dest) = dest
                    && dest.len() >= bytes.len()
                {
                    dest[..bytes.len()].copy_from_slice(&bytes);
                }
                i32::try_from(bytes.len()).unwrap_or(-Error::NoResources.code())
            }
        }
    }

    /// `setStandardMetadata`: into the shared page, so every process holding the buffer sees it.
    pub fn set_standard(&self, key: usize, ty: i64, data: &[u8]) -> Result<(), Error> {
        let b = self.find(key)?;
        // validate and encode on a copy, then store only the changed field: a concurrent set of another field in another process is not undone
        let mut copy = b.page.read();
        metadata::set_standard(&mut copy, ty, data).map_err(|e| match e {
            SetError::BadValue => Error::BadValue,
            SetError::Unsupported => Error::Unsupported,
        })?;
        for &(offset, len) in metadata::page_spans(ty) {
            b.page.write(offset, &copy[offset..offset + len]);
        }
        Ok(())
    }

    /// Calls `f` with the key of every imported buffer and its encoded standard metadata, buffer by buffer (`dumpBuffer` / `dumpAllBuffers`).
    pub fn dump(
        &self,
        key: Option<usize>,
        mut f: impl FnMut(Option<(i64, &[u8])>),
    ) -> Result<(), Error> {
        let all: Vec<Arc<Buffer>> = match key {
            Some(k) => vec![self.find(k)?],
            None => guard(&self.buffers).values().cloned().collect(),
        };
        for b in all {
            let page = b.page.read();
            let view = View {
                params: &b.params,
                page: &page,
            };
            f(None);
            for &(ty, _) in metadata::SUPPORTED {
                if let Ok(bytes) = metadata::get_standard(&view, ty) {
                    f(Some((ty, &bytes)));
                }
            }
        }
        Ok(())
    }

    /// How many buffers are imported (tests).
    pub fn len(&self) -> usize {
        guard(&self.buffers).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
#[allow(unsafe_code, reason = "one write through the pointer lock returns")]
mod tests {
    use atl_gralloc::format::{DRM_FORMAT_ABGR8888, RGBA_8888};
    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};

    use super::*;

    fn nop(_: BorrowedFd<'_>, _: u64) -> io::Result<()> {
        Ok(())
    }

    fn mapper() -> Mapper {
        Mapper::new(Kernel {
            begin: nop,
            end: nop,
        })
    }

    fn params() -> Params {
        Params {
            width: 16,
            height: 8,
            pixel_format: RGBA_8888,
            fourcc: DRM_FORMAT_ABGR8888,
            pitch: 64,
            size: 64 * 8,
            usage: 0x33, // CPU_READ_OFTEN | CPU_WRITE_OFTEN as the nibbles: read 3, write 3 << 4
            modifier: 0,
            id: 42,
        }
    }

    /// A raw handle's two fds: a memfd standing in for the dma-buf (sized as the params claim), and an initialised page.
    fn raw(p: &Params) -> (OwnedFd, OwnedFd) {
        let buf = memfd_create("b", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&buf, p.size).expect("size");
        let page = memfd_create("p", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&page, shared::PAGE_SIZE as u64).expect("size");
        let m = Mapping::new(page.as_fd(), shared::PAGE_SIZE).expect("map");
        let mut init = vec![0u8; shared::PAGE_SIZE];
        shared::init(&mut init, "unit").expect("init");
        m.write(0, &init);
        (buf, page)
    }

    fn import(m: &Mapper, p: &Params) -> usize {
        let (b, pg) = raw(p);
        m.import(&[b.as_fd(), pg.as_fd()], &p.to_ints())
            .expect("import")
    }

    /// A fence stand-in: the read end turns readable once the other end writes.
    fn signal_pair() -> (OwnedFd, OwnedFd) {
        use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
        socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair")
    }

    const WHOLE: Rect = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };

    #[test]
    fn import_validates_and_free_forgets() {
        let m = mapper();
        let p = params();
        let (b, pg) = raw(&p);
        let ints = p.to_ints();
        // wrong fd count, wrong int count
        assert_eq!(m.import(&[b.as_fd()], &ints), Err(Error::BadBuffer));
        assert_eq!(
            m.import(&[b.as_fd(), pg.as_fd()], &ints[..14]),
            Err(Error::BadBuffer)
        );
        // a dma-buf shorter than the ints claim (would fault on first access)
        let short = memfd_create("s", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&short, p.size - 1).expect("size");
        assert_eq!(
            m.import(&[short.as_fd(), pg.as_fd()], &ints),
            Err(Error::BadBuffer)
        );
        // a page of the wrong size, and one that is not ours
        let small = memfd_create("s", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&small, 100).expect("size");
        assert_eq!(
            m.import(&[b.as_fd(), small.as_fd()], &ints),
            Err(Error::BadBuffer)
        );
        let blank = memfd_create("s", MemfdFlags::CLOEXEC).expect("memfd");
        ftruncate(&blank, shared::PAGE_SIZE as u64).expect("size");
        assert_eq!(
            m.import(&[b.as_fd(), blank.as_fd()], &ints),
            Err(Error::BadBuffer)
        );
        assert!(m.is_empty());

        let key = m.import(&[b.as_fd(), pg.as_fd()], &ints).expect("import");
        assert_eq!(m.transport_size(key), Ok((2, 15)));
        assert_eq!(m.free(key), Ok(()));
        assert_eq!(m.free(key), Err(Error::BadBuffer));
        assert_eq!(m.transport_size(key), Err(Error::BadBuffer));
    }

    /// The imported handle is a `native_handle_t` whose fds are the duplicates and whose ints are the raw handle's.
    #[test]
    fn imported_handle_layout() {
        let m = mapper();
        let p = params();
        let key = import(&m, &p);
        let b = m.find(key).expect("find");
        assert_eq!(&b.handle[..3], &[12, 2, 15]);
        assert_eq!(b.handle[3], b.dmabuf.as_raw_fd());
        assert_eq!(&b.handle[5..], &p.to_ints());
        assert_ne!(b.handle[3], b.handle[4]);
    }

    #[test]
    fn lock_unlock_cycle_and_pointer_is_stable() {
        let m = mapper();
        let key = import(&m, &params());
        let p1 = m.lock(key, 0x30, WHOLE, None).expect("write lock");
        let p2 = m.lock(key, 0x3, WHOLE, None).expect("nested read lock");
        assert_eq!(p1, p2, "one mapping per buffer");
        // writes through the pointer land in the shared object: a second import of the same fd sees them
        // SAFETY: p1 points at 512 mapped bytes (16 x 8 px x 4); the test is the only user.
        unsafe { p1.write(0xAB) };
        assert_eq!(m.unlock(key), Ok(()));
        assert_eq!(m.unlock(key), Ok(()));
        assert_eq!(m.unlock(key), Err(Error::BadBuffer), "unbalanced unlock");
        assert_eq!(m.resync(key), Err(Error::BadBuffer), "flush needs a lock");
    }

    #[test]
    fn lock_refusals() {
        let m = mapper();
        let key = import(&m, &params());
        for (usage, region, why) in [
            (0u64, WHOLE, "no usage"),
            (1 << 8, WHOLE, "non-CPU bit"),
            (
                0x30,
                Rect {
                    left: 0,
                    top: 0,
                    right: 17,
                    bottom: 8,
                },
                "past the width",
            ),
            (
                0x30,
                Rect {
                    left: 4,
                    top: 0,
                    right: 4,
                    bottom: 8,
                },
                "empty",
            ),
            (
                0x30,
                Rect {
                    left: -1,
                    top: 0,
                    right: 4,
                    bottom: 8,
                },
                "negative",
            ),
        ] {
            assert_eq!(
                m.lock(key, usage, region, None),
                Err(Error::BadValue),
                "{why}"
            );
        }
        // a read-only buffer cannot be write-locked
        let mut ro = params();
        ro.usage = 0x3;
        let key = import(&m, &ro);
        assert_eq!(m.lock(key, 0x30, WHOLE, None), Err(Error::BadValue));
        assert!(
            m.lock(
                key,
                0x3,
                Rect {
                    left: 1,
                    top: 1,
                    right: 16,
                    bottom: 8
                },
                None
            )
            .is_ok()
        );
        assert_eq!(m.lock(usize::MAX, 0x3, WHOLE, None), Err(Error::BadBuffer));
    }

    /// A fence that is signalled (readable) lets the lock through; one that never signals times out as NO_RESOURCES. (The timeout test waits FENCE_WAIT.)
    #[test]
    fn acquire_fence_signalled_passes() {
        let m = mapper();
        let key = import(&m, &params());
        let (r, w) = signal_pair();
        rustix::io::write(&w, b"x").expect("signal");
        assert!(m.lock(key, 0x3, WHOLE, Some(r)).is_ok());
    }

    #[test]
    fn acquire_fence_never_signalled_times_out() {
        let m = mapper();
        let key = import(&m, &params());
        let (r, _w) = signal_pair();
        let t = std::time::Instant::now();
        assert_eq!(m.lock(key, 0x3, WHOLE, Some(r)), Err(Error::NoResources));
        assert!(t.elapsed() >= FENCE_WAIT);
    }

    #[test]
    fn metadata_get_answers_and_set_is_global() {
        let m = mapper();
        let p = params();
        let (b, pg) = raw(&p);
        let ints = p.to_ints();
        // two imports of the same raw handle = two processes holding the buffer
        let a = m.import(&[b.as_fd(), pg.as_fd()], &ints).expect("a");
        let c = m.import(&[b.as_fd(), pg.as_fd()], &ints).expect("c");
        assert_ne!(a, c);
        let mut buf = [0u8; 256];
        let n = m.get_standard(a, metadata::ty::WIDTH, Some(&mut buf));
        assert_eq!(n, 53 + 8 + 8 + 8);
        assert_eq!(&buf[n as usize - 8..n as usize], &16u64.to_ne_bytes());
        // too small a destination still reports the size; absent too
        assert_eq!(
            m.get_standard(a, metadata::ty::WIDTH, Some(&mut buf[..4])),
            n
        );
        assert_eq!(m.get_standard(a, metadata::ty::WIDTH, None), n);
        assert_eq!(m.get_standard(a, 99, None), -Error::Unsupported.code());
        assert_eq!(
            m.get_standard(usize::MAX, metadata::ty::WIDTH, None),
            -Error::BadBuffer.code()
        );

        // set dataspace through one import, read it through the other
        let mut v = 53i64.to_ne_bytes().to_vec();
        v.extend_from_slice(metadata::STANDARD_METADATA_NAME.as_bytes());
        v.extend_from_slice(&metadata::ty::DATASPACE.to_ne_bytes());
        v.extend_from_slice(&0x8_0000i32.to_ne_bytes());
        assert_eq!(m.set_standard(a, metadata::ty::DATASPACE, &v), Ok(()));
        let n = m.get_standard(c, metadata::ty::DATASPACE, Some(&mut buf)) as usize;
        assert_eq!(&buf[n - 4..n], &0x8_0000i32.to_ne_bytes());
        assert_eq!(
            m.set_standard(a, metadata::ty::WIDTH, &v),
            Err(Error::BadValue)
        );
        assert_eq!(
            m.set_standard(a, metadata::ty::STRIDE, &v),
            Err(Error::Unsupported)
        );
    }

    #[test]
    fn dump_visits_every_type_per_buffer() {
        let m = mapper();
        let key = import(&m, &params());
        let _second = import(&m, &params());
        let (mut begins, mut types) = (0, 0);
        m.dump(Some(key), |e| match e {
            None => begins += 1,
            Some(_) => types += 1,
        })
        .expect("dump");
        assert_eq!((begins, types), (1, metadata::SUPPORTED.len()));
        let mut begins = 0;
        m.dump(None, |e| begins += usize::from(e.is_none()))
            .expect("dump all");
        assert_eq!(begins, 2);
        assert_eq!(m.dump(Some(usize::MAX), |_| {}), Err(Error::BadBuffer));
    }
}
