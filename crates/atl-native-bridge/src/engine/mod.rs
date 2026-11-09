// SPDX-License-Identifier: MIT
//! Identity engine (bridge-1 route N): bionic built for the host arch in its upstream native-bridge mode, run natively; a crossing switches the thread pointer only.
//! `Guest::start` does what the kernel does for a dynamic executable whose interpreter is the guest linker (fs/binfmt_elf.c): maps `linker64` and the NB vdso, builds the argument block (`args`), and runs the linker's `__linker_init` through the crossing.
//! The linker is run directly (AT_ENTRY = its own `_start`, bionic linker_main.cpp `__linker_init`): it loads the main executable named by argv[1] itself -- NBS `app_process64`, the guest main executable berberis also starts -- links its closure, runs every constructor, and returns the executable's entry, which the engine does not jump to.
//! Every thread that runs guest code has its own `HostThread` + guest static TLS (thread.rs attach/detach); guest threads are glibc threads (intercepts.rs `pthread_create`).

/// The crossing: the host arch's FS switch (x86_64), or with feature `qemu-guest` QEMU's binding thunks (`arch/qemu.rs`). Both provide the seam functions `x86_64.rs` names (`is_stub`, `patch_site`, `patch_hook`, `patch_entry`, `bind`, ...). Guest calls go through `call_guest` / `call_bootstrap` below, which are `Engine::call` on `arch::CROSSING` in both builds.
#[cfg(all(target_arch = "x86_64", not(feature = "qemu-guest")))]
#[path = "x86_64.rs"]
pub mod arch;
#[cfg(feature = "qemu-guest")]
#[path = "arch/qemu.rs"]
pub mod arch;
mod args;
pub mod bindings;
pub mod image;
mod intercepts;
pub mod jni;
mod ndk;
mod provider;
#[cfg(not(feature = "qemu-guest"))]
pub mod signal;
pub mod thread;
pub mod vdso;

use std::path::PathBuf;

pub use arch::HostThread;
use image::Image;

/// Host `sigaction(sig)`, then `signal::rewrap_all`: `handler(sig, info, ucontext)` runs on the host thread pointer even when the signal hit guest code (spike gate g3) -- the path every host handler takes, ART's sigchain included.
#[cfg(not(feature = "qemu-guest"))]
pub fn install_signal(
    sig: i32,
    handler: extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void),
) -> Result<(), String> {
    // SAFETY: a zeroed sigaction is a valid empty one; the handler follows the SA_SIGINFO signature.
    let rc = unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut())
    };
    if rc != 0 {
        return Err(format!(
            "sigaction({sig}): {}",
            std::io::Error::last_os_error()
        ));
    }
    signal::rewrap_all();
    Ok(())
}

/// A host entry the guest reached that this engine cannot serve: name it and abort.
#[allow(
    clippy::print_stderr,
    reason = "inside a guest call, before any logger: stderr is the one channel left"
)]
pub(crate) fn unsupported(what: &str) -> ! {
    eprintln!("atl-native-bridge: {what}");
    std::process::abort();
}

/// What the guest process is made of.
pub struct Config {
    /// the guest `linker64` (static-PIE)
    pub linker: PathBuf,
    /// NBS `libnative_bridge_vdso.so`
    pub vdso: PathBuf,
    /// the guest main executable the linker loads (argv[1])
    pub exe: PathBuf,
    /// the guest's `LD_LIBRARY_PATH` (bionic default namespace search path without a linker config)
    pub library_path: Vec<PathBuf>,
}

impl Config {
    /// The guest as the sandbox holds it (atl-android-paths `native_bridge_bin` / `native_bridge_lib`), for the crossing's guest ISA (identity: the host's own; QEMU: arm64).
    pub fn installed() -> Config {
        let isa = arch::GUEST_ISA;
        let bin = PathBuf::from(atl_android_paths::native_bridge_bin(isa));
        let lib = PathBuf::from(atl_android_paths::native_bridge_lib(isa));
        Config {
            linker: bin.join("linker64"),
            vdso: lib.join(VDSO_FILE),
            exe: bin.join("app_process64"),
            library_path: vec![lib],
        }
    }
}

/// NBS's vdso file name (soname `linux-vdso.so.1`), installed among the guest libraries.
pub const VDSO_FILE: &str = "libnative_bridge_vdso.so";

/// The thread pointer one guest call runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tp {
    /// `__linker_init` and the constructors it runs, before the linker has a TCB of its own. Identity: runs on the host's thread pointer, with the bootstrap record armed for this call only. QEMU (Q5-5): a scratch TCB.
    Bootstrap,
    /// A guest TCB, installed for the call. The thread pointer the guest leaves stays on the CPU for its next call.
    Guest(usize),
}

/// Runs guest code on a host thread's CPU and registers guest memory. Two implementations, one per build: `arch::Identity` (the host's FS switch) and `arch::Qemu` (QEMU's crossing); `arch::CROSSING` is the one this build selects.
pub trait Engine {
    /// The host thread's guest CPU. Identity: its crossing record. QEMU: its record too; the engine CPU belongs to the thread (`arch::attach_current`).
    type Cpu;
    /// Registers guest memory `[start, start + len)` with `prot` (`PROT_*`). Identity: nothing to register, host memory is guest memory. QEMU: page flags.
    fn map(&self, start: usize, len: usize, prot: u32) -> Result<(), String>;
    /// Runs guest `f(args)` on `cpu` with thread pointer `tp`; returns x0. Nests: may run from inside a host function the guest called.
    ///
    /// # Safety
    /// `f` must be guest code taking up to four integer arguments, pointer arguments valid for it; `cpu` is the calling thread's own.
    unsafe fn call(&self, cpu: &mut Self::Cpu, tp: Tp, f: usize, args: &[usize; 4]) -> usize;
}

/// The core's guest call on `cpu`, with the thread pointer its record holds.
///
/// # Safety
/// As [`Engine::call`]; `cpu` is the calling thread's own record.
pub(crate) unsafe fn call_guest(
    cpu: &mut HostThread,
    f: usize,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
) -> usize {
    let tp = Tp::Guest(cpu.guest_fs());
    // SAFETY: caller contract.
    unsafe { arch::CROSSING.call(cpu, tp, f, &[a, b, c, d]) }
}

/// `Guest::start`'s bootstrap call (`Tp::Bootstrap`).
///
/// # Safety
/// As [`call_guest`].
pub(crate) unsafe fn call_bootstrap(
    cpu: &mut HostThread,
    f: usize,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
) -> usize {
    // SAFETY: caller contract.
    unsafe { arch::CROSSING.call(cpu, Tp::Bootstrap, f, &[a, b, c, d]) }
}

/// A started guest: the linker initialised, its libraries constructed.
pub struct Guest {
    pub linker: Image,
    pub vdso: Image,
    /// the main executable's entry `__linker_init` returned
    pub exe_entry: usize,
    /// the starting thread's record: valid while that thread lives (force_host_fs only)
    #[cfg(not(feature = "qemu-guest"))]
    start_thread: *mut HostThread,
}

// SAFETY: a Guest owns its mappings; guest calls use the calling thread's own record (`thread::attach_current`); `start_thread` is dereferenced only by `force_host_fs`, documented for the starting thread.
unsafe impl Send for Guest {}

impl Guest {
    /// Maps and initialises the guest. Returns right after the crossing back, before any allocation, so a probe can run on whatever thread pointer the crossing left.
    pub fn start(cfg: &Config) -> Result<Guest, String> {
        if !arch::supported() {
            return Err(
                "FSGSBASE not available (HWCAP2_FSGSBASE): no user-mode thread-pointer switch"
                    .into(),
            );
        }
        let linker = Image::map(&cfg.linker)?;
        vdso::bind_linker(&linker)?;
        linker.seal()?;
        let vdso = Image::map(&cfg.vdso)?;
        vdso::bind(&vdso)?;
        vdso.seal()?;
        let init = linker
            .symbol("__dl___linker_init")
            .ok_or("linker64 has no __dl___linker_init")?;
        let block = args::build(cfg, &linker, &vdso, &arch::kernel_auxv()?)?;
        arch::map_guest(&[&linker, &vdso])?;
        let thread = HostThread::leak();
        // SAFETY: __linker_init(void* raw_args) is the guest linker's C entry (bionic linker_main.cpp), `block` its kernel-layout argument block, leaked for the process's life.
        let exe_entry = unsafe { call_bootstrap(thread, init, block, 0, 0, 0) };
        // the linker ran on a temporary TCB in its own (now returned) frame; NBS leaves the main thread's static TLS to the host
        let tls = thread::static_tls()
            .ok_or("the guest linker reported no static TLS (__native_bridge_config_static_tls)")?;
        let tp = thread::alloc_static_tls(&tls)?;
        // SAFETY: `tp` is a fresh guest TCB inside static TLS (bionic's TCB words lie in it); the record is leaked.
        unsafe { arch::init_tcb(tp, &raw mut *thread) };
        thread.set_guest_fs(tp);
        let start_thread: *mut HostThread = thread;
        thread::adopt(start_thread);
        Ok(Guest {
            linker,
            vdso,
            exe_entry,
            #[cfg(not(feature = "qemu-guest"))]
            start_thread,
        })
    }

    /// Calls guest `f(a, b, c)` on the calling thread's guest thread pointer (attaching the thread first if needed).
    ///
    /// # Safety
    /// `f` must be guest code with an integer signature of at most three arguments; pointer arguments must be valid for it.
    pub unsafe fn call(&mut self, f: usize, a: usize, b: usize, c: usize) -> usize {
        // SAFETY: caller contract; the record is this thread's.
        unsafe { call_guest(&mut *thread::attach_current(), f, a, b, c, 0) }
    }

    /// The calling thread's record (attached first if needed).
    pub fn thread(&self) -> &HostThread {
        // SAFETY: the record lives until this thread's exit.
        unsafe { &*thread::attach_current() }
    }

    /// Puts the host thread pointer back on the starting thread. Only for a probe that found the crossing left the guest's in place (g1's break): host TLS is unreadable then, so the record comes from `self`.
    #[cfg(not(feature = "qemu-guest"))]
    pub fn force_host_fs(&self) {
        // SAFETY: the starting thread's record (the probe runs on it, which has not exited); host_fs is the value the crossing read before entering the guest.
        unsafe { arch::set_fs_base((*self.start_thread).host_fs()) }
    }
}
