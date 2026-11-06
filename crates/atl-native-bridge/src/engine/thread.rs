// SPDX-License-Identifier: MIT
//! The guest's per-thread bionic record (plan 4.2 "bionic thread record"): a `pthread_internal_t` and the `bionic_tls` it points to, laid out as build.rs generated from bionic's `pthread_internal.h` / `bionic_tls.h` (atl-bridgegen, guest view).
//! NBS bionic never creates its main thread's record: `__libc_init_main_thread_early` takes `__native_bridge_get_host_pthread()`'s answer as both the TCB's thread record and (in `_late`) the `TLS_SLOT_BIONIC_TLS` value. berberis answers host `pthread_self()` because its host is bionic; ours is glibc, so the engine builds one per host thread.
//! Set here: `tid`, the cached pid (bit-field), `bionic_tls`. Everything else zero, as bionic's own `__allocate_thread` leaves a fresh mapping before `__init_thread` (pthread_create.cpp).

use std::alloc::{Layout, alloc_zeroed};
use std::cell::Cell;

/// Generated layouts (`SIZE`, `ALIGN`, one offset per field, `bits::<field>` = (shift, width) of a bit-field).
#[allow(clippy::all, dead_code, reason = "generated from bionic's headers")]
mod layout {
    include!(concat!(env!("OUT_DIR"), "/bionic_thread.rs"));
}
use layout::{NativeBridgeStaticTlsConfig as tls_config, bionic_tls, pthread_internal_t as record};

/// `uint32_t cached_pid_ : 31; uint32_t vforked_ : 1;` (pthread_internal.h): both in one `uint32_t` storage unit.
const PID_UNIT: usize = record::cached_pid_;
const _: () = {
    assert!(record::vforked_ == PID_UNIT);
    assert!(record::bits::cached_pid_.0 + record::bits::cached_pid_.1 <= u32::BITS);
};

thread_local! {
    static RECORD: Cell<usize> = const { Cell::new(0) };
}

/// The calling host thread's record, built on first use; leaked: the guest TCB keeps pointing at it for the thread's life.
pub fn current() -> usize {
    RECORD.with(|r| {
        if r.get() == 0 {
            r.set(build());
        }
        r.get()
    })
}

/// The guest's static-TLS description (`NativeBridgeStaticTlsConfig`), as the linker reported it.
#[derive(Clone, Copy, Debug)]
pub struct StaticTls {
    /// Bytes of one thread's static TLS, a multiple of its alignment (at most a page).
    pub size: usize,
    /// Thread pointer - start of the block.
    pub tpoff: usize,
    /// `size` bytes the block starts as (TLS segments' images, the TCB's DTV and stack guard).
    pub init_img: usize,
    pub slot_thread_id: i32,
    pub slot_bionic_tls: i32,
}

static STATIC_TLS: std::sync::Mutex<Option<StaticTls>> = std::sync::Mutex::new(None);

/// What the linker reported (`__native_bridge_config_static_tls`, NBS `__libc_init_main_thread_final`); None before.
pub fn static_tls() -> Option<StaticTls> {
    STATIC_TLS.lock().ok().and_then(|g| *g)
}

/// Records the linker's report. `config` points at a `NativeBridgeStaticTlsConfig` the guest keeps alive for the call; its `init_img` for the process.
///
/// # Safety
/// `config` must point at a complete `NativeBridgeStaticTlsConfig` of the generated layout.
pub unsafe fn configure(config: *const u8) {
    // SAFETY: caller contract; offsets and field types per the generated layout (`size_t`, `size_t`, `const void*`, `int`, `int`).
    let t = unsafe {
        StaticTls {
            size: config.add(tls_config::size).cast::<usize>().read(),
            tpoff: config.add(tls_config::tpoff).cast::<usize>().read(),
            init_img: config.add(tls_config::init_img).cast::<usize>().read(),
            slot_thread_id: config
                .add(tls_config::tls_slot_thread_id)
                .cast::<i32>()
                .read(),
            slot_bionic_tls: config
                .add(tls_config::tls_slot_bionic_tls)
                .cast::<i32>()
                .read(),
        }
    };
    if let Ok(mut g) = STATIC_TLS.lock() {
        *g = Some(t);
    }
}

/// The calling thread's guest static TLS, as bionic's `__allocate_thread` + `__init_tcb` make one (pthread_create.cpp): `init_img` copied, the thread-id slot = this thread's record, the bionic-TLS slot = its `bionic_tls`. Returns the thread pointer; the arch's own TCB words (x86 self pointer) are `arch::init_tcb`'s. Leaked: the thread's for its life.
/// Pointer-sized slots are host words: the identity engine's guest has the host's pointer width.
pub fn alloc_static_tls(t: &StaticTls) -> Result<usize, String> {
    if t.tpoff > t.size || t.size == 0 {
        return Err(format!(
            "static TLS: tpoff {:#x} outside size {:#x}",
            t.tpoff, t.size
        ));
    }
    let word = std::mem::size_of::<usize>() as isize;
    for slot in [t.slot_thread_id, t.slot_bionic_tls] {
        let at = t.tpoff as isize + slot as isize * word;
        if at < 0 || at + word > t.size as isize {
            return Err(format!("static TLS: slot {slot} outside the block"));
        }
    }
    let block = zeroed(t.size, page_size());
    let rec = current();
    // SAFETY: `init_img` holds `size` bytes (NBS `new char[layout.size()]`); `block` is `size` fresh bytes. Slots are words at `tp + i * word` inside the block (bionic's TCB lies inside static TLS, `offset_bionic_tcb`); `bionic_tls` is a pointer field of the record built by `build`.
    unsafe {
        std::ptr::copy_nonoverlapping(t.init_img as *const u8, block, t.size);
        let tp = block.add(t.tpoff).cast::<usize>();
        let tls = (rec as *const u8)
            .add(record::bionic_tls)
            .cast::<usize>()
            .read();
        tp.offset(t.slot_thread_id as isize).write(rec);
        tp.offset(t.slot_bionic_tls as isize).write(tls);
        Ok(tp as usize)
    }
}

fn page_size() -> usize {
    // SAFETY: no memory effects.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

fn zeroed(size: usize, align: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, align).expect("generated size/align form a layout");
    // SAFETY: non-zero size (generated from a complete record).
    let p = unsafe { alloc_zeroed(layout) };
    assert!(!p.is_null(), "bionic thread record: out of memory");
    p
}

fn build() -> usize {
    let tls = zeroed(bionic_tls::SIZE, bionic_tls::ALIGN);
    let rec = zeroed(record::SIZE, record::ALIGN);
    // SAFETY: plain syscalls without arguments.
    let (tid, pid) = unsafe { (libc::gettid(), libc::getpid()) };
    let (shift, width) = record::bits::cached_pid_;
    let mask = (1u32 << width) - 1;
    let pid_bits = (pid as u32 & mask) << shift;
    // SAFETY: offsets inside `rec` (SIZE bytes) per the generated layout; `tid` is `pid_t` (i32), the pid unit `uint32_t`, `bionic_tls` a pointer; each at its natural alignment there (clang's layout).
    unsafe {
        rec.add(record::tid).cast::<i32>().write(tid);
        rec.add(PID_UNIT).cast::<u32>().write(pid_bits);
        rec.add(record::bionic_tls).cast::<*mut u8>().write(tls);
    }
    rec as usize
}
