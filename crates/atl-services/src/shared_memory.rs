// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/core/jni/com_android_internal_os_ApplicationSharedMemory.cpp, frameworks/base/core/jni/android_app_PropertyInvalidatedCache.h, frameworks/base/core/java/com/android/internal/os/ApplicationSharedMemory.java
//! The ApplicationSharedMemory region system_server creates (`ApplicationSharedMemory.create()`: nativeCreate + nativeInit) and ActivityManagerService hands every app in bindApplication (read-only fd), which ActivityThread maps (`fromFileDescriptor`) before anything else when the aconfig flag
//! `com.android.internal.os.application_shared_memory_enabled` is on (it is in every release config of this tree). Its layout is C++ (`SharedMemory` in the JNI file) with no Java or Rust description, so it is restated here and checked against the file's `static_assert`s (tests):
//! ```text
//! 0      i64   latestNetworkTimeUnixEpochMillisAtZeroElapsedRealtimeMillis, INVALID_NETWORK_TIME (-1) until a time source reports
//! 8      SystemFeaturesCache: 512 x i32 features, then i64 featuresLength (0 = nothing cached yet)
//! 2064   SystemCacheNonce = CacheNonce<128, 8192>: NonceStore header (i32 kMaxNonce, i32 kMaxByte, i32 mNonceOffset, i32 mByteOffset, i32 mByteHash, i32 pad: 24 bytes),
//!        128 x i64 nonces, 8192 bytes of block; every offset in the header is from the header's own address, the structure being mapped at different addresses per process
//! ```
//! Nothing writes the features cache or the nonces yet: the region only has to be valid for apps to map and read (zero nonces = "unset" on the reader's side; zero features = no cached features, so `SystemFeaturesCache` answers nothing and callers query PackageManager).

use std::os::fd::OwnedFd;

use rustix::fs::{MemfdFlags, Mode, OFlags, ftruncate, memfd_create, open};
use rustix::io::pwrite;

/// `SharedMemory::INVALID_NETWORK_TIME`.
const INVALID_NETWORK_TIME: i64 = -1;
/// `SystemFeaturesCache::kMaxSystemFeatures`.
const MAX_SYSTEM_FEATURES: usize = 512;
/// `SystemCacheNonce`'s template parameters (android_app_PropertyInvalidatedCache.h, `system_nonce_config`).
const MAX_NONCE: usize = 128;
const MAX_BYTE: usize = 8192;
/// `sizeof(NonceStore)`: the 24 bytes its static_assert fixes.
const NONCE_STORE_HEADER: usize = 24;
/// `SystemFeaturesCache`: the features then the 8-byte length.
const FEATURES_CACHE: usize = MAX_SYSTEM_FEATURES * 4 + 8;
/// Offset of `systemFeaturesCache` (after the 8-byte network time) and of `systemPic` (after the cache).
const FEATURES_OFFSET: usize = 8;
const NONCE_OFFSET: usize = FEATURES_OFFSET + FEATURES_CACHE;
/// `sizeof(SharedMemory)`: network time + cache + `sizeof(SystemCacheNonce)` (header + nonces + block).
pub const SIZE: usize = NONCE_OFFSET + NONCE_STORE_HEADER + MAX_NONCE * 8 + MAX_BYTE;

/// The initialised region as `SharedMemory()` (nativeInit) leaves it.
fn initial() -> Vec<u8> {
    let mut b = vec![0u8; SIZE];
    b[..8].copy_from_slice(&INVALID_NETWORK_TIME.to_ne_bytes());
    // NonceStore(kMaxNonce, kMaxByte, nonce, block): the offsets are from the NonceStore, the arrays follow its header
    let header = [
        MAX_NONCE as i32,
        MAX_BYTE as i32,
        NONCE_STORE_HEADER as i32,
        (NONCE_STORE_HEADER + MAX_NONCE * 8) as i32,
        0, // mByteHash = UNSET
        0, // _pad
    ];
    for (i, v) in header.iter().enumerate() {
        let at = NONCE_OFFSET + i * 4;
        b[at..at + 4].copy_from_slice(&v.to_ne_bytes());
    }
    b
}

/// The region and the read-only descriptor apps get.
pub struct ApplicationSharedMemory {
    /// The creator's descriptor (read-write): kept so the region lives as long as the service, and for the writers to come.
    _rw: OwnedFd,
    /// A read-only open of the same region (`nativeDupAsReadOnly`: ashmem's PROT_READ): what is sent to apps, which can map it only with PROT_READ.
    pub read_only: OwnedFd,
}

impl ApplicationSharedMemory {
    /// A new initialised region (`ApplicationSharedMemory.create()`).
    pub fn create() -> rustix::io::Result<Self> {
        let rw = memfd_create("ApplicationSharedMemory", MemfdFlags::CLOEXEC)?;
        ftruncate(&rw, SIZE as u64)?;
        let data = initial();
        let mut done = 0;
        while done < data.len() {
            done += pwrite(&rw, &data[done..], done as u64)?;
        }
        // reopening through /proc gives a new open file description with its own access mode: read-only, and not upgradable
        let read_only = open(
            format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&rw)),
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(ApplicationSharedMemory { _rw: rw, read_only })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::io::pread;

    /// The sizes the C++ static_asserts fix: sizeof(SystemFeaturesCache) == 4 * 512 + 8, sizeof(NonceStore) == 24, sizeof(SystemCacheNonce) == 24 + 128 * 8 + 8192, sizeof(SharedMemory) == 8 + both,
    /// offsetof(systemFeaturesCache) == 8, offsetof(systemPic) == that + sizeof(SystemFeaturesCache). Summed by hand: 2056, 9240, 11304, 2064.
    #[test]
    fn layout() {
        assert_eq!(FEATURES_CACHE, 2056);
        assert_eq!(NONCE_OFFSET, 2064);
        assert_eq!(SIZE, 11304);
    }

    #[test]
    fn region() {
        let m = ApplicationSharedMemory::create().expect("memfd");
        let read = |at: usize, n: usize| {
            let mut b = vec![0u8; n];
            assert_eq!(pread(&m.read_only, &mut b, at as u64).expect("pread"), n);
            b
        };
        // network time unset, the cache empty, the NonceStore header as its constructor writes it (offsets 24 and 24 + 128 * 8)
        assert_eq!(read(0, 8), (-1i64).to_ne_bytes());
        assert_eq!(read(NONCE_OFFSET - 8, 8), [0u8; 8]);
        let header: Vec<i32> = read(NONCE_OFFSET, 24)
            .chunks(4)
            .map(|c| i32::from_ne_bytes(c.try_into().expect("4 bytes")))
            .collect();
        assert_eq!(header, [128, 8192, 24, 1048, 0, 0]);
        // the file ends where the structure does
        assert_eq!(
            rustix::fs::fstat(&m.read_only).expect("fstat").st_size,
            SIZE as i64
        );
        // not writable through the descriptor apps get
        assert!(pwrite(&m.read_only, &[1], 0).is_err());
    }
}
