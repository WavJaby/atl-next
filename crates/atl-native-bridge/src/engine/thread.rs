// SPDX-License-Identifier: MIT
//! The guest's per-thread bionic record (plan 4.2 "bionic thread record"): a `pthread_internal_t` and the `bionic_tls` it points to, laid out as build.rs generated from bionic's `pthread_internal.h` / `bionic_tls.h` (atl-bridgegen, guest view).
//! NBS bionic never creates its main thread's record: `__libc_init_main_thread_early` takes `__native_bridge_get_host_pthread()`'s answer as both the TCB's thread record and (in `_late`) the `TLS_SLOT_BIONIC_TLS` value. berberis answers host `pthread_self()` because its host is bionic; ours is glibc, so the engine builds one per host thread.
//! Set here: `tid`, the cached pid (bit-field), `bionic_tls`. Everything else zero, as bionic's own `__allocate_thread` leaves a fresh mapping before `__init_thread` (pthread_create.cpp).
//! Attach: a host thread's first down call (or a guest `pthread_create`'s new thread) gets that record, its own guest static TLS, a `HostThread` and the engine's guest CPU (`Engine::attach`); detach at thread exit (a host key's destructor) runs the guest keys' destructors on guest FS, then frees the TLS and record and detaches the CPU.

use std::alloc::{Layout, alloc_zeroed};
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use super::{Active, HostThread, active, call_guest};

/// Generated layouts, one per engine's guest bionic view (`SIZE`, `ALIGN`, one offset per field, `bits::<field>` = (shift, width) of a bit-field, the TLS slot macros): `identity` = the host arch, `qemu` = arm64. tls_defines.h slots differ per arch (x86_64 0..10, arm64 -3..7).
pub(super) mod layout {
    #[allow(clippy::all, dead_code, reason = "generated from bionic's headers")]
    pub mod identity {
        include!(concat!(env!("OUT_DIR"), "/bionic_thread_identity.rs"));
    }
    #[allow(clippy::all, dead_code, reason = "generated from bionic's headers")]
    pub mod qemu {
        include!(concat!(env!("OUT_DIR"), "/bionic_thread_qemu.rs"));
    }
}

/// What this file reads of one guest's generated layout: the active engine's ([`abi`]).
#[derive(Clone, Copy)]
struct Abi {
    record_size: usize,
    record_align: usize,
    tid: usize,
    /// `uint32_t cached_pid_ : 31; uint32_t vforked_ : 1;` (pthread_internal.h): both in one `uint32_t` storage unit.
    pid_unit: usize,
    /// (shift, width) of `cached_pid_`
    pid_bits: (u32, u32),
    /// the record's `bionic_tls` pointer field
    bionic_tls: usize,
    tls_size: usize,
    tls_align: usize,
    /// `NativeBridgeStaticTlsConfig`: size, tpoff, init_img, tls_slot_thread_id, tls_slot_bionic_tls
    cfg: [usize; 5],
}

macro_rules! abi {
    ($m:ident) => {{
        use layout::$m::{
            NativeBridgeStaticTlsConfig as c, bionic_tls as t, pthread_internal_t as r,
        };
        const _: () = {
            assert!(r::vforked_ == r::cached_pid_);
            assert!(r::bits::cached_pid_.0 + r::bits::cached_pid_.1 <= u32::BITS);
        };
        Abi {
            record_size: r::SIZE,
            record_align: r::ALIGN,
            tid: r::tid,
            pid_unit: r::cached_pid_,
            pid_bits: r::bits::cached_pid_,
            bionic_tls: r::bionic_tls,
            tls_size: t::SIZE,
            tls_align: t::ALIGN,
            cfg: [
                c::size,
                c::tpoff,
                c::init_img,
                c::tls_slot_thread_id,
                c::tls_slot_bionic_tls,
            ],
        }
    }};
}

/// The active engine's guest layout.
fn abi() -> Abi {
    match active() {
        Active::Identity => abi!(identity),
        Active::Qemu => abi!(qemu),
    }
}

thread_local! {
    static RECORD: Cell<usize> = const { Cell::new(0) };
    /// This thread's crossing record and guest thread pointer, once attached (const-initialised: no TLS destructor, readable inside pthread key destructors).
    static CURRENT: Cell<*mut HostThread> = const { Cell::new(std::ptr::null_mut()) };
}

/// Threads attached (the guest's starting thread included) / detached since process start; `detach` logs both.
pub static ATTACHED: AtomicU32 = AtomicU32::new(0);
pub static DETACHED: AtomicU32 = AtomicU32::new(0);

/// The host key whose destructor detaches a thread at its exit (value = its `HostThread`); created on first attach.
static DETACH_KEY: OnceLock<libc::pthread_key_t> = OnceLock::new();

/// Guest keys with a guest destructor (pthread_key_create's host side): glibc gets the key without one, `detach` runs it on the guest thread pointer.
static GUEST_DTORS: Mutex<Vec<(libc::pthread_key_t, usize)>> = Mutex::new(Vec::new());

/// bionic/glibc `PTHREAD_DESTRUCTOR_ITERATIONS`: rounds over the guest keys at thread exit.
const DTOR_ROUNDS: usize = 4;

/// Records guest key `key`'s destructor (`dtor` = guest code, 0 = none).
pub fn set_guest_dtor(key: libc::pthread_key_t, dtor: usize) {
    if let Ok(mut v) = GUEST_DTORS.lock() {
        v.retain(|&(k, _)| k != key);
        if dtor != 0 {
            v.push((key, dtor));
        }
    }
}

/// Makes `ht` the calling thread's record (the thread that started the guest: its TCB came from `Guest::start`).
pub fn adopt(ht: *mut HostThread) {
    CURRENT.with(|c| c.set(ht));
    ATTACHED.fetch_add(1, Ordering::Relaxed);
    register_detach(ht);
}

/// The calling thread's record, attaching it first: the engine's guest CPU (`Engine::attach`), guest static TLS (`alloc_static_tls`, the TCB words) and a crossing record. Aborts if no guest runs (a down stub exists only for a loaded library, so a guest does).
pub fn attach_current() -> *mut HostThread {
    let cur = CURRENT.with(Cell::get);
    if !cur.is_null() {
        return cur;
    }
    active().attach().unwrap_or_else(|e| super::unsupported(&e));
    let ht = static_tls()
        .ok_or_else(|| "attach: no guest static TLS (guest not started)".to_owned())
        .and_then(|t| alloc_static_tls(&t))
        .map(|tp| {
            let ht: *mut HostThread = HostThread::leak();
            // SAFETY: `tp` is a fresh guest TCB inside static TLS; `ht` is leaked until this thread's detach, after which no guest code runs on `tp`.
            unsafe {
                active().init_tcb(tp, ht);
                (*ht).set_guest_fs(tp);
            }
            ht
        })
        .unwrap_or_else(|e| super::unsupported(&e));
    adopt(ht);
    ht
}

fn register_detach(ht: *mut HostThread) {
    let key = *DETACH_KEY.get_or_init(|| {
        let mut k: libc::pthread_key_t = 0;
        // SAFETY: `k` is written by glibc; `detach` follows the destructor signature.
        if unsafe { libc::pthread_key_create(&mut k, Some(detach)) } != 0 {
            super::unsupported("attach: pthread_key_create for the detach key failed");
        }
        k
    });
    // SAFETY: a key of this process; the value is only read back by `detach`.
    unsafe { libc::pthread_setspecific(key, ht.cast()) };
}

/// Thread exit (the detach key's destructor, host thread pointer): the guest keys' destructors on the guest thread pointer, then the thread's guest TLS, records and crossing record are freed.
extern "C" fn detach(value: *mut libc::c_void) {
    let ht = value.cast::<HostThread>();
    for _ in 0..DTOR_ROUNDS {
        let dtors = GUEST_DTORS.lock().map(|v| v.clone()).unwrap_or_default();
        let mut ran = false;
        for (key, dtor) in dtors {
            // SAFETY: keys from the guest's (intercepted) pthread_key_create, values its own.
            let v = unsafe { libc::pthread_getspecific(key) };
            if v.is_null() {
                continue;
            }
            ran = true;
            // deliberate break (`ATL_NB_BREAK=qt_skip_dtor`): the guest destructor never runs (qemu-thread gate)
            if cfg!(atl_nb_qt_skip_dtor) {
                continue;
            }
            // SAFETY: as above; `dtor` is the guest's `void (*)(void*)`, `ht` this thread's record, and this thread's engine CPU is still attached (`Engine::detach` comes last).
            unsafe {
                libc::pthread_setspecific(key, std::ptr::null());
                call_guest(&mut *ht, dtor, v as usize, 0, 0, 0);
            }
        }
        if !ran {
            break;
        }
    }
    // SAFETY: `ht` was leaked by attach for this thread, which runs no guest code after its destructors; its guest TLS block was `alloc_static_tls`'s.
    unsafe {
        let tp = (*ht).guest_fs();
        if let Some(t) = static_tls()
            && tp != 0
        {
            free(tp - t.tpoff, t.size, page_size());
        }
        drop(Box::from_raw(ht));
    }
    release_record();
    CURRENT.with(|c| c.set(std::ptr::null_mut()));
    active().detach();
    let d = DETACHED.fetch_add(1, Ordering::Relaxed) + 1;
    #[allow(
        clippy::print_stderr,
        reason = "the process's log (atl-broker): the thread gate reads it"
    )]
    {
        eprintln!(
            "atl-native-bridge: thread detached (attached {}, detached {d})",
            ATTACHED.load(Ordering::Relaxed)
        );
    }
}

fn release_record() {
    let rec = RECORD.with(|r| r.replace(0));
    if rec == 0 {
        return;
    }
    let a = abi();
    // SAFETY: `rec` and its `bionic_tls` were allocated by `build` with these layouts (the engine is chosen once per process).
    unsafe {
        let tls = (rec as *const u8).add(a.bionic_tls).cast::<usize>().read();
        free(tls, a.tls_size, a.tls_align);
        free(rec, a.record_size, a.record_align);
    }
}

/// # Safety
/// `p` came from `zeroed(size, align)`.
unsafe fn free(p: usize, size: usize, align: usize) {
    let layout = Layout::from_size_align(size, align).expect("allocated with this layout");
    // SAFETY: caller contract.
    unsafe { std::alloc::dealloc(p as *mut u8, layout) };
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
    let [size, tpoff, init_img, slot_thread_id, slot_bionic_tls] = abi().cfg;
    // SAFETY: caller contract; offsets and field types per the generated layout (`size_t`, `size_t`, `const void*`, `int`, `int`).
    let t = unsafe {
        StaticTls {
            size: config.add(size).cast::<usize>().read(),
            tpoff: config.add(tpoff).cast::<usize>().read(),
            init_img: config.add(init_img).cast::<usize>().read(),
            slot_thread_id: config.add(slot_thread_id).cast::<i32>().read(),
            slot_bionic_tls: config.add(slot_bionic_tls).cast::<i32>().read(),
        }
    };
    if let Ok(mut g) = STATIC_TLS.lock() {
        *g = Some(t);
    }
}

/// The calling thread's guest static TLS, as bionic's `__allocate_thread` + `__init_tcb` make one (pthread_create.cpp): `init_img` copied, the thread-id slot = this thread's record, the bionic-TLS slot = its `bionic_tls`. Returns the thread pointer; the arch's own TCB words (x86 self pointer) are `Active::init_tcb`'s. Leaked: the thread's for its life.
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
            .add(abi().bionic_tls)
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
    let a = abi();
    let tls = zeroed(a.tls_size, a.tls_align);
    let rec = zeroed(a.record_size, a.record_align);
    // SAFETY: plain syscalls without arguments.
    let (tid, pid) = unsafe { (libc::gettid(), libc::getpid()) };
    let (shift, width) = a.pid_bits;
    let mask = (1u32 << width) - 1;
    let pid_bits = (pid as u32 & mask) << shift;
    // SAFETY: offsets inside `rec` (record_size bytes) per the generated layout; `tid` is `pid_t` (i32), the pid unit `uint32_t`, `bionic_tls` a pointer; each at its natural alignment there (clang's layout).
    unsafe {
        rec.add(a.tid).cast::<i32>().write(tid);
        rec.add(a.pid_unit).cast::<u32>().write(pid_bits);
        rec.add(a.bionic_tls).cast::<*mut u8>().write(tls);
    }
    rec as usize
}
