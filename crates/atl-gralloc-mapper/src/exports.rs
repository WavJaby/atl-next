// SPDX-License-Identifier: MIT
//! The C ABI of `mapper.atl.so`: `AIMapper_loadIMapper` and the 15 entries of the `AIMapperV5` table it returns.
//! The entry shells are generated from IMapper.h (build.rs, atl-bridgegen `Table`): each has the header's exact signature, calls the function of the same name below with its arguments unchanged inside `catch_unwind` (a Rust panic must not cross into libui's C++; a panic answers `NO_RESOURCES`, `-NO_RESOURCES` for the getters), and `AIMapperV5::shells()` fills the table.
//! The functions below are the thin-shell pattern of AGENTS.md: each turns the caller's raw pointers into safe arguments at once (a handle's descriptors and ints, byte slices, an owned fence), calls [`Mapper`], and writes results back through the caller's out-pointers. Their signatures are checked by the compiler against the generated shells' calls.
//! Names are the header's field names, hence the lint allowance.
#![allow(non_snake_case)]

use std::ffi::{CStr, c_void};
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;

use atl_gralloc::handle;
use atl_gralloc::metadata::{self, STANDARD_METADATA_NAME};

use crate::generated::{
    AIMAPPER_ERROR_BAD_VALUE, AIMAPPER_ERROR_NONE, AIMAPPER_ERROR_UNSUPPORTED, AIMAPPER_VERSION_5,
    AIMapper, AIMapper_MetadataType, AIMapper_MetadataTypeDescription, AIMapperV5, ARect,
    native_handle,
};
use crate::mapper::{Error, Kernel, Mapper, Rect};

/// The process's mapper: one table of imported buffers (IMapper.h: "the imported handle must be considered valid everywhere in the process, including in another instance of the mapper").
fn mapper() -> &'static Mapper {
    static MAPPER: OnceLock<Mapper> = OnceLock::new();
    MAPPER.get_or_init(|| Mapper::new(Kernel::DMABUF))
}

fn status(r: Result<(), Error>) -> i32 {
    match r {
        Ok(()) => AIMAPPER_ERROR_NONE,
        Err(e) => e.code(),
    }
}

/// The most descriptors or ints read from a caller's handle before looking at them: ours has 2 and 15; a count beyond this is not ours, and no slice is built from it.
const HANDLE_READ_MAX: i32 = 64;

/// A handle's descriptors and ints, copied out.
struct RawHandle {
    fds: Vec<i32>,
    ints: Vec<i32>,
}

/// Reads `h` once.
///
/// # Safety
/// `h` is null or points at a `native_handle_t` whose `data` holds `numFds + numInts` ints (cutils/native_handle.h), as `importBuffer`'s contract guarantees for the handle it is given.
unsafe fn read_handle(h: *const native_handle) -> Option<RawHandle> {
    if h.is_null() {
        return None;
    }
    // SAFETY: non-null and valid per the caller's contract.
    let (nf, ni) = unsafe { ((*h).numFds, (*h).numInts) };
    if !(0..=HANDLE_READ_MAX).contains(&nf) || !(0..=HANDLE_READ_MAX).contains(&ni) {
        return None;
    }
    let (nf, ni) = (usize::try_from(nf).ok()?, usize::try_from(ni).ok()?);
    // SAFETY: `data` holds `numFds + numInts` ints (caller's contract), both counts bounded above.
    let data = unsafe { std::slice::from_raw_parts((*h).data.as_ptr(), nf + ni) };
    Some(RawHandle {
        fds: data[..nf].to_vec(),
        ints: data[nf..].to_vec(),
    })
}

/// A byte slice from `(ptr, len)`; null with length 0 is the empty slice.
///
/// # Safety
/// `ptr` is valid for reads of `len` bytes, or null with `len == 0`.
unsafe fn bytes<'a>(ptr: *const c_void, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: valid for `len` bytes per the caller.
    Some(unsafe { std::slice::from_raw_parts(ptr.cast(), len) })
}

/// Whether `t` names the standard metadata type list.
///
/// # Safety
/// `t.name` is null or a NUL-terminated string.
unsafe fn is_standard(t: &AIMapper_MetadataType) -> bool {
    if t.name.is_null() {
        return false;
    }
    // SAFETY: non-null, NUL-terminated per the caller's contract (IMapper.h: `name` is a C string).
    unsafe { CStr::from_ptr(t.name) }.to_bytes() == STANDARD_METADATA_NAME.as_bytes()
}

pub(crate) unsafe fn importBuffer(h: *const native_handle, out: *mut *const native_handle) -> i32 {
    if out.is_null() {
        return AIMAPPER_ERROR_BAD_VALUE;
    }
    // SAFETY: `h` is the raw handle `importBuffer` is called with (IMapper.h), valid for the call.
    let Some(raw) = (unsafe { read_handle(h) }) else {
        return Error::BadBuffer.code();
    };
    if raw.fds.iter().any(|&fd| fd < 0) {
        return Error::BadBuffer.code();
    }
    // SAFETY: the descriptors are open for the duration of the call (they belong to the caller's handle), and are only borrowed: `import` duplicates what it keeps.
    let fds: Vec<BorrowedFd<'_>> = raw
        .fds
        .iter()
        .map(|&fd| unsafe { BorrowedFd::borrow_raw(fd) })
        .collect();
    match mapper().import(&fds, &raw.ints) {
        Ok(key) => {
            // SAFETY: `out` is a valid out-pointer (non-null above).
            unsafe { *out = key as *const native_handle };
            AIMAPPER_ERROR_NONE
        }
        Err(e) => e.code(),
    }
}

pub(crate) unsafe fn freeBuffer(b: *const native_handle) -> i32 {
    status(mapper().free(b as usize))
}

pub(crate) unsafe fn getTransportSize(
    b: *const native_handle,
    fds: *mut u32,
    ints: *mut u32,
) -> i32 {
    if fds.is_null() || ints.is_null() {
        return AIMAPPER_ERROR_BAD_VALUE;
    }
    match mapper().transport_size(b as usize) {
        Ok((f, i)) => {
            // SAFETY: both out-pointers are valid (non-null above).
            unsafe {
                *fds = f;
                *ints = i;
            }
            AIMAPPER_ERROR_NONE
        }
        Err(e) => e.code(),
    }
}

pub(crate) unsafe fn lock(
    b: *const native_handle,
    cpu_usage: u64,
    region: ARect,
    acquire_fence: i32,
    out: *mut *mut c_void,
) -> i32 {
    // IMapper.h lock: ownership of the fence passes to the callee, which must close it on every path
    let fence = (acquire_fence >= 0).then(|| {
        // SAFETY: a non-negative `acquireFence` is an open descriptor whose ownership the caller hands over (IMapper.h).
        unsafe { OwnedFd::from_raw_fd(acquire_fence) }
    });
    if out.is_null() {
        return AIMAPPER_ERROR_BAD_VALUE;
    }
    let region = Rect {
        left: region.left,
        top: region.top,
        right: region.right,
        bottom: region.bottom,
    };
    match mapper().lock(b as usize, cpu_usage, region, fence) {
        Ok(p) => {
            // SAFETY: `out` is a valid out-pointer (non-null above).
            unsafe { *out = p.cast() };
            AIMAPPER_ERROR_NONE
        }
        Err(e) => e.code(),
    }
}

pub(crate) unsafe fn unlock(b: *const native_handle, release_fence: *mut i32) -> i32 {
    if release_fence.is_null() {
        return AIMAPPER_ERROR_BAD_VALUE;
    }
    let r = status(mapper().unlock(b as usize));
    // SAFETY: valid out-pointer (non-null above). No work is pending after the CPU access ends: the empty fence.
    unsafe { *release_fence = -1 };
    r
}

pub(crate) unsafe fn flushLockedBuffer(b: *const native_handle) -> i32 {
    status(mapper().resync(b as usize))
}

pub(crate) unsafe fn rereadLockedBuffer(b: *const native_handle) -> i32 {
    status(mapper().resync(b as usize))
}

/// `getMetadata` / `getStandardMetadata` common part.
///
/// # Safety
/// `dest` is null with `size == 0`, or valid for writes of `size` bytes.
unsafe fn get_standard(b: *const native_handle, ty: i64, dest: *mut c_void, size: usize) -> i32 {
    let dest = if dest.is_null() {
        None
    } else {
        // SAFETY: valid for `size` bytes per the caller.
        Some(unsafe { std::slice::from_raw_parts_mut(dest.cast::<u8>(), size) })
    };
    mapper().get_standard(b as usize, ty, dest)
}

pub(crate) unsafe fn getMetadata(
    b: *const native_handle,
    t: AIMapper_MetadataType,
    dest: *mut c_void,
    size: usize,
) -> i32 {
    // SAFETY: `t.name` is a C string (IMapper.h).
    if !unsafe { is_standard(&t) } {
        return -AIMAPPER_ERROR_UNSUPPORTED;
    }
    // SAFETY: `dest`/`size` are the caller's buffer (IMapper.h: null with size 0, or `size` bytes).
    unsafe { get_standard(b, t.value, dest, size) }
}

pub(crate) unsafe fn getStandardMetadata(
    b: *const native_handle,
    ty: i64,
    dest: *mut c_void,
    size: usize,
) -> i32 {
    // SAFETY: `dest`/`size` are the caller's buffer (IMapper.h).
    unsafe { get_standard(b, ty, dest, size) }
}

/// `setMetadata` / `setStandardMetadata` common part.
///
/// # Safety
/// `data` is valid for reads of `size` bytes, or null with `size == 0`.
unsafe fn set_standard(b: *const native_handle, ty: i64, data: *const c_void, size: usize) -> i32 {
    // SAFETY: per the caller.
    let Some(data) = (unsafe { bytes(data, size) }) else {
        return AIMAPPER_ERROR_BAD_VALUE;
    };
    status(mapper().set_standard(b as usize, ty, data))
}

pub(crate) unsafe fn setMetadata(
    b: *const native_handle,
    t: AIMapper_MetadataType,
    data: *const c_void,
    size: usize,
) -> i32 {
    // SAFETY: `t.name` is a C string (IMapper.h).
    if !unsafe { is_standard(&t) } {
        return AIMAPPER_ERROR_UNSUPPORTED;
    }
    // SAFETY: `data`/`size` are the caller's value bytes (IMapper.h).
    unsafe { set_standard(b, t.value, data, size) }
}

pub(crate) unsafe fn setStandardMetadata(
    b: *const native_handle,
    ty: i64,
    data: *const c_void,
    size: usize,
) -> i32 {
    // SAFETY: `data`/`size` are the caller's value bytes (IMapper.h).
    unsafe { set_standard(b, ty, data, size) }
}

/// The descriptions `listSupportedMetadataTypes` returns: one per standard type, alive for the process ("the returned array must be valid for the lifetime of the process").
struct Descriptions(Vec<AIMapper_MetadataTypeDescription>);

// SAFETY: the table is built once and never mutated; its raw pointers point at `'static` C string literals.
unsafe impl Send for Descriptions {}
// SAFETY: see above; shared reads only.
unsafe impl Sync for Descriptions {}

const STANDARD_NAME_C: &CStr = c"android.hardware.graphics.common.StandardMetadataType";

fn descriptions() -> &'static Descriptions {
    static D: OnceLock<Descriptions> = OnceLock::new();
    D.get_or_init(|| {
        Descriptions(
            metadata::SUPPORTED
                .iter()
                .map(|&(ty, settable)| AIMapper_MetadataTypeDescription {
                    metadataType: AIMapper_MetadataType {
                        name: STANDARD_NAME_C.as_ptr(),
                        value: ty,
                    },
                    // standard types may leave the description null (IMapper.h)
                    description: std::ptr::null(),
                    isGettable: true,
                    isSettable: settable,
                    reserved: [0; 32],
                })
                .collect(),
        )
    })
}

pub(crate) unsafe fn listSupportedMetadataTypes(
    list: *mut *const AIMapper_MetadataTypeDescription,
    count: *mut u64,
) -> i32 {
    if list.is_null() || count.is_null() {
        return AIMAPPER_ERROR_BAD_VALUE;
    }
    let d = descriptions();
    // SAFETY: both out-pointers are valid (non-null above).
    unsafe {
        *list = d.0.as_ptr();
        *count = d.0.len() as u64;
    }
    AIMAPPER_ERROR_NONE
}

type DumpCallback =
    Option<unsafe extern "C" fn(*mut c_void, AIMapper_MetadataType, *const c_void, usize)>;
type BeginCallback = Option<unsafe extern "C" fn(*mut c_void)>;

/// Calls the caller's callbacks for each dumped item.
fn dump(key: Option<usize>, begin: BeginCallback, cb: DumpCallback, ctx: *mut c_void) -> i32 {
    let Some(cb) = cb else {
        return AIMAPPER_ERROR_BAD_VALUE;
    };
    status(mapper().dump(key, |item| match item {
        None => {
            if let Some(begin) = begin {
                // SAFETY: `begin` and `ctx` are the caller's (IMapper.h `dumpAllBuffers`); called synchronously.
                unsafe { begin(ctx) };
            }
        }
        Some((ty, value)) => {
            let t = AIMapper_MetadataType {
                name: STANDARD_NAME_C.as_ptr(),
                value: ty,
            };
            // SAFETY: `cb` and `ctx` are the caller's; `value` lives for the call ("The lifetime of this pointer is only valid for the duration of the call").
            unsafe { cb(ctx, t, value.as_ptr().cast(), value.len()) };
        }
    }))
}

pub(crate) unsafe fn dumpBuffer(
    b: *const native_handle,
    cb: DumpCallback,
    ctx: *mut c_void,
) -> i32 {
    dump(Some(b as usize), None, cb, ctx)
}

pub(crate) unsafe fn dumpAllBuffers(
    begin: BeginCallback,
    cb: DumpCallback,
    ctx: *mut c_void,
) -> i32 {
    dump(None, begin, cb, ctx)
}

pub(crate) unsafe fn getReservedRegion(
    b: *const native_handle,
    region: *mut *mut c_void,
    size: *mut u64,
) -> i32 {
    if region.is_null() || size.is_null() {
        return AIMAPPER_ERROR_BAD_VALUE;
    }
    if mapper().transport_size(b as usize).is_err() {
        return Error::BadBuffer.code();
    }
    // allocation refuses a reserved size (atl_gralloc::descriptor::Refusal::ReservedRegion), so every buffer's region is empty
    // SAFETY: both out-pointers are valid (non-null above).
    unsafe {
        *region = std::ptr::null_mut();
        *size = 0;
    }
    AIMAPPER_ERROR_NONE
}

/// The `AIMapper` the library hands out, built once and kept for the process.
struct Table(*mut AIMapper);

// SAFETY: the pointer is to a leaked, never-mutated allocation.
unsafe impl Send for Table {}
// SAFETY: see above.
unsafe impl Sync for Table {}

/// `AIMapper_loadIMapper`: libui's `dlsym` target. Returns the same table on every call.
///
/// # Safety
/// `out` is null or valid for a pointer write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn AIMapper_loadIMapper(out: *mut *mut AIMapper) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() {
            return AIMAPPER_ERROR_BAD_VALUE;
        }
        static TABLE: OnceLock<Table> = OnceLock::new();
        let t = TABLE.get_or_init(|| {
            Table(Box::leak(Box::new(AIMapper {
                version: AIMAPPER_VERSION_5,
                v5: AIMapperV5::shells(),
            })))
        });
        // SAFETY: `out` is valid for a pointer write (non-null above).
        unsafe { *out = t.0 };
        AIMAPPER_ERROR_NONE
    }))
    .unwrap_or(AIMAPPER_ERROR_UNSUPPORTED)
}

// the handle ints this library parses are atl-gralloc's; keep the dependency on its layout explicit
const _: () = assert!(handle::NUM_FDS == 2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_name_matches_the_core_constant() {
        assert_eq!(STANDARD_NAME_C.to_str(), Ok(STANDARD_METADATA_NAME));
    }

    /// The loader hands out the same version-5 table twice, with all 15 entries set.
    #[test]
    fn loader_returns_one_complete_table() {
        let mut a: *mut AIMapper = std::ptr::null_mut();
        let mut b: *mut AIMapper = std::ptr::null_mut();
        // SAFETY: valid out-pointers to locals.
        unsafe {
            assert_eq!(AIMapper_loadIMapper(&mut a), AIMAPPER_ERROR_NONE);
            assert_eq!(AIMapper_loadIMapper(&mut b), AIMAPPER_ERROR_NONE);
            assert_eq!(
                AIMapper_loadIMapper(std::ptr::null_mut()),
                AIMAPPER_ERROR_BAD_VALUE
            );
        }
        assert_eq!(a, b);
        // SAFETY: `a` is the leaked table.
        let t = unsafe { &*a };
        assert_eq!(t.version, 5);
        let v = &t.v5;
        let all = [
            v.importBuffer.is_some(),
            v.freeBuffer.is_some(),
            v.getTransportSize.is_some(),
            v.lock.is_some(),
            v.unlock.is_some(),
            v.flushLockedBuffer.is_some(),
            v.rereadLockedBuffer.is_some(),
            v.getMetadata.is_some(),
            v.getStandardMetadata.is_some(),
            v.setMetadata.is_some(),
            v.setStandardMetadata.is_some(),
            v.listSupportedMetadataTypes.is_some(),
            v.dumpBuffer.is_some(),
            v.dumpAllBuffers.is_some(),
            v.getReservedRegion.is_some(),
        ];
        assert!(all.iter().all(|&s| s));
    }

    /// Entries are called through the table, as libui does: a handle the table does not know answers BAD_BUFFER (2) from `freeBuffer`, and the getter's failure is the negative error.
    #[test]
    fn entries_work_through_the_table() {
        let t = AIMapperV5::shells();
        let free = t.freeBuffer.expect("entry");
        let get = t.getStandardMetadata.expect("entry");
        // SAFETY: a handle value the table has never seen is only looked up, never dereferenced.
        let (f, g) = unsafe {
            (
                free(usize::MAX as *const native_handle),
                get(
                    usize::MAX as *const native_handle,
                    1,
                    std::ptr::null_mut(),
                    0,
                ),
            )
        };
        assert_eq!((f, g), (2, -2));
    }

    #[test]
    fn handle_reads_are_bounded() {
        // SAFETY: null is allowed.
        assert!(unsafe { read_handle(std::ptr::null()) }.is_none());
        // a header claiming more descriptors than the bound is rejected before any slice is built
        let h = native_handle {
            version: 12,
            numFds: HANDLE_READ_MAX + 1,
            numInts: 0,
            data: [],
        };
        // SAFETY: the counts are rejected before `data` is read.
        assert!(unsafe { read_handle(&h) }.is_none());
        let h = native_handle {
            version: 12,
            numFds: -1,
            numInts: 0,
            data: [],
        };
        // SAFETY: as above.
        assert!(unsafe { read_handle(&h) }.is_none());
    }

    #[test]
    fn listed_types_are_the_core_list() {
        let mut list: *const AIMapper_MetadataTypeDescription = std::ptr::null();
        let mut n = 0u64;
        // SAFETY: valid out-pointers to locals.
        let status = unsafe { listSupportedMetadataTypes(&mut list, &mut n) };
        assert_eq!(status, AIMAPPER_ERROR_NONE);
        assert_eq!(n as usize, metadata::SUPPORTED.len());
        // SAFETY: `list` points at `n` descriptions that live for the process.
        let d = unsafe { std::slice::from_raw_parts(list, n as usize) };
        let settable: Vec<i64> = d
            .iter()
            .filter(|d| d.isSettable)
            .map(|d| d.metadataType.value)
            .collect();
        assert_eq!(settable, [17, 18, 19, 20]);
        assert!(d.iter().all(|d| d.isGettable));
    }
}
