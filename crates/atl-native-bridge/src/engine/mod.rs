// SPDX-License-Identifier: MIT
//! Identity engine (bridge-1 route N): bionic built for the host arch in its upstream native-bridge mode, run natively; a crossing switches the thread pointer only.
//! `Guest::start` does what the kernel does for a dynamic executable whose interpreter is the guest linker (fs/binfmt_elf.c): maps `linker64` and the NB vdso, builds the argument block (`args`), and runs the linker's `__linker_init` through the crossing.
//! The linker is run directly (AT_ENTRY = its own `_start`, bionic linker_main.cpp `__linker_init`): it loads the main executable named by argv[1] itself -- NBS `app_process64`, the guest main executable berberis also starts -- links its closure, runs every constructor, and returns the executable's entry, which the engine does not jump to.
//! Every thread that runs guest code has its own `HostThread` + guest static TLS (thread.rs attach/detach); guest threads are glibc threads (intercepts.rs `pthread_create`).

mod args;
pub mod bindings;
mod dispatch;
/// The crossings, both compiled into every core: the host arch's FS switch (`identity/mod.rs`) and QEMU's binding thunks (`qemu/mod.rs`, placement in `qemu/place.rs`). Shared code reaches the process's one through [`active`] (an [`Active`]), whose methods dispatch to the functions both provide (`is_stub`, `patch_site`, `patch_hook`, `patch_entry`, `bind`, ...).
#[cfg(target_arch = "x86_64")]
pub mod identity;
pub mod image;
mod intercepts;
pub mod jni;
mod ndk;
mod provider;
pub mod qemu;
pub mod signal;
pub mod thread;
pub mod vdso;

use std::path::PathBuf;
use std::sync::OnceLock;

pub use identity::HostThread;
use image::Image;

/// The process's engine, chosen once (QEMU bridge plan 2.3, D-3): ART's `initialize` names the app's ISA ([`select`]); a process that never calls it runs the identity engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Active {
    /// the host ISA: guest bionic built for the host arch, the FS switch
    Identity,
    /// a foreign ISA (arm64): the engine `.so` (atl-android-paths `engine_library`)
    Qemu,
}

static ACTIVE: OnceLock<Active> = OnceLock::new();

/// The process's engine: [`select`]'s choice, identity when none was made (fixed from the first call on).
pub fn active() -> Active {
    *ACTIVE.get_or_init(|| Active::Identity)
}

/// Chooses the process's engine for app ISA `isa` (`None`: ART named none) and starts it ([`Engine::start`]). A second call must name the same engine: one bridge per process, one ABI per app (plan README "only one process width"), so a second foreign ISA is refused.
pub fn select(isa: Option<&str>) -> Result<Active, String> {
    let engine = claim(isa)?;
    engine.start()?;
    Ok(engine)
}

/// Makes the engine for `isa` the process's ([`decide`]) without starting it. [`select`] starts it next; `qemu::boot` claims QEMU itself, so a process that boots QEMU directly (the QEMU gates) runs it everywhere.
pub fn claim(isa: Option<&str>) -> Result<Active, String> {
    let want = decide(ACTIVE.get().copied(), isa)?;
    let got = *ACTIVE.get_or_init(|| want);
    if got != want {
        return Err(format!(
            "ISA {isa:?}: engine {want:?} lost the race to {got:?}"
        ));
    }
    Ok(got)
}

/// The engine for ISA `isa` in a process already running `current`: none or the host ISA -> identity, QEMU's guest ISA -> QEMU, anything else refused; a choice other than `current` is refused.
fn decide(current: Option<Active>, isa: Option<&str>) -> Result<Active, String> {
    let want = match isa {
        // deliberate break (`ATL_NB_BREAK=host_qemu`): the host ISA routed to QEMU (unit test `selection`)
        None | Some(identity::GUEST_ISA) if cfg!(atl_nb_host_qemu) => Active::Qemu,
        None | Some(identity::GUEST_ISA) => Active::Identity,
        Some(qemu::GUEST_ISA) => Active::Qemu,
        Some(other) => return Err(format!("ISA {other}: no engine runs it")),
    };
    match current {
        // deliberate break (`ATL_NB_BREAK=second_isa`): a second ISA accepted (unit test `selection`)
        Some(got) if got != want && !cfg!(atl_nb_second_isa) => Err(format!(
            "ISA {isa:?} needs the {want:?} engine, but this process already runs {got:?}: one engine per process"
        )),
        _ => Ok(want),
    }
}

impl Active {
    /// The engine's guest ISA (atl-android-paths `native_bridge_bin` / `native_bridge_lib` directory).
    pub fn guest_isa(self) -> &'static str {
        match self {
            Active::Identity => identity::GUEST_ISA,
            Active::Qemu => qemu::GUEST_ISA,
        }
    }

    /// The ELF machine of the libraries this engine runs (elf.rs `classify`): the host's, or QEMU's guest machine.
    pub fn machine(self) -> Result<object::elf::Machine, String> {
        match self {
            Active::Identity => crate::elf::host_machine(),
            Active::Qemu => Ok(qemu::GUEST_MACHINE),
        }
    }

    /// Boots the engine ([`Engine::start`]); once per process, later calls answer the first's result.
    fn start(self) -> Result<(), String> {
        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        STARTED
            .get_or_init(|| match self {
                Active::Identity => identity::CROSSING.start(),
                Active::Qemu => qemu::CROSSING.start(),
            })
            .clone()
    }

    /// Gives the calling thread its guest CPU ([`Engine::attach`]).
    fn attach(self, req: StackReq) -> Result<Stack, String> {
        match self {
            Active::Identity => identity::CROSSING.attach(req),
            Active::Qemu => qemu::CROSSING.attach(req),
        }
    }

    /// Takes the calling thread's guest CPU back ([`Engine::detach`]).
    fn detach(self) {
        match self {
            Active::Identity => identity::CROSSING.detach(),
            Active::Qemu => qemu::CROSSING.detach(),
        }
    }

    /// Whether this host can run the engine's crossing.
    pub fn supported(self) -> bool {
        match self {
            Active::Identity => identity::supported(),
            Active::Qemu => qemu::supported(),
        }
    }

    /// The kernel's auxv for the guest (args.rs).
    pub fn kernel_auxv(self) -> Result<Vec<(usize, usize)>, String> {
        match self {
            Active::Identity => identity::kernel_auxv(),
            Active::Qemu => qemu::kernel_auxv(),
        }
    }

    /// Makes `images` reachable by guest code.
    pub fn map_guest(self, images: &[&Image]) -> Result<(), String> {
        match self {
            Active::Identity => identity::map_guest(images),
            Active::Qemu => qemu::map_guest(images),
        }
    }

    /// The engine's own words of guest TCB `tp`.
    ///
    /// # Safety
    /// `tp` is a guest TCB, live while `ht` is used on it.
    pub unsafe fn init_tcb(self, tp: usize, ht: *mut HostThread) {
        // SAFETY: caller contract.
        unsafe {
            match self {
                Active::Identity => identity::init_tcb(tp, ht),
                Active::Qemu => qemu::init_tcb(tp, ht),
            }
        }
    }

    /// A guest-callable address that crosses to host function `target` with an integer result; None when the engine's thunks are used up.
    pub fn bind(self, target: usize) -> Option<usize> {
        self.bind_result(target, jni::Class::Int)
    }

    /// [`Active::bind`] for a host function whose result is of class `ret`: the QEMU engine answers the binding trap in x0 or v0 by it. Identity's native call returns in its own registers, so it ignores `ret`.
    pub fn bind_result(self, target: usize, ret: jni::Class) -> Option<usize> {
        match self {
            Active::Identity => identity::bind(target),
            Active::Qemu => qemu::bind_result(target, ret),
        }
    }

    /// Whether `addr` holds an unbound interceptable stub of this engine's guest ISA.
    pub fn is_stub(self, addr: usize) -> bool {
        match self {
            Active::Identity => identity::is_stub(addr),
            Active::Qemu => qemu::is_stub(addr),
        }
    }

    /// Points stub `addr` at `thunk`; false when `addr` is not a stub.
    pub fn patch_site(self, addr: usize, thunk: usize) -> bool {
        match self {
            Active::Identity => identity::patch_site(addr, thunk),
            Active::Qemu => qemu::patch_site(addr, thunk),
        }
    }

    /// Routes the linker hook at `at` to host function `host`.
    pub fn patch_hook(self, at: usize, host: usize) -> Result<(), &'static str> {
        match self {
            Active::Identity => identity::patch_hook(at, host),
            Active::Qemu => qemu::patch_hook(at, host),
        }
    }

    /// Routes the vdso entry whose value lies at `value_at` to host function `host`.
    pub fn patch_entry(
        self,
        vdso: &Image,
        value_at: usize,
        host: usize,
    ) -> Result<(), &'static str> {
        match self {
            Active::Identity => identity::patch_entry(vdso, value_at, host),
            Active::Qemu => qemu::patch_entry(vdso, value_at, host),
        }
    }

    /// The guest-callable address of JNI table slot `slot`.
    pub fn jni_stub(self, slot: usize) -> usize {
        match self {
            Active::Identity => identity::jni_stub(slot),
            Active::Qemu => qemu::jni_stub(slot),
        }
    }

    /// A host-callable address that runs guest `target` (loader.rs trampolines): a down call of `kind` whose arguments follow `shorty` (ART's JNI signature; None for JNI_OnLoad / JNI_OnUnload). QEMU places the arguments by it (place.rs `down`); identity's stub passes the caller's registers through.
    pub fn down(
        self,
        target: usize,
        kind: identity::Down,
        shorty: Option<&[u8]>,
    ) -> Result<usize, String> {
        match self {
            Active::Identity => identity::down(target, kind),
            Active::Qemu => qemu::down(target, kind, shorty),
        }
    }

    /// [`Engine::call`] on this engine.
    ///
    /// # Safety
    /// As [`Engine::call`].
    unsafe fn call(self, cpu: &mut HostThread, tp: Tp, f: usize, args: &[usize; 4]) -> usize {
        // SAFETY: caller contract.
        unsafe {
            match self {
                Active::Identity => identity::CROSSING.call(cpu, tp, f, args),
                Active::Qemu => qemu::CROSSING.call(cpu, tp, f, args),
            }
        }
    }
}

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
    /// The guest as the sandbox holds it (atl-android-paths `native_bridge_bin` / `native_bridge_lib`), for the active engine's guest ISA (identity: the host's own; QEMU: arm64).
    pub fn installed() -> Config {
        let isa = active().guest_isa();
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

/// The guest stack a thread asks for at attach (a guest `pthread_create` attr's stack and guard size); 0 = the engine's default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackReq {
    pub size: usize,
    pub guard: usize,
}

/// The stack a thread's guest code runs on, as bionic's `pthread_getattr_np` reports one: usable `[base, base + len)`, sp starts at the top, `guard` bytes of guard right below `base`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stack {
    pub base: usize,
    pub len: usize,
    pub guard: usize,
}

/// Runs guest code on a host thread's CPU and registers guest memory. Two implementations: `identity::Identity` (the host's FS switch) and `qemu::Qemu` (QEMU's crossing); [`Active`] dispatches to the process's one.
pub trait Engine {
    /// The host thread's guest CPU. Identity: its crossing record. QEMU: its record too; the engine CPU belongs to the thread (`qemu::attach_current`).
    type Cpu;
    /// Boots the engine for this process ([`select`], once). Identity: checks the crossing is supported. QEMU: loads the engine `.so` (atl-android-paths `engine_library`) and gives the calling thread its guest CPU.
    fn start(&self) -> Result<(), String>;
    /// Registers guest memory `[start, start + len)` with `prot` (`PROT_*`). Identity: nothing to register, host memory is guest memory. QEMU: page flags.
    fn map(&self, start: usize, len: usize, prot: u32) -> Result<(), String>;
    /// Gives the calling thread its guest CPU before its first guest call (thread.rs `attach_current`: a guest `pthread_create` thread, or any host thread that runs guest code). Once per thread; a second call keeps the first CPU and answers its stack (`req` unused). Returns the stack guest code on this thread runs on. Identity: no CPU to make, the thread's `HostThread` record is its CPU, and guest code runs on the host thread's own stack (glibc `pthread_getattr_np`; `req` sized it already, as the host thread's attr). QEMU: an engine CPU on a guest stack sized by `req` (`atl_engine.attach_ex`).
    fn attach(&self, req: StackReq) -> Result<Stack, String>;
    /// Takes the calling thread's guest CPU back at thread exit, after its guest destructors ran (thread.rs `detach`). Identity: nothing. QEMU: `atl_engine.detach`.
    fn detach(&self);
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
    unsafe { active().call(cpu, tp, f, &[a, b, c, d]) }
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
    unsafe { active().call(cpu, Tp::Bootstrap, f, &[a, b, c, d]) }
}

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
        let engine = active();
        if !engine.supported() {
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
        let block = args::build(cfg, &linker, &vdso, &engine.kernel_auxv()?)?;
        engine.map_guest(&[&linker, &vdso])?;
        let thread = HostThread::leak();
        // SAFETY: __linker_init(void* raw_args) is the guest linker's C entry (bionic linker_main.cpp), `block` its kernel-layout argument block, leaked for the process's life.
        let exe_entry = unsafe { call_bootstrap(thread, init, block, 0, 0, 0) };
        // the linker ran on a temporary TCB in its own (now returned) frame; NBS leaves the main thread's static TLS to the host
        let tls = thread::static_tls()
            .ok_or("the guest linker reported no static TLS (__native_bridge_config_static_tls)")?;
        let tp = thread::alloc_static_tls(&tls)?;
        // SAFETY: `tp` is a fresh guest TCB inside static TLS (bionic's TCB words lie in it); the record is leaked.
        unsafe { engine.init_tcb(tp, &raw mut *thread) };
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
        unsafe { call_guest(&mut *thread::attach_current(), f, a, b, c, 0) }
    }

    /// The calling thread's record (attached first if needed).
    pub fn thread(&self) -> &HostThread {
        // SAFETY: the record lives until this thread's exit.
        unsafe { &*thread::attach_current() }
    }

    /// Puts the host thread pointer back on the starting thread. Only for a probe that found the crossing left the guest's in place (g1's break): host TLS is unreadable then, so the record comes from `self`.
    pub fn force_host_fs(&self) {
        // SAFETY: the starting thread's record (the probe runs on it, which has not exited); host_fs is the value the crossing read before entering the guest.
        unsafe { identity::set_fs_base((*self.start_thread).host_fs()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-3: host ISA (or none) -> identity, QEMU's ISA -> QEMU, an unknown ISA and a second, different engine refused. Red under breaks `host_qemu` and `second_isa`.
    #[test]
    fn selection() {
        assert_eq!(decide(None, None), Ok(Active::Identity));
        assert_eq!(
            decide(None, Some(std::env::consts::ARCH)),
            Ok(Active::Identity)
        );
        assert_eq!(decide(None, Some("arm64")), Ok(Active::Qemu));
        assert!(decide(None, Some("mips")).is_err());
        assert_eq!(decide(Some(Active::Qemu), Some("arm64")), Ok(Active::Qemu));
        assert!(decide(Some(Active::Qemu), Some(std::env::consts::ARCH)).is_err());
        assert!(decide(Some(Active::Identity), Some("arm64")).is_err());
    }
}
