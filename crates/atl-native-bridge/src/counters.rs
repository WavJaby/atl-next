// SPDX-License-Identifier: MIT
//! Crossing counter (QEMU bridge plan section 5, Q5-8 / Q5-8b; feature `crossing-count`, absent from shipped builds).
//!
//! Per pool slot: the host target that ran and the crossings it took. Three pools, each its own slot space: QEMU's binding slots (`count_binding`, called by `dispatch::call_binding`), the identity engine's host thunks (`atl_nb_count_host`, called by the counted shells of `atl-bridgegen` `x86::entries_asm`), and the JNI slots of both engines (`count_jni`, called only by `dispatch::jni`: QEMU's `call_jni` and the identity JNI shell share that one entry). A JNI slot is one index into both the JNIEnv and the JavaVM table (one stub pool serves both), so the dump names it by both tables' entries. Per [`Mem`] kind: a histogram of the sizes the counted `memcpy`/`memset`/`memcmp` shims were given. Bin = bit length of the size: bin 0 = size 0, bin k = 2^(k-1) <= size < 2^k.
//!
//! Every 2^16 crossings the 16-bit tick wraps and [`dump`] writes the slots and histogram bins that are non-zero to fd 2. The dump allocates nothing, calls no libc function and touches no TLS: its line buffer is on the stack and it writes with a raw `syscall`, because the identity side counts on guest FS, where glibc's wrapper would read the host thread pointer (errno, cancellation).
//! Counts are relaxed atomics: exact, lock-free, and a crossing costs a few stores.

use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};

use crate::engine::bindings::qemu::THUNKS;
use crate::engine::identity::{JNI_STUBS, THUNKS as HOST_THUNKS};

/// Histogram bins: the bit lengths 0..=64 of a `usize` size.
pub const BINS: usize = usize::BITS as usize + 1;

/// The sized shims, each with its own histogram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mem {
    Memcpy = 0,
    Memset = 1,
    Memcmp = 2,
}

impl Mem {
    /// Histogram names in index order, for the dump.
    const NAMES: [&'static str; 3] = ["memcpy", "memset", "memcmp"];
}

/// One pool slot: the host target it last ran and its crossings.
struct Slot {
    target: AtomicUsize,
    calls: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Slot {
            target: AtomicUsize::new(0),
            calls: AtomicU64::new(0),
        }
    }
}

static SLOTS: [Slot; THUNKS] = [const { Slot::new() }; THUNKS];
/// Identity: the host thunk pool (`bind`). Both engines: the JNI slots.
static HOST: [Slot; HOST_THUNKS] = [const { Slot::new() }; HOST_THUNKS];
static JNI: [Slot; JNI_STUBS] = [const { Slot::new() }; JNI_STUBS];
static HIST: [[AtomicU64; BINS]; 3] = [const { [const { AtomicU64::new(0) }; BINS] }; 3];
/// Crossings modulo 2^16: wraps (dumps) every 65536.
static TICK: AtomicU16 = AtomicU16::new(0);
static DUMPS: AtomicU64 = AtomicU64::new(0);

/// Counts one crossing of QEMU's pool slot `slot` to host `target`.
pub fn count_binding(slot: usize, target: usize) {
    count(&SLOTS, slot, target);
}

/// The identity thunk shell's counter (`x86.rs` counted shells): host-thunk slot `slot` ran `target`. Called on guest FS with the argument registers saved: no TLS, no libc.
#[unsafe(no_mangle)]
pub extern "C" fn atl_nb_count_host(slot: usize, target: usize) {
    count(&HOST, slot, target);
}

/// Counts one guest->host JNI call of slot `slot` to host `target`. Only `dispatch::jni` calls it, so both engines count JNI through one entry. No TLS, no libc (the identity side runs it on guest FS).
pub(crate) fn count_jni(slot: usize, target: usize) {
    count(&JNI, slot, target);
}

/// Counts one crossing of `pool`'s slot `slot` to host `target`; dumps when the tick wraps.
fn count(pool: &[Slot], slot: usize, target: usize) {
    let s = &pool[slot];
    s.target.store(target, Ordering::Relaxed);
    s.calls.fetch_add(1, Ordering::Relaxed);
    if TICK.fetch_add(1, Ordering::Relaxed) == u16::MAX {
        dump();
    }
}

/// Every counted pool with its name in the dump: QEMU's `slot`, identity's `host`, both engines' `jni`.
fn pools() -> [(&'static str, &'static [Slot]); 3] {
    [("slot", &SLOTS), ("host", &HOST), ("jni", &JNI)]
}

/// Records a size given to a counted shim: its bin is the size's bit length.
pub fn count_size(kind: Mem, n: usize) {
    let bin = (usize::BITS - n.leading_zeros()) as usize;
    HIST[kind as usize][bin].fetch_add(1, Ordering::Relaxed);
}

/// The counted `memcpy` (bindings.rs `shim`, QEMU table under the feature): records the size, then glibc's `memcpy`.
///
/// # Safety
/// As glibc's `memcpy`: `src` and `dst` are valid for `n` bytes and do not overlap.
pub unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    count_size(Mem::Memcpy, n);
    // SAFETY: the caller's contract, as glibc's memcpy.
    unsafe { libc::memcpy(dst, src, n) }
}

/// The counted `memset`: records the size, then glibc's `memset`.
///
/// # Safety
/// As glibc's `memset`: `s` is valid for `n` bytes.
pub unsafe extern "C" fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void {
    count_size(Mem::Memset, n);
    // SAFETY: the caller's contract, as glibc's memset.
    unsafe { libc::memset(s, c, n) }
}

/// The counted `memcmp`: records the size, then glibc's `memcmp`.
///
/// # Safety
/// As glibc's `memcmp`: `a` and `b` are valid for `n` bytes.
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> c_int {
    count_size(Mem::Memcmp, n);
    // SAFETY: the caller's contract, as glibc's memcmp.
    unsafe { libc::memcmp(a, b, n) }
}

/// The counters at one instant, for the gates. `targets` sums the slots that ran each host target, so two slots bound to one target count together.
pub struct Snapshot {
    /// (host target, crossings), in slot order of first use.
    pub targets: Vec<(usize, u64)>,
    /// (JNI slot, crossings) of every JNI slot that ran, in slot order: the per-slot view `targets` folds away.
    pub jni: Vec<(usize, u64)>,
    /// Bin counts per [`Mem`] kind (index = `kind as usize`).
    pub sizes: [[u64; BINS]; 3],
    /// Dumps written so far: one per 2^16 crossings.
    pub dumps: u64,
}

/// The counters now. Allocates (the result is a `Vec`): for gates on the host thread, never from a crossing.
pub fn snapshot() -> Snapshot {
    let mut targets: Vec<(usize, u64)> = Vec::new();
    for s in pools().iter().flat_map(|&(_, pool)| pool.iter()) {
        let calls = s.calls.load(Ordering::Relaxed);
        if calls == 0 {
            continue;
        }
        let target = s.target.load(Ordering::Relaxed);
        match targets.iter_mut().find(|(t, _)| *t == target) {
            Some((_, c)) => *c += calls,
            None => targets.push((target, calls)),
        }
    }
    let sizes =
        std::array::from_fn(|k| std::array::from_fn(|b| HIST[k][b].load(Ordering::Relaxed)));
    let jni = JNI
        .iter()
        .enumerate()
        .map(|(k, s)| (k, s.calls.load(Ordering::Relaxed)))
        .filter(|&(_, c)| c != 0)
        .collect();
    Snapshot {
        targets,
        jni,
        sizes,
        dumps: DUMPS.load(Ordering::Relaxed),
    }
}

/// Writes the dump number, the non-empty slots (`<pool> <k> <target> <calls>`; a JNI slot `jni <k> <env entry>|<vm entry> <target> <calls>`, the vm part only where the vm table reaches `k`) and the non-empty histogram bins (`<kind> bin <b> <count>`) to fd 2. No allocation, no libc, no TLS.
fn dump() {
    let n = DUMPS.fetch_add(1, Ordering::Relaxed) + 1;
    let mut out = Out::new();
    out.text("crossing-count: dump ");
    out.dec(n);
    out.text(" (every 65536 crossings)\n");
    for (name, pool) in pools() {
        for (slot, s) in pool.iter().enumerate() {
            let calls = s.calls.load(Ordering::Relaxed);
            if calls != 0 {
                out.text("  ");
                out.text(name);
                out.text(" ");
                out.dec(slot as u64);
                out.text(" ");
                if std::ptr::eq(pool, JNI.as_slice()) {
                    let [env, vm] = crate::engine::jni::slot_names(slot);
                    out.text(env);
                    if !vm.is_empty() {
                        out.text("|");
                        out.text(vm);
                    }
                    out.text(" ");
                }
                out.hex(s.target.load(Ordering::Relaxed));
                out.text(" ");
                out.dec(calls);
                out.text("\n");
            }
        }
    }
    for (kind, name) in Mem::NAMES.iter().enumerate() {
        for (bin, c) in HIST[kind].iter().enumerate() {
            let c = c.load(Ordering::Relaxed);
            if c != 0 {
                out.text("  ");
                out.text(name);
                out.text(" bin ");
                out.dec(bin as u64);
                out.text(" ");
                out.dec(c);
                out.text("\n");
            }
        }
    }
    out.flush();
}

/// Stack line buffer for [`dump`]: text and numbers go in, [`Out::flush`] writes them to fd 2.
struct Out {
    buf: [u8; OUT_BYTES],
    len: usize,
}

/// Bytes per write: a dump line is at most ~60 bytes, so a full buffer is rare and flushes early.
const OUT_BYTES: usize = 512;

impl Out {
    fn new() -> Self {
        Out {
            buf: [0; OUT_BYTES],
            len: 0,
        }
    }

    fn byte(&mut self, b: u8) {
        if self.len == OUT_BYTES {
            self.flush();
        }
        self.buf[self.len] = b;
        self.len += 1;
    }

    fn text(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.byte(b);
        }
    }

    fn dec(&mut self, mut v: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for &b in &digits[i..] {
            self.byte(b);
        }
    }

    fn hex(&mut self, v: usize) {
        self.text("0x");
        for shift in (0..usize::BITS / 4).rev() {
            let d = ((v >> (shift * 4)) & 0xf) as u8;
            self.byte(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        }
    }

    fn flush(&mut self) {
        if self.len != 0 {
            write_raw(2, &self.buf[..self.len]);
            self.len = 0;
        }
    }
}

/// One `write(2)` of `buf` to `fd`, as a raw `syscall`. glibc's wrapper writes errno (TLS) on failure, which the identity side must not do on guest FS. Best effort: a failed write is not reported.
#[cfg(target_arch = "x86_64")]
fn write_raw(fd: usize, buf: &[u8]) {
    const SYS_WRITE: usize = 1;
    // SAFETY: write(2) reads `buf.len()` bytes at `buf.as_ptr()`, which the slice keeps valid for the call. `syscall` clobbers rcx and r11 (declared); it uses no stack and no TLS.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") SYS_WRITE => _,
            in("rdi") fd,
            in("rsi") buf.as_ptr(),
            in("rdx") buf.len(),
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
}

#[cfg(not(target_arch = "x86_64"))]
compile_error!("crossing-count: the raw write is x86_64 only (counters.rs write_raw)");
