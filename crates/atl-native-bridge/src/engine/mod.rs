// SPDX-License-Identifier: MIT
//! Identity engine (bridge-1 route N): bionic built for the host arch in its upstream native-bridge mode, run natively; a crossing switches the thread pointer only.
//! `Guest::start` does what the kernel does for a dynamic executable whose interpreter is the guest linker (fs/binfmt_elf.c): maps `linker64` and the NB vdso, builds the argument block (`args`), and runs the linker's `__linker_init` through the crossing.
//! The linker is run directly (AT_ENTRY = its own `_start`, bionic linker_main.cpp `__linker_init`): it loads the main executable named by argv[1] itself -- NBS `app_process64`, the guest main executable berberis also starts -- links its closure, runs every constructor, and returns the executable's entry, which the engine does not jump to.
//! One thread so far: its `HostThread` is the record step 4.2's attach makes per thread.

/// The crossing of the host arch (FS on x86_64; `tpidr_el0` / `tp` modules for arm64 / riscv64 later).
#[cfg(target_arch = "x86_64")]
#[path = "x86_64.rs"]
pub mod arch;
mod args;
pub mod image;
pub mod thread;
pub mod vdso;

use std::path::PathBuf;

pub use arch::HostThread;
use image::Image;

/// Host `sigaction(sig)` through the engine's signal entry: `handler(sig, info, ucontext)` runs on the host thread pointer even when the signal hit guest code (spike gate g3).
/// Step 4 puts this under ART's sigchain (its `sigaction` claim), not beside it.
pub fn install_signal(
    sig: i32,
    handler: extern "C" fn(i32, *mut libc::siginfo_t, *mut libc::c_void),
) -> Result<(), String> {
    let n = usize::try_from(sig)
        .ok()
        .filter(|&n| n > 0 && n < arch::SIGNALS)
        .ok_or_else(|| format!("signal {sig} out of range"))?;
    arch::set_signal_handler(n, handler as *const () as usize);
    // SAFETY: a zeroed sigaction is a valid empty one; the entry point follows the SA_SIGINFO signature.
    let rc = unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = arch::signal_entry();
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
    Ok(())
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

/// A started guest: the linker initialised, its libraries constructed.
pub struct Guest {
    pub linker: Image,
    pub vdso: Image,
    /// the main executable's entry `__linker_init` returned
    pub exe_entry: usize,
    thread: &'static mut HostThread,
}

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
        let block = args::build(cfg, &linker, &vdso)?;
        let thread = HostThread::leak();
        // SAFETY: __linker_init(void* raw_args) is the guest linker's C entry (bionic linker_main.cpp), `block` its kernel-layout argument block, leaked for the process's life.
        let exe_entry = unsafe { arch::call_guest(thread, init, block, 0, 0) };
        // the linker ran on a temporary TCB in its own (now returned) frame; NBS leaves the main thread's static TLS to the host
        let tls = thread::static_tls()
            .ok_or("the guest linker reported no static TLS (__native_bridge_config_static_tls)")?;
        let tp = thread::alloc_static_tls(&tls)?;
        arch::init_tcb(tp);
        thread.set_guest_fs(tp);
        Ok(Guest {
            linker,
            vdso,
            exe_entry,
            thread,
        })
    }

    /// Calls guest `f(a, b, c)` on the guest thread pointer.
    ///
    /// # Safety
    /// `f` must be guest code with an integer signature of at most three arguments; pointer arguments must be valid for it.
    pub unsafe fn call(&mut self, f: usize, a: usize, b: usize, c: usize) -> usize {
        // SAFETY: caller contract.
        unsafe { arch::call_guest(self.thread, f, a, b, c) }
    }

    /// The record of the thread that started the guest.
    pub fn thread(&self) -> &HostThread {
        self.thread
    }

    /// Puts the host thread pointer back. Only for a probe that found the crossing left the guest's in place (g1's break).
    pub fn force_host_fs(&self) {
        // SAFETY: host_fs is the value the crossing read on this thread before entering the guest.
        unsafe { arch::set_fs_base(self.thread.host_fs()) }
    }
}
