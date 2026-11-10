// SPDX-License-Identifier: MIT
//! Host threads as guest CPUs. linux-user makes a CPU only for a guest `clone` (syscall.c `do_fork` + `clone_func`) and frees one only in the guest's `exit` (syscall.c `TARGET_NR_exit`). A host thread that calls into the guest goes through neither, so [`attach`] does those steps on the calling thread through QEMU's exported functions, and dropping the [`Cpu`] does `exit`'s.
//! Attach, in `do_fork` / `clone_func` order: `rcu_register_thread`; `tcg_register_thread` (user mode: sets this thread's `tcg_ctx`, NULL until then); `cpu_copy` of the boot CPU; a zeroed TaskState with `init_task_state` plus the parent's `bprm`, `info`, `signal_mask`, `gcs_el0_locked`, hung on `CPUState.opaque`; `thread_cpu` (read by `lock_user` for every syscall buffer); `task_settid`. Then ours: `helper_retaddr` registered for fault classification (signal.rs) and a guest stack.
//! Not done, with why: `clone_lock` is static in syscall.c, so attach serialises on its own lock and a guest clone may overlap an attach; a boot CPU with GCS on is refused (`do_fork` would give the child its own GCS stack); `child_tidptr` and the random seed are guest clone arguments a host thread has none of.
//! Parallel code: `begin_parallel_context` (linux-user/user-internals.h, static inline) runs for the boot CPU at BOOT ([`parallel`]), the only point where the flush's serial-context assert holds without a clone; every copy inherits `CF_PARALLEL` from it.

use std::ffi::c_void;
use std::sync::Mutex;

use crate::engine::{Guest, lib, page, set_pages, sym};
use crate::layout;

type VoidFn = unsafe extern "C" fn();
type PtrFn = unsafe extern "C" fn(*mut c_void);
type CpuCopyFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type CflagsHasFn = unsafe extern "C" fn(*mut c_void, u32) -> bool;
type CflagsSetFn = unsafe extern "C" fn(*mut c_void, u32);

/// linux-user's functions a host-attached CPU needs, resolved once with the library.
pub(crate) struct Api {
    /// `CPUArchState *cpu_copy(CPUArchState *)` (linux-user/main.c).
    cpu_copy: CpuCopyFn,
    /// `void init_task_state(TaskState *)` (linux-user/main.c), on zeroed memory.
    init_task_state: PtrFn,
    /// `void task_settid(TaskState *)` (linux-user/main.c).
    task_settid: PtrFn,
    /// `void rcu_register_thread(void)`, `void rcu_unregister_thread(void)` (util/rcu.c).
    rcu_register_thread: VoidFn,
    rcu_unregister_thread: VoidFn,
    /// `void tcg_register_thread(void)` (tcg/tcg.c, user-mode variant).
    tcg_register_thread: VoidFn,
    /// `void object_unparent(Object *)`, `void object_unref(void *)` (qom/object.c): the guest `exit`'s CPU teardown.
    object_unparent: PtrFn,
    object_unref: PtrFn,
    /// `bool tcg_cflags_has(CPUState *, uint32_t)`, `void tcg_cflags_set(CPUState *, uint32_t)` (include/exec/translation-block.h).
    tcg_cflags_has: CflagsHasFn,
    tcg_cflags_set: CflagsSetFn,
    /// `void tb_flush__exclusive_or_serial(void)` (accel/tcg/tb-maint.c).
    tb_flush: VoidFn,
    /// `void process_queued_cpu_work(CPUState *)` (cpu-common.c): runs a CPU's queued work (see attach).
    process_queued_cpu_work: PtrFn,
}

/// dlsym `$name` and type it as `$ty`, the prototype the field doc names.
macro_rules! func {
    ($h:expr, $name:literal, $ty:ty) => {{
        let p = sym($h, $name)?;
        // SAFETY: $ty is the C prototype of $name in the pinned QEMU (Api field doc); a function of the never-closed library.
        unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
    }};
}

impl Api {
    pub(crate) fn resolve(h: usize) -> Result<Self, String> {
        Ok(Self {
            cpu_copy: func!(h, c"cpu_copy", CpuCopyFn),
            init_task_state: func!(h, c"init_task_state", PtrFn),
            task_settid: func!(h, c"task_settid", PtrFn),
            rcu_register_thread: func!(h, c"rcu_register_thread", VoidFn),
            rcu_unregister_thread: func!(h, c"rcu_unregister_thread", VoidFn),
            tcg_register_thread: func!(h, c"tcg_register_thread", VoidFn),
            object_unparent: func!(h, c"object_unparent", PtrFn),
            object_unref: func!(h, c"object_unref", PtrFn),
            tcg_cflags_has: func!(h, c"tcg_cflags_has", CflagsHasFn),
            tcg_cflags_set: func!(h, c"tcg_cflags_set", CflagsSetFn),
            tb_flush: func!(h, c"tb_flush__exclusive_or_serial", VoidFn),
            process_queued_cpu_work: func!(h, c"process_queued_cpu_work", PtrFn),
        })
    }
}

/// QEMU's layout facts (build.rs `FIELD_OFFSETS`, clang's values for the pinned headers). Each is a size or offset inside a record of at most 79 KB, or a flag bit, so the `as` conversions are exact.
const SIZEOF_CPUSTATE: usize = layout::ATL_SIZEOF_CPUSTATE as usize;
const CPU_OPAQUE: usize = layout::ATL_OFFSET_CPU_OPAQUE as usize;
const SIZEOF_TASKSTATE: usize = layout::ATL_SIZEOF_TASKSTATE as usize;
const TS_INFO: usize = layout::ATL_OFFSET_TS_INFO as usize;
const TS_BPRM: usize = layout::ATL_OFFSET_TS_BPRM as usize;
const TS_SIGNAL_MASK: usize = layout::ATL_OFFSET_TS_SIGNAL_MASK as usize;
const SIZEOF_TS_SIGNAL_MASK: usize = layout::ATL_SIZEOF_TS_SIGNAL_MASK as usize;
const TS_GCS_EL0_LOCKED: usize = layout::ATL_OFFSET_TS_GCS_EL0_LOCKED as usize;
const GCSPR_EL0: usize = layout::ATL_OFFSET_GCSPR_EL0 as usize;
const GCSCR_EL0: usize = layout::ATL_OFFSET_GCSCR_EL0 as usize;
const GCSCR_PCRSEL: u64 = layout::ATL_GCSCR_PCRSEL;
const CF_PARALLEL: u32 = layout::ATL_CF_PARALLEL as u32;
const _: () = assert!(
    TS_GCS_EL0_LOCKED + 8 <= SIZEOF_TASKSTATE
        && TS_SIGNAL_MASK + SIZEOF_TS_SIGNAL_MASK <= SIZEOF_TASKSTATE
);

/// Default guest stack per attached CPU (attach_ex size 0): 1 MiB, bionic's default pthread stack order (D2). A host thread's own stack cannot serve: its `cpu_loop` frames live there while the guest runs.
const DEFAULT_STACK: usize = 1 << 20;

fn page_size() -> usize {
    // SAFETY: sysconf has no memory effects.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// `(stack, guard)` bytes for a request: each rounded up to pages, 0 = default. Default guard: one host page (bionic `PTHREAD_GUARD_SIZE`, D2). The guard is host PROT_NONE + not VALID in QEMU's page table, so a guest store there faults in TCG code and QEMU raises guest SIGSEGV/MAPERR instead of writing the mapping below.
fn stack_sizes(size: usize, guard: usize) -> Result<(usize, usize), String> {
    let page = page_size();
    // break `ignore-size`: the requested size is dropped, every stack gets the default
    let size = if size == 0 || cfg!(atl_qe_ignore_size) {
        DEFAULT_STACK
    } else {
        size
    };
    let guard = if guard == 0 { page } else { guard };
    match (
        size.checked_next_multiple_of(page),
        guard.checked_next_multiple_of(page),
    ) {
        (Some(s), Some(g)) if s.checked_add(g).is_some() => Ok((s, g)),
        _ => Err("attach: guest stack size overflows".to_owned()),
    }
}

/// Break `shared-stack`: the first stack attach made, handed to every later CPU too (never freed).
#[cfg(atl_qe_shared_stack)]
static SHARED_STACK: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Serialises attach and detach (linux-user holds `clone_lock` across the same steps; it is static in syscall.c).
static ATTACH: Mutex<()> = Mutex::new(());

/// `env_cpu` (include/exec/cpu-common.h): the CPUState sits right before the CPUArchState.
fn env_cpu(env: *mut u8) -> *mut u8 {
    env.wrapping_sub(SIZEOF_CPUSTATE)
}

/// `thread_cpu` (linux-user/main.c `__thread CPUState *`) of the calling thread: dlsym of a TLS symbol answers the caller's instance.
fn thread_cpu_slot() -> Result<*mut *mut u8, String> {
    sym(lib().handle, c"thread_cpu").map(|p| p.cast())
}

/// `begin_parallel_context(cs)` for the boot CPU at BOOT: flush and set `CF_PARALLEL` unless set. On the boot thread inside its trap, where `current_cpu` is the boot CPU and still serial, as the flush asserts.
pub(crate) fn parallel(env: *mut u8) {
    let api = &lib().thread;
    let cpu = env_cpu(env).cast();
    // SAFETY: cpu is the live boot CPUState (env from the BOOT hook); the flush runs in a serial context (doc above).
    unsafe {
        if !(api.tcg_cflags_has)(cpu, CF_PARALLEL) {
            (api.tb_flush)();
            (api.tcg_cflags_set)(cpu, CF_PARALLEL);
        }
    }
}

/// A host thread's guest CPU, from [`crate::Booted::attach`]. Not `Send`: QEMU's per-thread state (`thread_cpu`, `tcg_ctx`, RCU) belongs to the attaching thread, which also detaches it on drop.
pub struct Cpu {
    env: *mut u8,
    /// The TaskState this attach allocated; null when the CPU is not this thread's own (break `shared-cpu`).
    ts: *mut u8,
    /// Lowest usable byte of the guest stack; `len` bytes above it, `guard` bytes of guard below it.
    stack: usize,
    len: usize,
    guard: usize,
}

impl Cpu {
    /// The CPU as a [`Guest`]: `call_on` runs guest code on it, on this thread.
    pub fn guest(&self) -> Guest {
        Guest::new(self.env.cast())
    }

    /// Top of this CPU's guest stack.
    pub fn stack_top(&self) -> usize {
        self.stack + self.len
    }

    /// Lowest usable byte of this CPU's guest stack; the guard lies right below it.
    pub fn stack_base(&self) -> usize {
        self.stack
    }

    /// Guard bytes right below [`Cpu::stack_base`] (the request rounded up, or the default).
    pub fn stack_guard(&self) -> usize {
        self.guard
    }
}

fn read_u64(base: *const u8, offset: usize) -> u64 {
    // SAFETY: base is a live QEMU record and offset a u64 field of it (clang layout, module consts).
    unsafe { base.add(offset).cast::<u64>().read_unaligned() }
}

fn write_u64(base: *mut u8, offset: usize, v: u64) {
    // SAFETY: as in read_u64.
    unsafe { base.add(offset).cast::<u64>().write_unaligned(v) }
}

/// `size` and `guard` as [`stack_sizes`] reads them.
pub(crate) fn attach(boot_env: usize, size: usize, guard: usize) -> Result<Cpu, String> {
    let (len, guard) = stack_sizes(size, guard)?;
    let lib = lib();
    let api = &lib.thread;
    let boot_env = boot_env as *mut u8;
    let _held = ATTACH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if read_u64(boot_env, GCSCR_EL0) & GCSCR_PCRSEL != 0 {
        return Err("attach: the boot CPU has GCS on; a host CPU would need its own GCS stack (do_fork's gcs_new_stack)".to_owned());
    }
    let slot = thread_cpu_slot()?;
    // SAFETY: this thread's thread_cpu instance, a plain pointer word.
    if !unsafe { slot.read() }.is_null() {
        return Err("attach: this thread already has a guest CPU".to_owned());
    }
    // SAFETY: QEMU's per-thread registration, on the thread that will run the CPU (clone_func's first two calls).
    // RCU has no break: skipping it only lets `synchronize_rcu` miss this thread's readers (a use-after-free window), which no deterministic gate observes; measured green with it skipped.
    unsafe {
        (api.rcu_register_thread)();
        if !cfg!(atl_qe_skip_tcg) {
            (api.tcg_register_thread)();
        }
    }
    let (env, ts) = if cfg!(atl_qe_shared_cpu) {
        (boot_env, std::ptr::null_mut())
    } else {
        // SAFETY: boot_env is the boot CPU's live CPUArchState; cpu_copy creates, resets and fills a new CPU from it (do_fork's call).
        let env = unsafe { (api.cpu_copy)(boot_env.cast()) }.cast::<u8>();
        (env, task_state(api, boot_env, env)?)
    };
    let cpu = env_cpu(env);
    if !cfg!(atl_qe_skip_thread_cpu) {
        // SAFETY: this thread's thread_cpu instance (above); cpu is this thread's CPU from here on.
        unsafe { slot.write(cpu) };
    }
    if !ts.is_null() {
        // SAFETY: ts is this CPU's TaskState (task_state).
        unsafe { (api.task_settid)(ts.cast()) };
    }
    if !cfg!(atl_qe_no_init_drain) {
        // The copy's queued work is its plugin init hook (plugins/core.c qemu_plugin_vcpu_init__async), which enters the CPU in `plugin.cpu_ht`. Clone's child runs it before its first guest instruction; an attached CPU may never run guest code, and detach asserts the entry (plugins/core.c:293), so attach runs it here.
        // SAFETY: cpu is this thread's complete CPU (cpu_copy, TaskState); the work runs on the thread that owns it.
        unsafe { (api.process_queued_cpu_work)(cpu.cast()) };
    }
    crate::signal::register_thread();
    let stack = guest_stack(len, guard)?;
    Guest::new(env.cast()).set_x(31, (stack + len) as u64);
    Ok(Cpu {
        env,
        ts,
        stack,
        len,
        guard,
    })
}

/// `do_fork`'s TaskState for the new CPU `env`: zeroed, `init_task_state`, the boot thread's `bprm`, `info`, `signal_mask`, `gcs_el0_locked`; hung on `CPUState.opaque`. GCSPR_EL0 = 0, as `do_fork` sets it with GCS off.
fn task_state(api: &Api, boot_env: *mut u8, env: *mut u8) -> Result<*mut u8, String> {
    let parent = read_u64(env_cpu(boot_env), CPU_OPAQUE) as *const u8;
    if parent.is_null() {
        return Err("attach: the boot CPU has no TaskState".to_owned());
    }
    // SAFETY: calloc of the TaskState's size; freed in detach (QEMU's exit path g_frees its own, which is glibc free underneath).
    let ts = unsafe { libc::calloc(1, SIZEOF_TASKSTATE) }.cast::<u8>();
    if ts.is_null() {
        return Err("attach: no memory for a TaskState".to_owned());
    }
    // SAFETY: ts is zeroed TaskState memory, init_task_state's precondition.
    unsafe { (api.init_task_state)(ts.cast()) };
    for field in [TS_BPRM, TS_INFO, TS_GCS_EL0_LOCKED] {
        write_u64(ts, field, read_u64(parent, field));
    }
    // SAFETY: both are TaskStates of SIZEOF_TASKSTATE bytes; the mask lies inside (const assert above).
    unsafe {
        std::ptr::copy_nonoverlapping(
            parent.add(TS_SIGNAL_MASK),
            ts.add(TS_SIGNAL_MASK),
            SIZEOF_TS_SIGNAL_MASK,
        );
    }
    write_u64(env_cpu(env), CPU_OPAQUE, ts as u64);
    write_u64(env, GCSPR_EL0, 0);
    Ok(ts)
}

/// One mapping of `guard` + `len` bytes (page multiples), bionic's layout (`__allocate_thread_mapping`): the low guard PROT_NONE and never registered with QEMU, the stack above it registered read/write so syscalls on stack buffers pass QEMU's checks. Returns the stack's base (mapping + guard).
fn guest_stack(len: usize, guard: usize) -> Result<usize, String> {
    #[cfg(atl_qe_shared_stack)]
    if let Some(&s) = SHARED_STACK.get() {
        return Ok(s);
    }
    // SAFETY: a fresh private anonymous mapping, all PROT_NONE; unmapped in detach.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            guard + len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err("attach: mmap of the guest stack failed".to_owned());
    }
    // break `no-guard`: the guard page is ordinary stack memory, RW on the host and VALID for QEMU
    let low = if cfg!(atl_qe_no_guard) { 0 } else { guard };
    let open = p as usize + low;
    // SAFETY: [open, p + guard + len) lies inside the mapping just made.
    if unsafe {
        libc::mprotect(
            open as *mut c_void,
            guard + len - low,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    } != 0
    {
        // SAFETY: the mapping just made, unused.
        unsafe { libc::munmap(p, guard + len) };
        return Err("attach: mprotect of the guest stack failed".to_owned());
    }
    set_pages(
        open,
        guard + len - low,
        page::READ | page::WRITE | page::VALID,
        0,
    )?;
    let stack = p as usize + guard;
    #[cfg(atl_qe_shared_stack)]
    let _ = SHARED_STACK.set(stack);
    Ok(stack)
}

impl Drop for Cpu {
    /// The guest `exit` path (syscall.c `TARGET_NR_exit`) for this thread's CPU: unparent and unref the CPU, clear `thread_cpu`, free the TaskState, leave RCU. Plus ours: the stack and the fault slot.
    fn drop(&mut self) {
        let api = &lib().thread;
        let _held = ATTACH
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cfg!(atl_qe_shared_stack) {
            let (guard, len) = (self.guard, self.len);
            let map = self.stack - guard;
            // clearing VALID on the never-registered guard is a no-op; covering it keeps the no-guard break's pages consistent too
            let _ = set_pages(map, guard + len, 0, page::VALID);
            // SAFETY: the mapping guest_stack made (guard + stack); no guest code runs on it any more.
            unsafe { libc::munmap(map as *mut c_void, guard + len) };
        }
        crate::signal::unregister_thread();
        if !self.ts.is_null() {
            let cpu = env_cpu(self.env).cast::<c_void>();
            // SAFETY: this thread's own CPU (cpu_copy in attach), idle now; the exit path's teardown order.
            unsafe {
                (api.object_unparent)(cpu);
                (api.object_unref)(cpu);
            }
        }
        if let Ok(slot) = thread_cpu_slot() {
            // SAFETY: this thread's thread_cpu instance.
            unsafe { slot.write(std::ptr::null_mut()) };
        }
        // SAFETY: the TaskState attach calloc'd (null when none); its CPU is gone.
        unsafe { libc::free(self.ts.cast()) };
        // SAFETY: this thread registered in attach.
        unsafe { (api.rcu_unregister_thread)() };
    }
}
