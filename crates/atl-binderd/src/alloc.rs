// SPDX-License-Identifier: MIT
//! Per-process buffer region allocator: binderd hands out offsets, the process holds the bytes. Rules
//! from the kernel's `binder_alloc.c` (v6.12): sizes padded by `sanitized_size` (data and offsets each
//! aligned to the pointer size, empty buffers padded to one pointer, `:532-551`), best fit over free
//! space (`binder_alloc_new_buf_locked`, `:436-520`), and one-way buffers limited to half the region
//! (`free_async_space = buffer_size / 2`, `:864`; checked `:449`, returned on free `:681`).

use std::collections::BTreeMap;

/// Pointer size of every host arch (64-bit hosts only, arches.toml): the kernel's alignment unit.
const PTR: u64 = 8;

/// The kernel caps a process's binder mapping at 4 MiB (`binder_alloc_mmap_handler`, `SZ_4M`,
/// binder_alloc.c:834).
pub const MAX_REGION_SIZE: u64 = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// Size computation overflowed (`-EINVAL` in the kernel).
    Invalid,
    /// No free range or no async space left (`-ENOSPC`).
    NoSpace,
}

pub fn align(n: u64) -> Option<u64> {
    n.checked_add(PTR - 1).map(|v| v & !(PTR - 1))
}

/// Region bytes a buffer of `data_size` + `offsets_size` occupies.
pub fn sanitized_size(data_size: u64, offsets_size: u64) -> Option<u64> {
    let total = align(data_size)?.checked_add(align(offsets_size)?)?;
    Some(total.max(PTR))
}

#[derive(Debug)]
pub struct Alloc {
    /// Free ranges: offset -> length; adjacent ranges are always merged.
    free: BTreeMap<u64, u64>,
    /// Allocated ranges: offset -> (length, async).
    used: BTreeMap<u64, (u64, bool)>,
    free_async: u64,
}

impl Alloc {
    pub fn new(region_size: u64) -> Self {
        let size = region_size.min(MAX_REGION_SIZE);
        let mut free = BTreeMap::new();
        if size > 0 {
            free.insert(0, size);
        }
        Self {
            free,
            used: BTreeMap::new(),
            free_async: size / 2,
        }
    }

    /// Offset of a new buffer for `data_size` + `offsets_size` bytes.
    pub fn alloc(
        &mut self,
        data_size: u64,
        offsets_size: u64,
        is_async: bool,
    ) -> Result<u64, AllocError> {
        let size = sanitized_size(data_size, offsets_size).ok_or(AllocError::Invalid)?;
        if is_async && self.free_async < size {
            return Err(AllocError::NoSpace);
        }
        // best fit: the smallest free range that holds it, lowest offset on a tie
        let (&off, &len) = self
            .free
            .iter()
            .filter(|(_, len)| **len >= size)
            .min_by_key(|(off, len)| (**len, **off))
            .ok_or(AllocError::NoSpace)?;
        self.free.remove(&off);
        if len > size {
            self.free.insert(off + size, len - size);
        }
        self.used.insert(off, (size, is_async));
        if is_async {
            self.free_async -= size;
        }
        Ok(off)
    }

    /// Releases the buffer at `off`; false when nothing is allocated there.
    pub fn free(&mut self, off: u64) -> bool {
        let Some((len, is_async)) = self.used.remove(&off) else {
            return false;
        };
        if is_async {
            self.free_async += len;
        }
        let mut start = off;
        let mut end = off + len;
        if let Some((&prev, &plen)) = self.free.range(..off).next_back()
            && prev + plen == off
        {
            self.free.remove(&prev);
            start = prev;
        }
        if let Some(next) = self.free.remove(&end) {
            end += next;
        }
        self.free.insert(start, end - start);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_follow_sanitized_size() {
        // ALIGN(5, 8) + ALIGN(8, 8) = 16; an empty buffer still takes one pointer
        assert_eq!(sanitized_size(5, 8), Some(16));
        assert_eq!(sanitized_size(0, 0), Some(8));
        assert_eq!(sanitized_size(u64::MAX, 0), None);
    }

    #[test]
    fn best_fit_split_merge_and_async_half() {
        let mut a = Alloc::new(64);
        assert_eq!(a.alloc(16, 0, false), Ok(0));
        assert_eq!(a.alloc(8, 0, false), Ok(16));
        assert_eq!(a.alloc(16, 0, false), Ok(24));
        // free [0,16): two holes, 16 at 0 and 24 at 40; 16 bytes go to the exact fit
        assert!(a.free(0));
        assert_eq!(a.alloc(16, 0, false), Ok(0));
        assert!(a.free(16));
        assert!(!a.free(16));
        // freeing neighbours merges back to one range
        assert!(a.free(0));
        assert!(a.free(24));
        assert_eq!(a.alloc(64, 0, false), Ok(0));
        assert!(a.free(0));
        // async: half of 64 = 32
        assert_eq!(a.alloc(24, 0, true), Ok(0));
        assert_eq!(a.alloc(16, 0, true), Err(AllocError::NoSpace));
        assert_eq!(a.alloc(8, 0, true), Ok(24));
        assert!(a.free(0));
        assert_eq!(a.alloc(16, 0, true), Ok(0));
        // the region cap: all of 4 MiB, not a byte more
        let mut a = Alloc::new(u64::MAX);
        assert_eq!(
            a.alloc(MAX_REGION_SIZE + 8, 0, false),
            Err(AllocError::NoSpace)
        );
        assert_eq!(a.alloc(MAX_REGION_SIZE, 0, false), Ok(0));
    }
}
