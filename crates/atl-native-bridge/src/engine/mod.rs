// SPDX-License-Identifier: MIT
//! Identity engine (bridge-1 route N): bionic built for the host arch in its upstream native-bridge mode, run natively; a crossing switches the thread pointer only.
//! `Guest::start` does what the kernel does for a dynamic executable whose interpreter is the guest linker (fs/binfmt_elf.c): maps `linker64` and the NB vdso, builds the argument block (`args`), and runs the linker's `__linker_init` through the crossing.
//! The linker is run directly (AT_ENTRY = its own `_start`, bionic linker_main.cpp `__linker_init`): it loads the main executable named by argv[1] itself -- NBS `app_process64`, the guest main executable berberis also starts -- links its closure, runs every constructor, and returns the executable's entry, which the engine does not jump to.
//! Every thread that runs guest code has its own `HostThread` + guest static TLS (thread.rs attach/detach); guest threads are glibc threads (intercepts.rs `pthread_create`).

/// The crossing of the host arch (FS on x86_64; `tpidr_el0` / `tp` modules for arm64 / riscv64 later).
#[cfg(target_arch = "x86_64")]
#[path = "x86_64.rs"]
pub mod arch;
mod args;
pub mod bindings;
pub mod image;
mod intercepts;
pub mod jni;
mod provider;
pub mod signal;
pub mod thread;
pub mod vdso;

use std::path::PathBuf;

pub use arch::HostThread;
use image::Image;

/// Host `sigaction(sig)`, then `signal::rewrap_all`: `handler(sig, info, ucontext)` runs on the host thread pointer even when the signal hit guest code (spike gate g3) -- the path every host handler takes, ART's sigchain included.
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
    /// The guest as the sandbox holds it (atl-android-paths `native_bridge_bin` / `native_bridge_lib`): the identity guest's ISA is the host's own.
    pub fn installed() -> Config {
        let isa = std::env::consts::ARCH;
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

/// A started guest: the linker initialised, its libraries constructed.
pub struct Guest {
    pub linker: Image,
    pub vdso: Image,
    /// the main executable's entry `__linker_init` returned
    pub exe_entry: usize,
    /// the starting thread's record: valid while that thread lives (force_host_fs only)
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
        let block = args::build(cfg, &linker, &vdso)?;
        let thread = HostThread::leak();
        arch::set_bootstrap(Some(&raw mut *thread));
        // SAFETY: __linker_init(void* raw_args) is the guest linker's C entry (bionic linker_main.cpp), `block` its kernel-layout argument block, leaked for the process's life.
        let exe_entry = unsafe { arch::call_guest(thread, init, block, 0, 0, 0) };
        arch::set_bootstrap(None);
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
            start_thread,
        })
    }

    /// Calls guest `f(a, b, c)` on the calling thread's guest thread pointer (attaching the thread first if needed).
    ///
    /// # Safety
    /// `f` must be guest code with an integer signature of at most three arguments; pointer arguments must be valid for it.
    pub unsafe fn call(&mut self, f: usize, a: usize, b: usize, c: usize) -> usize {
        // SAFETY: caller contract; the record is this thread's.
        unsafe { arch::call_guest(&mut *thread::attach_current(), f, a, b, c, 0) }
    }

    /// The calling thread's record (attached first if needed).
    pub fn thread(&self) -> &HostThread {
        // SAFETY: the record lives until this thread's exit.
        unsafe { &*thread::attach_current() }
    }

    /// Puts the host thread pointer back on the starting thread. Only for a probe that found the crossing left the guest's in place (g1's break): host TLS is unreadable then, so the record comes from `self`.
    pub fn force_host_fs(&self) {
        // SAFETY: the starting thread's record (the probe runs on it, which has not exited); host_fs is the value the crossing read before entering the guest.
        unsafe { arch::set_fs_base((*self.start_thread).host_fs()) }
    }
}
