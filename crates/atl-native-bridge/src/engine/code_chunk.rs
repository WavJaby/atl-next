// SPDX-License-Identifier: MIT
//! Generated host code made at run time: the down shells of both engines (identity `down`, QEMU place.rs `down`). One allocator, no fixed count (berberis' `CodePool` is unbounded too).
//! W^X: each chunk is one memfd mapped twice, a RW view the allocator writes and a RX view the code runs from; no page is ever writable and executable, and a placed piece never changes protection. A full chunk stays mapped (its code is in use); pieces are never freed, as shells are bound for the process.
//! Thread safety: one mutex, held for the bump, the copy of one piece (tens of bytes) and the icache flush; a chunk's mmap happens under it once per [`CHUNK`] bytes.
//! Counters: [`placed`] = pieces placed, [`chunks`] = chunks mapped; each new chunk logs one stderr line with both.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes per chunk: 2,048 32-byte shells.
const CHUNK: usize = 64 * 1024;
/// Every piece starts on this boundary: the down shells' 8-byte literals stay naturally aligned (riscv64 `ld`), and a piece never straddles a cache line it does not need.
const ALIGN: usize = 16;

/// The chunk being filled.
struct Chunk {
    rw: usize,
    rx: usize,
    used: usize,
}

static CURRENT: Mutex<Option<Chunk>> = Mutex::new(None);
static PLACED: AtomicUsize = AtomicUsize::new(0);
static CHUNKS: AtomicUsize = AtomicUsize::new(0);

/// Pieces placed so far (all engines' down shells).
pub fn placed() -> usize {
    PLACED.load(Ordering::Relaxed)
}

/// Chunks mapped so far.
pub fn chunks() -> usize {
    CHUNKS.load(Ordering::Relaxed)
}

fn new_chunk() -> Result<Chunk, String> {
    // SAFETY: memfd + two shared mappings of CHUNK bytes; the fd is closed once mapped (the mappings keep the file).
    unsafe {
        let fd = libc::memfd_create(c"atl-nb-code".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
            return Err(format!(
                "code chunk: memfd: {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::ftruncate(fd, CHUNK as libc::off_t) != 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(format!("code chunk: ftruncate: {e}"));
        }
        let map = |prot| libc::mmap(std::ptr::null_mut(), CHUNK, prot, libc::MAP_SHARED, fd, 0);
        let rw = map(libc::PROT_READ | libc::PROT_WRITE);
        let rx = map(libc::PROT_READ | libc::PROT_EXEC);
        let e = std::io::Error::last_os_error();
        libc::close(fd);
        if rw == libc::MAP_FAILED || rx == libc::MAP_FAILED {
            for v in [rw, rx] {
                if v != libc::MAP_FAILED {
                    libc::munmap(v, CHUNK);
                }
            }
            return Err(format!("code chunk: mmap: {e}"));
        }
        Ok(Chunk {
            rw: rw as usize,
            rx: rx as usize,
            used: 0,
        })
    }
}

/// Places `len` bytes of host code: `fill` writes them into the RW view (its second argument is the address the code will run at), then the instruction cache is made coherent for the RX range. Returns that RX address, [`ALIGN`]-aligned.
pub fn place(len: usize, fill: impl FnOnce(&mut [u8], usize)) -> Result<usize, String> {
    assert!(
        len <= CHUNK,
        "code chunk: a {len}-byte piece exceeds a chunk"
    );
    let mut cur = CURRENT
        .lock()
        .map_err(|_| "code chunk: poisoned".to_owned())?;
    if cur.as_ref().is_none_or(|c| c.used + len > CHUNK) {
        // the full chunk stays mapped: its code is in use
        *cur = Some(new_chunk()?);
        let n = CHUNKS.fetch_add(1, Ordering::Relaxed) + 1;
        #[allow(
            clippy::print_stderr,
            reason = "the process's stderr is its log (atl-broker); one line per 64 KiB of generated code"
        )]
        {
            eprintln!(
                "atl-native-bridge: code chunk {n} mapped ({} pieces placed)",
                placed()
            );
        }
    }
    let Some(c) = cur.as_mut() else {
        return Err("code chunk: none mapped".into());
    };
    let at = c.used;
    // SAFETY: `at + len <= CHUNK` bytes into the RW view, which only this lock holder writes; the RX view shows the same pages.
    let rw = unsafe { std::slice::from_raw_parts_mut((c.rw + at) as *mut u8, len) };
    let rx = c.rx + at;
    fill(rw, rx);
    flush_icache(rx, len);
    c.used = (at + len).next_multiple_of(ALIGN);
    PLACED.fetch_add(1, Ordering::Relaxed);
    Ok(rx)
}

/// Makes code written through the RW view visible to instruction fetch at `rx..rx + len`. riscv64 needs it (no coherent I-cache: Zifencei / the kernel's `riscv_flush_icache`, glibc `__riscv_flush_icache`, flags 0 = every hart running this mm); x86_64 keeps instruction fetch coherent with stores (Intel SDM Vol. 3, 11.6 self-modifying code), and no other host has a down backend (atl-bridgegen down.rs `backend`).
#[cfg(target_arch = "riscv64")]
fn flush_icache(rx: usize, len: usize) {
    unsafe extern "C" {
        /// glibc sys/cachectl.h: `int __riscv_flush_icache(void *start, void *end, unsigned long flags)`.
        fn __riscv_flush_icache(
            start: *mut libc::c_void,
            end: *mut libc::c_void,
            flags: libc::c_ulong,
        ) -> libc::c_int;
    }
    // SAFETY: the range is mapped (this chunk's RX view); the call only orders the instruction stream.
    let rc = unsafe { __riscv_flush_icache(rx as *mut _, (rx + len) as *mut _, 0) };
    assert_eq!(
        rc,
        0,
        "riscv_flush_icache failed: {}",
        std::io::Error::last_os_error()
    );
}

#[cfg(not(target_arch = "riscv64"))]
fn flush_icache(_rx: usize, _len: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allocator's contract: each piece is ALIGN-aligned, filled at the address it reports, readable there with the bytes `fill` wrote, and distinct pieces never overlap; past one chunk's worth a second chunk is mapped. 3,000 32-byte pieces = 96,000 bytes > CHUNK (65,536), so at least two chunks.
    #[test]
    fn pieces_are_aligned_distinct_and_grow_past_a_chunk() {
        let (p0, c0) = (placed(), chunks());
        let mut seen = std::collections::HashSet::new();
        for k in 0u32..3000 {
            let at = place(32, |b, rx| {
                b.fill(0);
                b[..4].copy_from_slice(&k.to_le_bytes());
                b[8..16].copy_from_slice(&rx.to_le_bytes());
            })
            .expect("a piece");
            assert_eq!(at % ALIGN, 0);
            // SAFETY: test read of the RX view the allocator just returned, 32 bytes.
            let got = unsafe { std::slice::from_raw_parts(at as *const u8, 32) };
            assert_eq!(got[..4], k.to_le_bytes());
            assert_eq!(got[8..16], at.to_le_bytes());
            assert!(
                seen.insert(at / 32),
                "piece {k} at {at:#x} overlaps another"
            );
        }
        assert_eq!(placed() - p0, 3000);
        assert!(chunks() - c0 >= 2);
    }
}
