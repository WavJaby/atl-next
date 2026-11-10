// SPDX-License-Identifier: MIT
//! Library load, QEMU's `main`, and guest calls on the CPU state P1's hook hands over.

#[cfg(atl_qe_ret_stub_local)]
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
#[cfg(not(feature = "engine-so"))]
use std::path::Path;
#[cfg(not(atl_qe_ret_stub_local))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use atl_engine_api::AtlLanes;

use crate::layout::CPUArchState as Env;

/// What reached the host from the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// The bootstrap trap: the guest's x0. The answer becomes the guest's x0 (its exit status in the spike fixture).
    Boot(u64),
    /// A host call: the guest's x0..x7. The answer becomes the guest's x0.
    Call([u64; 8]),
    /// A binding thunk (`svc #(BIND | id)`, the pool of atl-bridgegen's `aarch64` emitter): the id and the guest's x0..x7. The answer becomes the guest's x0.
    Bind { id: u32, args: [u64; 8] },
    /// A value reported for a gate: the guest's x0. The answer is ignored.
    Report(u64),
}

/// The host side of every trap. Runs on the guest thread inside QEMU's `cpu_loop`; may call [`Guest::call`].
pub type OnTrap = fn(Guest, Trap) -> u64;

type MainFn = unsafe extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char) -> c_int;
type CpuLoopFn = unsafe extern "C" fn(*mut c_void);
pub(crate) type InCodeGenBufferFn = unsafe extern "C" fn(*const c_void) -> bool;
pub(crate) type PageGetFlagsFn = unsafe extern "C" fn(u64) -> c_int;
pub(crate) type PageSetFlagsFn = unsafe extern "C" fn(u64, u64, c_int, c_int);
pub(crate) type MmapLockFn = unsafe extern "C" fn();

/// QEMU's user-mode page flags (include/exec/page-protection.h:12-16), the `set` argument of [`Guest::map_pages`].
pub mod page {
    pub const READ: i32 = 0x1;
    pub const WRITE: i32 = 0x2;
    pub const EXEC: i32 = 0x4;
    /// The page is mapped for the guest; a page without it is unmapped and a fetch from it faults.
    pub const VALID: i32 = 0x8;
}

/// The library's entry points and data, resolved once by [`run`].
pub(crate) struct Lib {
    cpu_loop: CpuLoopFn,
    pub(crate) in_code_gen_buffer: InCodeGenBufferFn,
    /// `tcg_splitwx_diff` (tcg.h): rw minus rx address of the code buffer; 0 without split w^x.
    pub(crate) splitwx_diff: &'static isize,
    /// `guest_base` (QEMU `main.c`, set by `-B`): host = guest + guest_base.
    pub(crate) guest_base: &'static usize,
    /// `guest_addr_max` (linux-user/main.c): the highest guest address (`h2g_valid`, include/user/guest-host.h).
    pub(crate) guest_addr_max: &'static usize,
    /// `page_get_flags` (accel/tcg/user-exec.c): the QEMU page flags of a guest address.
    pub(crate) page_get_flags: PageGetFlagsFn,
    /// `page_set_flags(start, last, set, clear)` (accel/tcg/user-exec.c): `last` inclusive, needs the mmap lock.
    pub(crate) page_set_flags: PageSetFlagsFn,
    /// `mmap_lock` / `mmap_unlock` (linux-user/mmap.c): the lock `page_set_flags` asserts.
    pub(crate) mmap_lock: MmapLockFn,
    pub(crate) mmap_unlock: MmapLockFn,
    /// What a host-attached CPU needs from linux-user (attach.rs).
    pub(crate) thread: crate::attach::Api,
    /// Handle of the QEMU library (the spike's, or this engine `.so`): `helper_retaddr` and `thread_cpu` are dlsym'd on it later.
    pub(crate) handle: usize,
    pub(crate) on_trap: OnTrap,
}

static LIB: OnceLock<Lib> = OnceLock::new();

pub(crate) fn lib() -> &'static Lib {
    LIB.get().expect("atl-qemu-engine: start() not called")
}

/// The guest's `svc #RET` stub, from its BOOT trap (x1): guest calls return there. One address for the process: the boot executable's code is mapped for every CPU.
#[cfg(not(atl_qe_ret_stub_local))]
static RET_STUB: AtomicU64 = AtomicU64::new(0);

#[cfg(not(atl_qe_ret_stub_local))]
fn ret_stub() -> u64 {
    RET_STUB.load(Ordering::Acquire)
}

#[cfg(not(atl_qe_ret_stub_local))]
fn set_ret_stub(addr: u64) {
    RET_STUB.store(addr, Ordering::Release);
}

// break `ret-stub-local` (gate m): the stub is known on the BOOT thread only
#[cfg(atl_qe_ret_stub_local)]
thread_local! {
    static RET_STUB: Cell<u64> = const { Cell::new(0) };
}

#[cfg(atl_qe_ret_stub_local)]
fn ret_stub() -> u64 {
    RET_STUB.with(Cell::get)
}

#[cfg(atl_qe_ret_stub_local)]
fn set_ret_stub(addr: u64) {
    RET_STUB.with(|s| s.set(addr));
}

/// QEMU's symbol `name` in the library behind `handle`: dlsym in the spike's library (rlib build), the linked address in the engine `.so` ([`linked`]). The TLS names are dlsym'd on `handle` in both builds.
#[cfg(not(feature = "engine-so"))]
pub(crate) fn sym(handle: usize, name: &CStr) -> Result<*mut c_void, String> {
    dlsym(handle, name)
}

#[cfg(feature = "engine-so")]
pub(crate) fn sym(handle: usize, name: &CStr) -> Result<*mut c_void, String> {
    if name == c"thread_cpu" || name == c"helper_retaddr" {
        return dlsym(handle, name);
    }
    linked(name.to_bytes())
        .ok_or_else(|| format!("{}: not linked into the engine", name.to_string_lossy()))
}

/// dlsym of `name` in `handle`, or the dlerror text.
fn dlsym(handle: usize, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: handle came from dlopen (or dladdr + RTLD_NOLOAD) and is never closed; name is NUL-terminated.
    let p = unsafe { libc::dlsym(handle as *mut c_void, name.as_ptr()) };
    if p.is_null() {
        Err(format!("{}: {}", name.to_string_lossy(), dlerror()))
    } else {
        Ok(p)
    }
}

/// QEMU's symbols as the engine `.so` links them. The version script keeps them local, so dlsym cannot find them; the link resolves them. The declared types are placeholders: only addresses are taken, and the users cast them to their C prototypes (`Lib`, attach.rs `Api`).
#[cfg(feature = "engine-so")]
mod linked_syms {
    unsafe extern "C" {
        pub(super) fn atl_qemu_user_main();
        pub(super) fn cpu_loop();
        pub(super) fn in_code_gen_buffer();
        pub(super) fn page_get_flags();
        pub(super) fn page_set_flags();
        pub(super) fn mmap_lock();
        pub(super) fn mmap_unlock();
        pub(super) fn cpu_copy();
        pub(super) fn process_queued_cpu_work();
        pub(super) fn init_task_state();
        pub(super) fn task_settid();
        pub(super) fn rcu_register_thread();
        pub(super) fn rcu_unregister_thread();
        pub(super) fn tcg_register_thread();
        pub(super) fn object_unparent();
        pub(super) fn object_unref();
        pub(super) fn tcg_cflags_has();
        pub(super) fn tcg_cflags_set();
        pub(super) fn tb_flush__exclusive_or_serial();
        pub(super) static tcg_splitwx_diff: isize;
        pub(super) static guest_base: usize;
        pub(super) static guest_addr_max: usize;
    }
}

/// The address of a linked QEMU symbol, or None for a name the engine does not link.
#[cfg(feature = "engine-so")]
fn linked(name: &[u8]) -> Option<*mut c_void> {
    use linked_syms as s;
    let p: *const () = match name {
        b"atl_qemu_user_main" => s::atl_qemu_user_main as *const (),
        b"cpu_loop" => s::cpu_loop as *const (),
        b"in_code_gen_buffer" => s::in_code_gen_buffer as *const (),
        b"page_get_flags" => s::page_get_flags as *const (),
        b"page_set_flags" => s::page_set_flags as *const (),
        b"mmap_lock" => s::mmap_lock as *const (),
        b"mmap_unlock" => s::mmap_unlock as *const (),
        b"cpu_copy" => s::cpu_copy as *const (),
        b"process_queued_cpu_work" => s::process_queued_cpu_work as *const (),
        b"init_task_state" => s::init_task_state as *const (),
        b"task_settid" => s::task_settid as *const (),
        b"rcu_register_thread" => s::rcu_register_thread as *const (),
        b"rcu_unregister_thread" => s::rcu_unregister_thread as *const (),
        b"tcg_register_thread" => s::tcg_register_thread as *const (),
        b"object_unparent" => s::object_unparent as *const (),
        b"object_unref" => s::object_unref as *const (),
        b"tcg_cflags_has" => s::tcg_cflags_has as *const (),
        b"tcg_cflags_set" => s::tcg_cflags_set as *const (),
        b"tb_flush__exclusive_or_serial" => s::tb_flush__exclusive_or_serial as *const (),
        b"tcg_splitwx_diff" => return Some(&raw const s::tcg_splitwx_diff as *mut c_void),
        b"guest_base" => return Some(&raw const s::guest_base as *mut c_void),
        b"guest_addr_max" => return Some(&raw const s::guest_addr_max as *mut c_void),
        _ => return None,
    };
    Some(p as *mut c_void)
}

/// This `.so`'s own handle: dladdr of a function here, then dlopen RTLD_NOLOAD (the object is already loaded; the handle only names it). The TLS names are dlsym'd on it.
#[cfg(feature = "engine-so")]
fn self_handle() -> Result<usize, String> {
    let here = self_handle as *const () as *const c_void;
    // SAFETY: a zeroed Dl_info is a valid out-parameter; `here` is a function of this object.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::dladdr(here, &mut info) } == 0 || info.dli_fname.is_null() {
        return Err("engine: dladdr finds no object".to_owned());
    }
    // SAFETY: dli_fname is the NUL-terminated path of this loaded object; RTLD_NOLOAD loads nothing new.
    let handle = unsafe { libc::dlopen(info.dli_fname, libc::RTLD_NOW | libc::RTLD_NOLOAD) };
    if handle.is_null() {
        return Err(format!("dlopen (this engine): {}", dlerror()));
    }
    Ok(handle as usize)
}

fn dlerror() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated thread-local string.
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "no dlerror".to_owned()
    } else {
        // SAFETY: non-null, NUL-terminated (above).
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

/// `helper_retaddr` (QEMU `__thread`, nonzero while a helper or the translator touches guest memory) of the calling thread.
pub(crate) fn helper_retaddr_slot() -> Result<*const usize, String> {
    // dlsym of a __thread symbol answers the calling thread's instance
    sym(lib().handle, c"helper_retaddr").map(|p| p as *const usize)
}

/// Host stack of the boot helper thread: 8 MiB, the main thread's usual RLIMIT_STACK, which QEMU's `main` expects to run on (and the BOOT callback runs on top of it).
const BOOT_STACK: usize = 8 << 20;

/// Where the boot stands: the creator in [`start`] waits until the helper leaves `Pending`.
enum BootState {
    Pending,
    /// BOOT ran: the callback's answer and the boot CPU (`CPUArchState *`).
    Ready {
        answer: u64,
        env: usize,
    },
    /// The helper ended without BOOT: why.
    Ended(String),
}

static BOOT: (Mutex<BootState>, Condvar) = (Mutex::new(BootState::Pending), Condvar::new());

fn publish(state: BootState) {
    let (m, c) = &BOOT;
    *m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    c.notify_all();
}

/// The engine after BOOT: QEMU's boot thread is parked in its BOOT trap for the life of the process; host threads become guest CPUs with [`Booted::attach`].
pub struct Booted {
    answer: u64,
    boot_env: usize,
    helper: std::thread::JoinHandle<()>,
}

impl Booted {
    /// What the BOOT callback answered.
    pub fn answer(&self) -> u64 {
        self.answer
    }

    /// Whether the boot helper thread still exists (parked after BOOT). It never ends while the process runs.
    pub fn helper_parked(&self) -> bool {
        !self.helper.is_finished()
    }

    /// The calling host thread gets its own guest CPU, a copy of the boot CPU as linux-user's clone makes one. Detached when the [`crate::Cpu`] drops, on this thread.
    pub fn attach(&self) -> Result<crate::Cpu, String> {
        self.attach_sized(0, 0)
    }

    /// [`Self::attach`] on a guest stack of `size` bytes above a `guard` (rounded up to pages; 0 = default: 1 MiB, one page).
    pub fn attach_sized(&self, size: usize, guard: usize) -> Result<crate::Cpu, String> {
        crate::attach::attach(self.boot_env, size, guard)
    }
}

/// Boots QEMU on a helper thread: it loads `library` and runs QEMU's `main` with `argv` (argv[0] first) and this process's environment. The guest's BOOT trap runs `on_trap(Boot)` on that thread, publishes the answer, and parks the thread for good: QEMU's `main` never returns to a library caller (linux-user/main.c runs `cpu_loop` last), so this needs no QEMU change.
/// Returns once BOOT ran, or with why the helper ended without it. Call once per process. dlopen runs on the helper too, so QEMU's constructors (RCU's thread registration) belong to the thread that runs `main`.
#[cfg(not(feature = "engine-so"))]
pub fn start(library: &Path, argv: &[&str], on_trap: OnTrap) -> Result<Booted, String> {
    let path = CString::new(library.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    let args = c_args(argv)?;
    boot_on_helper(move || run_spike(&path, &args, on_trap))
}

/// The engine `.so`'s start (C ABI `create`): QEMU is linked into this object, so the helper runs its `main` on this object's own symbols. The command line is the engine's: `-B 0` is identity addresses (guest = host, the ABI's `map`), and the host names only `boot_exe`.
#[cfg(feature = "engine-so")]
pub(crate) fn start_linked(boot_exe: &str, on_trap: OnTrap) -> Result<Booted, String> {
    let args = c_args(&["qemu-aarch64", "-B", "0", boot_exe])?;
    boot_on_helper(move || run_main(self_handle()?, &args, on_trap))
}

fn c_args(argv: &[&str]) -> Result<Vec<CString>, String> {
    argv.iter()
        .map(|a| CString::new(*a).map_err(|e| e.to_string()))
        .collect()
}

/// Spawns the boot helper on `body`, then waits for BOOT (or for the helper to end without it).
fn boot_on_helper(
    body: impl FnOnce() -> Result<i32, String> + Send + 'static,
) -> Result<Booted, String> {
    let helper = std::thread::Builder::new()
        .name("qemu-boot".to_owned())
        .stack_size(BOOT_STACK)
        .spawn(move || {
            let why = match body() {
                Ok(status) => format!("QEMU main returned {status} without BOOT"),
                Err(e) => e,
            };
            publish(BootState::Ended(why));
        })
        .map_err(|e| format!("spawning the boot helper: {e}"))?;
    let (m, c) = &BOOT;
    let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // break no-boot-join (gate q): the creator returns without waiting for BOOT
    while matches!(*state, BootState::Pending) && !cfg!(atl_qe_no_boot_join) {
        state = c
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    match &*state {
        BootState::Ready { answer, env } => Ok(Booted {
            answer: *answer,
            boot_env: *env,
            helper,
        }),
        BootState::Ended(why) => Err(why.clone()),
        BootState::Pending if cfg!(atl_qe_no_boot_join) => Ok(Booted {
            answer: 0,
            boot_env: 0,
            helper,
        }),
        BootState::Pending => unreachable!("loop above"),
    }
}

/// The boot CPU's `CPUArchState *`, published when BOOT is entered and before its callback: from then on the callback may map and attach (D9). Once per process.
static BOOT_ENV: OnceLock<usize> = OnceLock::new();

/// The boot CPU that [`crate::attach`] copies from; None before BOOT.
#[cfg(feature = "engine-so")]
pub(crate) fn boot_env() -> Option<usize> {
    BOOT_ENV.get().copied()
}

/// The BOOT trap, on the boot helper: fault chain, parallel code, the callback, then the answer to [`start`] and a park that never ends.
pub(crate) fn boot(g: Guest) -> ! {
    set_ret_stub(g.x(1));
    crate::signal::install();
    crate::attach::parallel(g.env.cast());
    // break boot-late (gate q): the env is published after the callback, so map and attach inside BOOT fail
    #[cfg(not(atl_qe_boot_late))]
    let _ = BOOT_ENV.set(g.env as usize);
    let answer = (lib().on_trap)(g, Trap::Boot(g.x(0)));
    #[cfg(atl_qe_boot_late)]
    let _ = BOOT_ENV.set(g.env as usize);
    publish(BootState::Ready {
        answer,
        env: g.env as usize,
    });
    // the boot CPU stays outside `cpu_exec` (the hook runs between `cpu_exec_end` and the next start), so QEMU's exclusive sections never wait for it
    loop {
        std::thread::park();
    }
}

/// The spike's helper body: dlopen `path` (its initialisers run on this thread), then [`run_main`]. P1's `atl_svc_hook` binds to the test executable's own definition, which build.rs exports to the global scope.
#[cfg(not(feature = "engine-so"))]
fn run_spike(path: &CStr, args: &[CString], on_trap: OnTrap) -> Result<i32, String> {
    // RTLD_LOCAL: P1's call to `atl_svc_hook` still binds to the executable's exported definition
    // SAFETY: path is NUL-terminated; the library's initialisers are QEMU's own (QOM type registration).
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("dlopen {}: {}", path.to_string_lossy(), dlerror()));
    }
    run_main(handle as usize, args, on_trap)
}

/// `handle` is the QEMU library (the spike's, or this engine `.so`): resolve its symbols, run QEMU's `main`. Returns `main`'s status if it returns.
fn run_main(handle: usize, args: &[CString], on_trap: OnTrap) -> Result<i32, String> {
    let main = sym(handle, c"atl_qemu_user_main")?;
    let cpu_loop = sym(handle, c"cpu_loop")?;
    let in_cgb = sym(handle, c"in_code_gen_buffer")?;
    let splitwx = sym(handle, c"tcg_splitwx_diff")?;
    let guest_base = sym(handle, c"guest_base")?;
    let guest_addr_max = sym(handle, c"guest_addr_max")?;
    let page_get_flags = sym(handle, c"page_get_flags")?;
    let page_set_flags = sym(handle, c"page_set_flags")?;
    let mmap_lock = sym(handle, c"mmap_lock")?;
    let mmap_unlock = sym(handle, c"mmap_unlock")?;
    let lib = Lib {
        // SAFETY: cpu_loop is `void cpu_loop(CPUArchState *)` (include/user/cpu_loop.h with P1).
        cpu_loop: unsafe { std::mem::transmute::<*mut c_void, CpuLoopFn>(cpu_loop) },
        // SAFETY: `bool in_code_gen_buffer(const void *)` (include/tcg/tcg.h).
        in_code_gen_buffer: unsafe {
            std::mem::transmute::<*mut c_void, InCodeGenBufferFn>(in_cgb)
        },
        // SAFETY: `extern uintptr_t tcg_splitwx_diff` (tcg.h, signed use there), a global of the never-closed library.
        splitwx_diff: unsafe { &*(splitwx as *const isize) },
        // SAFETY: `uintptr_t guest_base` (common-user/probe-guest-base.c), a global of the never-closed library.
        guest_base: unsafe { &*(guest_base as *const usize) },
        // SAFETY: `unsigned long guest_addr_max`, a global of the never-closed library.
        guest_addr_max: unsafe { &*(guest_addr_max as *const usize) },
        // SAFETY: `int page_get_flags(vaddr)` (include/user/page-protection.h).
        page_get_flags: unsafe {
            std::mem::transmute::<*mut c_void, PageGetFlagsFn>(page_get_flags)
        },
        // SAFETY: `void page_set_flags(vaddr start, vaddr last, int set, int clear)` (include/user/page-protection.h).
        page_set_flags: unsafe {
            std::mem::transmute::<*mut c_void, PageSetFlagsFn>(page_set_flags)
        },
        // SAFETY: `void mmap_lock(void)` (linux-user/mmap.c, declared include/exec/mmap-lock.h).
        mmap_lock: unsafe { std::mem::transmute::<*mut c_void, MmapLockFn>(mmap_lock) },
        // SAFETY: `void mmap_unlock(void)`, as mmap_lock.
        mmap_unlock: unsafe { std::mem::transmute::<*mut c_void, MmapLockFn>(mmap_unlock) },
        thread: crate::attach::Api::resolve(handle)?,
        handle,
        on_trap,
    };
    LIB.set(lib)
        .map_err(|_| "atl-qemu-engine: start() called twice".to_owned())?;
    crate::signal::capture_previous();
    let mut ptrs: Vec<*mut c_char> = args.iter().map(|a| a.as_ptr().cast_mut()).collect();
    ptrs.push(std::ptr::null_mut());
    let argc = c_int::try_from(args.len()).map_err(|e| e.to_string())?;
    // SAFETY: `int main(int, char **, char **)` (linux-user/main.c); argv is NULL-terminated and outlives the call, environ is the process's.
    let main = unsafe { std::mem::transmute::<*mut c_void, MainFn>(main) };
    // SAFETY: as above.
    Ok(unsafe { main(argc, ptrs.as_mut_ptr(), process_environ()) })
}

fn process_environ() -> *mut *mut c_char {
    unsafe extern "C" {
        static mut environ: *mut *mut c_char;
    }
    // SAFETY: glibc's process environment pointer, read once here.
    unsafe { environ }
}

/// `page_set_flags(start, last, set, clear)` over `[start, start + len)` under QEMU's mmap lock. `clear` with `VALID` unmaps the range (QEMU's own munmap does the same, linux-user/mmap.c).
pub(crate) fn set_pages(start: usize, len: usize, set: i32, clear: i32) -> Result<(), String> {
    let last = start
        .checked_add(len)
        .and_then(|end| end.checked_sub(1))
        .map(|last| last as u64)
        .ok_or("set_pages: empty or wrapping range")?;
    let lib = lib();
    // QEMU asserts `last <= guest_addr_max`; refuse here instead of aborting in its assert
    if last > *lib.guest_addr_max as u64 {
        return Err(format!(
            "set_pages {start:#x}+{len:#x}: beyond guest_addr_max {:#x}",
            *lib.guest_addr_max
        ));
    }
    // SAFETY: the mmap lock and `page_set_flags` are QEMU's own (linux-user/mmap.c, accel/tcg/user-exec.c); QEMU's page table is live once `lib()` is set and BOOT ran, and the range is within `guest_addr_max`.
    unsafe {
        (lib.mmap_lock)();
        (lib.page_set_flags)(start as u64, last, set, clear);
        (lib.mmap_unlock)();
    }
    Ok(())
}

/// Byte offset of TPIDR_EL0 in `CPUArchState` (clang's `offsetof`, a `size_t` the generator types u64; an offset inside a 79 KB record fits any usize).
const TPIDR_EL0: usize = crate::layout::ATL_OFFSET_TPIDR_EL0 as usize;
const _: () = assert!(TPIDR_EL0 + 8 <= Env::SIZE);
/// `vfp.zregs` (v<i> is zregs[i]; its low 64 bits are `d[0]`): offsets and stride from clang (build.rs `FIELD_OFFSETS`).
const ZREGS: usize = crate::layout::ATL_OFFSET_ZREGS as usize;
const ZREG_SIZE: usize = crate::layout::ATL_SIZEOF_ZREG as usize;
const ZREG_D: usize = crate::layout::ATL_OFFSET_ZREG_D as usize;
const _: () = assert!(ZREGS + 32 * ZREG_SIZE <= Env::SIZE);

/// The guest CPU as P1's hook received it (`CPUArchState *`), valid while the hook runs.
#[derive(Clone, Copy, Debug)]
pub struct Guest {
    env: *mut u8,
}

impl Guest {
    pub(crate) fn new(env: *mut c_void) -> Self {
        Self { env: env.cast() }
    }

    fn reg(&self, offset: usize) -> u64 {
        // SAFETY: env is the live CPUArchState (hook contract); offset is a u64 field from its clang layout.
        unsafe { self.env.add(offset).cast::<u64>().read() }
    }

    fn set_reg(&self, offset: usize, v: u64) {
        // SAFETY: as in reg.
        unsafe { self.env.add(offset).cast::<u64>().write(v) }
    }

    /// x<i> (0..=30), or sp for 31.
    pub fn x(&self, i: usize) -> u64 {
        assert!(i < 32, "x{i}");
        self.reg(Env::xregs + 8 * i)
    }

    pub fn set_x(&self, i: usize, v: u64) {
        assert!(i < 32, "x{i}");
        self.set_reg(Env::xregs + 8 * i, v);
    }

    /// v<i> (0..=31): its low 64 bits, the lane a `double` argument or result uses.
    pub fn v(&self, i: usize) -> u64 {
        assert!(i < 32, "v{i}");
        self.reg(ZREGS + ZREG_SIZE * i + ZREG_D)
    }

    /// Sets v<i>'s low 64 bits and clears the high 64, as a write to d<i> does.
    pub fn set_v(&self, i: usize, v: u64) {
        assert!(i < 32, "v{i}");
        let at = ZREGS + ZREG_SIZE * i + ZREG_D;
        self.set_reg(at, v);
        self.set_reg(at + 8, 0);
    }

    /// Stack word `i` (0..8) at the guest's sp: where a call's stack arguments sit. `None` when the word's address is not mapped for the guest (QEMU bridge plan R-5: the trap reads all eight, and past a shallow thread's stack top they are refused, not read). Identity: a mapped word is read at its host address.
    pub fn stack_word(&self, i: usize) -> Option<u64> {
        assert!(i < 8, "stack word {i}");
        let addr = self.x(31) + 8 * i as u64;
        if addr > *lib().guest_addr_max as u64 || self.page_flags(addr) & page::VALID == 0 {
            return None;
        }
        // SAFETY: the page is mapped for the guest, so its host address is mapped (identity); an unaligned read is fine for any alignment.
        Some(unsafe { (self.host_addr(addr) as *const u64).read_unaligned() })
    }

    pub fn pc(&self) -> u64 {
        self.reg(Env::pc)
    }

    /// The guest thread pointer, TPIDR_EL0 (what the guest's `msr tpidr_el0` and linux-user's `cpu_set_tls` write).
    pub fn tp(&self) -> u64 {
        self.reg(TPIDR_EL0)
    }

    pub fn set_tp(&self, v: u64) {
        self.set_reg(TPIDR_EL0, v);
    }

    /// The host address of guest address `addr` (`g2h`: plus `guest_base`; identity at `-B 0`).
    pub fn host_addr(&self, addr: u64) -> usize {
        (addr as usize).wrapping_add(*lib().guest_base)
    }

    /// QEMU's page flags (`page::*`) for guest address `addr`; 0 = not mapped for the guest, so syscalls on it fail `access_ok`.
    pub fn page_flags(&self, addr: u64) -> i32 {
        // SAFETY: page_get_flags takes any guest address and answers 0 for one it does not track.
        unsafe { (lib().page_get_flags)(addr) }
    }

    /// Marks the guest pages of `[start, start + len)` as mapped with `flags` (`page::*`, `VALID` added here), identity: guest address = host address (`guest_base` 0). Host memory outside this table is unmapped to the guest, so a fetch from a page not marked faults.
    /// Needs QEMU booted (its page table is live from `main`); any thread, which takes the mmap lock. Before the first guest fetch from the range.
    pub fn map_pages(&self, start: usize, len: usize, flags: i32) -> Result<(), String> {
        set_pages(start, len, flags | page::VALID, 0)
    }

    /// Calls guest function `f` with `args` in x0.. and returns its x0, on the interrupted code's thread pointer. As [`Guest::call_on`].
    pub fn call(&self, f: u64, args: &[u64]) -> u64 {
        self.call_on(self.tp(), f, args).0
    }

    /// Calls guest function `f` with `args` in x0.. on thread pointer `tp`; returns its x0 and the thread pointer it returned with. The integer lanes of [`Guest::call_lanes`].
    pub fn call_on(&self, tp: u64, f: u64, args: &[u64]) -> (u64, u64) {
        assert!(args.len() <= 8, "{} arguments, x0..x7 only", args.len());
        let mut lanes = AtlLanes::default();
        lanes.x[..args.len()].copy_from_slice(args);
        let tp_out = self.call_lanes(tp, f, &mut lanes);
        (lanes.x[0], tp_out)
    }

    /// Calls guest function `f` with `lanes` on thread pointer `tp`: x0..x7, v0..v7 (low 64 bits) and the eight stack words at the callee's sp. The answer is `lanes.x[0]` and `lanes.v[0]`; returns the thread pointer the call returned with. Nested: may run from inside a trap callback, which may itself call again.
    /// The whole CPU state is saved and restored around the call (the thread pointer included), so the interrupted guest code resumes exactly where its trap left it.
    pub fn call_lanes(&self, tp: u64, f: u64, lanes: &mut AtlLanes) -> u64 {
        let ret = ret_stub();
        assert!(ret != 0, "no BOOT trap yet: no return stub");
        let mut saved = vec![0u8; Env::SIZE];
        // SAFETY: env is a live CPUArchState of Env::SIZE bytes (clang layout); saved is that long.
        unsafe { std::ptr::copy_nonoverlapping(self.env, saved.as_mut_ptr(), Env::SIZE) };
        self.set_tp(tp);
        for (i, a) in lanes.x.iter().enumerate() {
            self.set_x(i, *a);
        }
        // break `call-v` (qemu-abi gate, check d): the FP lanes are not set, so v0..v7 keep the CPU's values
        if !cfg!(atl_qe_call_v) {
            for (i, v) in lanes.v.iter().enumerate() {
                self.set_v(i, *v);
            }
        }
        // AAPCS64: sp 16-aligned; below the interrupted frame (no red zone on aarch64, the margin is for a stray store)
        let sp = (self.x(31) - 256) & !15;
        // break `call-stack` (qemu-abi gate, check d): the stack words are not written, so the callee's stack arguments are what lies at sp
        if !cfg!(atl_qe_call_stack) {
            for (i, w) in lanes.stack.iter().enumerate() {
                // SAFETY: sp..sp+64 lies in the CPU's guest stack below the interrupted sp, where the callee's own frame goes too; the host address is mapped (identity).
                unsafe { (self.host_addr(sp + 8 * i as u64) as *mut u64).write_unaligned(*w) };
            }
        }
        self.set_x(31, sp);
        self.set_x(30, ret);
        self.set_reg(Env::pc, f);
        // SAFETY: cpu_loop on this thread's CPU; P1 makes it return when the guest reaches `svc #RET` (trap.rs).
        unsafe { (lib().cpu_loop)(self.env.cast()) };
        lanes.x[0] = self.x(0);
        lanes.v[0] = self.v(0);
        let tp_out = self.tp();
        // SAFETY: as for the save.
        unsafe { std::ptr::copy_nonoverlapping(saved.as_ptr(), self.env, Env::SIZE) };
        tp_out
    }

    /// Whether guest and host addresses are the same (`guest_base` 0, `-B 0`). Not merely a convenience: with a nonzero base TCG on an x86_64 host sets GS to it (tcg/x86_64/tcg-target.c.inc, `ARCH_SET_GS`), which ART owns on every thread.
    pub fn identity(&self) -> bool {
        *lib().guest_base == 0
    }
}
