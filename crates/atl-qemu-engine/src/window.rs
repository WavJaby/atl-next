// SPDX-License-Identifier: MIT
//! The window allocator (phase 4 plan D8 obstacle 2): fresh host memory the guest can address, for everything the host hands a guest to dereference (images, stacks, TCB, auxv, vdso, copies).
//! It is QEMU's own guest mmap (`target_mmap`, anonymous, no hint), not a second allocator: under `-B 0` a 64-bit guest gets any host address; a 32-bit guest gets a host hole below `guest_addr_max` (4 GiB) found in `/proc/self/maps` and taken with MAP_FIXED_NOREPLACE (patch 0006 `mmap_find_vma_narrow`), so it coexists with low mappings QEMU did not make (ART's low-4G heap). The pages come back marked for the guest with `prot` (target_mmap's own page_set_flags). `MAP_32BIT` is not used: x86_64-only and 2 GiB.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::attach::AbiUlong;
use crate::engine::{lib, page};

/// Windows made since the engine loaded ([`windows`]).
static WINDOWS: AtomicU64 = AtomicU64::new(0);

/// `MAP_PRIVATE` / `MAP_ANONYMOUS` (linux-user/generic/target_mman.h; arm and aarch64 use the generic values): target values, which `target_mmap` translates.
const TARGET_MAP_PRIVATE: i32 = 0x02;
const TARGET_MAP_ANONYMOUS: i32 = 0x20;

/// A fresh zeroed anonymous window of `len` bytes (rounded up to pages by QEMU) with `prot` (`page::READ | WRITE | EXEC`, the target PROT_* values), marked for the guest; returns its address (guest = host, identity). Needs QEMU booted; any thread (QEMU takes its mmap lock). Freed with host `munmap` after clearing its page marks, as attach does for stacks.
pub fn map_window(len: usize, prot: i32) -> Result<usize, String> {
    if prot & !(page::READ | page::WRITE | page::EXEC) != 0 {
        return Err(format!("map_window: unknown prot {prot:#x}"));
    }
    let n = AbiUlong::try_from(len)
        .ok()
        .filter(|&n| n != 0)
        .ok_or_else(|| format!("map_window: length {len:#x} empty or beyond the guest's range"))?;
    // SAFETY: QEMU's guest mmap of fresh anonymous memory (no fd, no hint); it takes the mmap lock itself.
    let r = unsafe {
        (lib().thread.target_mmap)(0, n, prot, TARGET_MAP_PRIVATE | TARGET_MAP_ANONYMOUS, -1, 0)
    };
    // a negative target errno is in -4095..-1 (linux-user/mmap.c returns -TARGET_E*)
    if (-4095..0).contains(&r) {
        return Err(format!("map_window {len:#x}: target_mmap: {r}"));
    }
    WINDOWS.fetch_add(1, Ordering::Relaxed);
    Ok(r as AbiUlong as usize)
}

/// How many windows [`map_window`] made in this process.
pub fn windows() -> u64 {
    WINDOWS.load(Ordering::Relaxed)
}
