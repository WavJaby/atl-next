// SPDX-License-Identifier: MIT
//! Identity engine (bridge-1 route N): bionic built for the host arch in its upstream native-bridge mode, run natively; a crossing switches the thread pointer only.
//! `Guest::start` does what the kernel does for a dynamic executable whose interpreter is the guest linker (fs/binfmt_elf.c): maps `linker64` and the NB vdso, builds the argument block (`args`), and runs the linker's `__linker_init` through the crossing.
//! The linker is run directly (AT_ENTRY = its own `_start`, bionic linker_main.cpp `__linker_init`): it loads the main executable named by argv[1] itself -- NBS `app_process64`, the guest main executable berberis also starts -- links its closure, runs every constructor, and returns the executable's entry, which the engine does not jump to.
//! Every thread that runs guest code has its own `HostThread` + guest static TLS (thread.rs attach/detach); guest threads are glibc threads (intercepts.rs `pthread_create`).

mod args;
pub mod bindings;
mod dispatch;
/// The fatal-signal report both engines use (identity's signal paths, QEMU's `atl_host.fault_report`).
pub mod fault_report;
/// The identity crossing (the host arch's thread-pointer switch), a per-host-ISA capability: compiled where build.rs `IDENTITY_HOSTS` names the host ISA (cfg `atl_nb_identity`). QEMU's binding thunks (`qemu/mod.rs`, placement in `qemu/place.rs`) are in every core. Shared code reaches the process's engine through [`active`] (an [`Active`]), whose methods dispatch to the functions each provides (`is_stub`, `patch_site`, `patch_hook`, `patch_entry`, `bind`, ...).
#[cfg(atl_nb_identity)]
pub mod identity;
pub mod image;
mod intercepts;
pub mod jni;
mod ndk;
mod netdb;
pub mod objects;
/// `eglGetProcAddress` (GS2.2 G1): a row's guest address, or a host answer bound by its descriptor.
pub mod proc;
mod provider;
pub mod qemu;
mod record;
/// Host signal handlers wrapped at install onto the identity crossing's signal entry; QEMU never moves the host thread pointer, so it needs none.
#[cfg(atl_nb_identity)]
pub mod signal;
pub mod sles;
pub mod thread;
mod va_list;
pub mod vdso;

use std::path::PathBuf;
use std::sync::OnceLock;

use image::Image;
pub use record::HostThread;

/// The host's ISA string (arches.toml key, ART instruction-set name): the identity engine's guest ISA where the core has it.
pub const HOST_ISA: &str = std::env::consts::ARCH;

/// What a down call's host arguments are, so the crossing hands the guest its own objects for them (jni.rs, sles.rs). Both engines: identity carries it in the stub (`identity` `Down::code`), QEMU in the down slot.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Down {
    /// passed through: plain C, `@CriticalNative`
    Plain,
    /// a host `JNIEnv*` -> this thread's guest env (Regular / FastNative JNI methods)
    Env,
    /// a host `JavaVM*` -> the guest vm (`JNI_OnLoad`)
    Vm,
    /// an OpenSL ES callback: its first argument a host interface of proxy table `t` -> its guest proxy (sles.rs `down_args`, which also rewrites `slObjectCallback`'s 6th)
    Itf(objects::TableId),
}

impl Down {
    /// Rewrites the host argument words `first` and `sixth` of a down call of this kind to the guest's objects for them. Both engines' crossings call it before the guest runs.
    pub(in crate::engine) fn guest_args(self, first: &mut usize, sixth: &mut usize) {
        match self {
            Down::Plain => {}
            Down::Env => *first = jni::guest_env(*first),
            Down::Vm => *first = jni::guest_vm(*first),
            Down::Itf(t) => sles::down_args(t, first, sixth),
        }
    }
}

/// The process's engine, chosen once (QEMU bridge plan 2.3, D-3): ART's `initialize` names the app's ISA ([`select`]); a process that never calls it runs [`DEFAULT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Active {
    /// the host ISA: guest bionic built for the host arch, the thread-pointer switch (cores built with `atl_nb_identity` only)
    #[cfg(atl_nb_identity)]
    Identity,
    /// a foreign ISA, the guest architecture's engine `.so` (atl-android-paths `engine_library`)
    Qemu(qemu::Arch),
}

static ACTIVE: OnceLock<Active> = OnceLock::new();

/// [`Active::reserve`] blocks not yet given back through [`Active::release`]: the thread gate's churn check reads it back to its start value after create + join.
pub static RESERVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A down shell's identity: guest target, kind, shorty.
type DownKey = (usize, Down, Option<Vec<u8>>);
/// Every down shell made ([`Active::down`]), by what it runs.
static DOWN_SHELLS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<DownKey, usize>>,
> = std::sync::LazyLock::new(Default::default);

/// A sigchain special handler (`art::SigchainAction::sc_sigaction`; NativeBridge `getSignalHandler`'s answer): true when it took the signal.
pub type SignalHandler =
    unsafe extern "C" fn(i32, *mut crate::generated::siginfo_t, *mut std::ffi::c_void) -> bool;

/// The engine a process runs when [`select`] made no choice: the host ISA's (identity) where the core has it, else QEMU, the one every core has.
#[cfg(atl_nb_identity)]
pub const DEFAULT: Active = Active::Identity;
/// The engine a process runs when [`select`] made no choice: the host ISA's (identity) where the core has it, else QEMU, the one every core has.
#[cfg(not(atl_nb_identity))]
pub const DEFAULT: Active = Active::Qemu(qemu::Arch::Aarch64);

/// The process's engine: [`select`]'s choice, [`DEFAULT`] when none was made (fixed from the first call on).
pub fn active() -> Active {
    *ACTIVE.get_or_init(|| DEFAULT)
}

/// Chooses the process's engine for app ISA `isa` (`None`: ART named none) and starts it ([`Engine::start`]). A second call must name the same engine: one bridge per process, one ABI per app (plan README "only one process width"), so a second foreign ISA is refused.
/// An arm app is refused here, before the claim: the arm table's rows still pending an ILP32 conversion trap (bindings `qemu_arm`, phase4-32bit P4b s2-s4) and 32-bit JNI is P5. Its engine boots (the QEMU gates do it through `qemu::boot`). Delete with P5.
pub fn select(isa: Option<&str>) -> Result<Active, String> {
    if isa.and_then(qemu::Arch::of_isa) == Some(qemu::Arch::Arm) {
        return Err(
            "ISA arm: arm rows pending ILP32 conversion (P4b s2-s4) and 32-bit JNI (P5)"
                .to_owned(),
        );
    }
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

/// The engine for ISA `isa` in a process already running `current`: none or the host ISA -> identity (refused in a core without it), a QEMU guest architecture's ISA (qemu/arch.rs) -> QEMU running it, anything else refused; a choice other than `current` is refused.
fn decide(current: Option<Active>, isa: Option<&str>) -> Result<Active, String> {
    let want = match isa {
        // deliberate break (`ATL_NB_BREAK=host_qemu`): the host ISA routed to QEMU (unit test `selection`)
        #[cfg(atl_nb_identity)]
        None | Some(identity::GUEST_ISA) if cfg!(atl_nb_host_qemu) => {
            Active::Qemu(qemu::Arch::Aarch64)
        }
        #[cfg(atl_nb_identity)]
        None | Some(identity::GUEST_ISA) => Active::Identity,
        #[cfg(not(atl_nb_identity))]
        None | Some(HOST_ISA) => {
            return Err(format!(
                "ISA {}: this core has no identity engine for its host ISA",
                isa.unwrap_or(HOST_ISA)
            ));
        }
        Some(other) => match qemu::Arch::of_isa(other) {
            Some(arch) => Active::Qemu(arch),
            None => return Err(format!("ISA {other}: no engine runs it")),
        },
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
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::GUEST_ISA,
            Active::Qemu(arch) => arch.isa(),
        }
    }

    /// The engine's guest pointer width in bits (arches.toml `pointer_bits` of the guest row): picks the guest library dir (atl-android-paths `native_bridge_lib`).
    pub fn guest_pointer_bits(self) -> u32 {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::GUEST_POINTER_BITS,
            Active::Qemu(arch) => arch.pointer_bits(),
        }
    }

    /// The ELF machine of the libraries this engine runs (elf.rs `classify`): the host's, or QEMU's guest machine.
    pub fn machine(self) -> Result<object::elf::Machine, String> {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => crate::elf::host_machine(),
            Active::Qemu(arch) => Ok(arch.machine()),
        }
    }

    /// Boots the engine ([`Engine::start`]); once per process, later calls answer the first's result.
    fn start(self) -> Result<(), String> {
        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        STARTED
            .get_or_init(|| match self {
                #[cfg(atl_nb_identity)]
                Active::Identity => identity::CROSSING.start(),
                Active::Qemu(_) => qemu::CROSSING.start(),
            })
            .clone()
    }

    /// Gives the calling thread its guest CPU ([`Engine::attach`]).
    fn attach(self, req: StackReq) -> Result<Stack, String> {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::CROSSING.attach(req),
            Active::Qemu(_) => qemu::CROSSING.attach(req),
        }
    }

    /// Takes the calling thread's guest CPU back ([`Engine::detach`]).
    fn detach(self) {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::CROSSING.detach(),
            Active::Qemu(_) => qemu::CROSSING.detach(),
        }
    }

    /// The special handler ART registers for `sig` (NativeBridge `getSignalHandler`). Identity: none, its signals are wrapped at install (signal.rs). QEMU: the engine's guest-fault route for SIGSEGV and SIGBUS.
    pub fn signal_handler(self, sig: i32) -> Option<SignalHandler> {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => None,
            Active::Qemu(_) => qemu::signal_handler(sig),
        }
    }

    /// Whether this host can run the engine's crossing.
    pub fn supported(self) -> bool {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::supported(),
            Active::Qemu(_) => qemu::supported(),
        }
    }

    /// The kernel's auxv for the guest (args.rs).
    pub fn kernel_auxv(self) -> Result<Vec<(usize, usize)>, String> {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::kernel_auxv(),
            Active::Qemu(_) => qemu::kernel_auxv(),
        }
    }

    /// Gives back [`Active::reserve`]'s `[at, at + len)` (thread records and static TLS at thread exit). Identity: munmap. QEMU: the engine's `unmap_window` (ABI 4.2, the guest's own munmap: host unmap and guest page marks cleared together). A refused release is logged and stays counted in [`RESERVED`].
    pub fn release(self, at: usize, len: usize) {
        let r = match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => {
                // SAFETY: a mapping `reserve` made, which nothing uses any more (caller contract of the record's owner).
                if unsafe { libc::munmap(at as *mut _, len) } == 0 {
                    Ok(())
                } else {
                    Err(format!(
                        "munmap {at:#x}+{len:#x}: {}",
                        std::io::Error::last_os_error()
                    ))
                }
            }
            // deliberate break (`ATL_NB_BREAK=qt_keep_windows`): windows are never given back (qemu-thread check c must go red)
            Active::Qemu(_) if cfg!(atl_nb_qt_keep_windows) => return,
            Active::Qemu(_) => qemu::CROSSING.unmap_window(at, len),
        };
        match r {
            Ok(()) => {
                RESERVED.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                let line = format!("atl-native-bridge: release {at:#x}+{len:#x} refused: {e}\n");
                let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            }
        }
    }

    /// Fresh zeroed memory of `len` bytes the guest can address, `prot` (`PROT_*`) for the guest: the one seam for every host allocation the guest dereferences (image reservations, the argument block, scratch TCBs, thread records and static TLS). Identity: anonymous host memory, guest memory is host memory. QEMU: the engine's `map_window` (below 4 GiB for a 32-bit guest). Given back only through [`Active::release`].
    pub fn reserve(self, len: usize, prot: u32) -> Result<usize, String> {
        let at = self.reserve_raw(len, prot)?;
        RESERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(at)
    }

    fn reserve_raw(self, len: usize, prot: u32) -> Result<usize, String> {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => {
                // SAFETY: a fresh anonymous mapping; nothing else refers to it.
                let at = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        prot as i32,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                if at == libc::MAP_FAILED {
                    return Err(format!(
                        "mmap {len:#x}: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                Ok(at as usize)
            }
            Active::Qemu(_) => qemu::CROSSING.map_window(len, prot),
        }
    }

    /// Makes `images` reachable by guest code.
    pub fn map_guest(self, images: &[&Image]) -> Result<(), String> {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::map_guest(images),
            Active::Qemu(_) => qemu::map_guest(images),
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
                #[cfg(atl_nb_identity)]
                Active::Identity => identity::init_tcb(tp, ht),
                Active::Qemu(_) => qemu::init_tcb(tp, ht),
            }
        }
    }

    /// A guest-callable address that crosses to host function `target` with an integer result; None when the engine's thunks are used up.
    pub fn bind(self, target: usize) -> Option<usize> {
        self.bind_result(target, objects::Sig::INT)
    }

    /// [`Active::bind`] for a host function of lanes `sig`: the QEMU engine places the arguments and answers the binding trap in x0 or v0 by it. Identity's native call passes and returns in its own registers, so it ignores `sig`.
    pub fn bind_result(self, target: usize, sig: objects::Sig) -> Option<usize> {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::bind(target),
            Active::Qemu(_) => qemu::bind_result(target, sig),
        }
    }

    /// Whether `addr` holds an unbound interceptable stub of this engine's guest ISA.
    pub fn is_stub(self, addr: usize) -> bool {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::is_stub(addr),
            Active::Qemu(_) => qemu::is_stub(addr),
        }
    }

    /// Points stub `addr` at `thunk`; false when `addr` is not a stub.
    pub fn patch_site(self, addr: usize, thunk: usize) -> bool {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::patch_site(addr, thunk),
            Active::Qemu(_) => qemu::patch_site(addr, thunk),
        }
    }

    /// Routes the linker hook at `at` to host function `host`.
    pub fn patch_hook(self, at: usize, host: usize) -> Result<(), &'static str> {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::patch_hook(at, host),
            Active::Qemu(_) => qemu::patch_hook(at, host),
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
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::patch_entry(vdso, value_at, host),
            Active::Qemu(_) => qemu::patch_entry(vdso, value_at, host),
        }
    }

    /// The guest-callable address of JNI table slot `slot`.
    pub fn object_stub(self, slot: usize) -> usize {
        match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::object_stub(slot),
            Active::Qemu(_) => qemu::object_stub(slot),
        }
    }

    /// A host-callable address that runs guest `target` (loader.rs trampolines, sles.rs callbacks): a down call of `kind` whose arguments follow `shorty` (ART's JNI signature; None for JNI_OnLoad / JNI_OnUnload). QEMU places the arguments by it (place.rs `down`); identity's stub passes the caller's registers through.
    /// Shells are never freed, so one `(target, kind, shorty)` gets one shell for the process (opensl-es plan 2.2 item 5: an app registering one callback per player would otherwise use a shell per player). Break `sles_no_dedup`: every request takes a new shell.
    pub fn down(self, target: usize, kind: Down, shorty: Option<&[u8]>) -> Result<usize, String> {
        let key = (target, kind, shorty.map(<[u8]>::to_vec));
        let mut shells = DOWN_SHELLS
            .lock()
            .map_err(|_| "down shells: poisoned".to_owned())?;
        #[cfg(not(atl_nb_sles_no_dedup))]
        if let Some(&at) = shells.get(&key) {
            return Ok(at);
        }
        let at = match self {
            #[cfg(atl_nb_identity)]
            Active::Identity => identity::down(target, kind)?,
            Active::Qemu(_) => qemu::down(target, kind, shorty)?,
        };
        #[cfg(atl_nb_sles_no_dedup)]
        let key = (key.0, key.1, Some(at.to_ne_bytes().to_vec()));
        shells.insert(key, at);
        Ok(at)
    }

    /// Down shells made so far ([`Active::down`]), for the gates: a deduplicated request makes none.
    pub fn down_shells(self) -> usize {
        DOWN_SHELLS.lock().map_or(0, |s| s.len())
    }

    /// [`Engine::call`] on this engine.
    ///
    /// # Safety
    /// As [`Engine::call`].
    unsafe fn call(self, cpu: &mut HostThread, tp: Tp, f: usize, args: &[usize; 4]) -> usize {
        // SAFETY: caller contract.
        unsafe {
            match self {
                #[cfg(atl_nb_identity)]
                Active::Identity => identity::CROSSING.call(cpu, tp, f, args),
                Active::Qemu(_) => qemu::CROSSING.call(cpu, tp, f, args),
            }
        }
    }

    /// [`Engine::end_call`] on this engine.
    ///
    /// # Safety
    /// As [`Engine::end_call`].
    pub(crate) unsafe fn end_call(
        self,
        cpu: &mut HostThread,
        value: usize,
    ) -> Result<usize, String> {
        // SAFETY: caller contract.
        unsafe {
            match self {
                #[cfg(atl_nb_identity)]
                Active::Identity => identity::CROSSING.end_call(cpu, value),
                Active::Qemu(_) => qemu::CROSSING.end_call(cpu, value),
            }
        }
    }
}

/// Host `sigaction(sig)`, then `signal::wrap(sig)` (already done where the launcher's interposer hooked the call; idempotent): `handler(sig, info, ucontext)` runs on the host thread pointer even when the signal hit guest code (spike gate g3) -- the path every host handler takes, ART's sigchain included.
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
    #[cfg(atl_nb_identity)]
    signal::wrap(sig as usize);
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
        let engine = active();
        let isa = engine.guest_isa();
        let bits = engine.guest_pointer_bits();
        let bin = PathBuf::from(atl_android_paths::native_bridge_bin(isa));
        let lib = PathBuf::from(atl_android_paths::native_bridge_lib(isa, bits));
        let (linker, exe) = atl_android_paths::native_bridge_exes(bits);
        Config {
            linker: bin.join(linker),
            vdso: lib.join(VDSO_FILE),
            exe: bin.join(exe),
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
    /// From a host function the guest called in the thread's outermost guest call (intercepts.rs `pthread_exit`): ends that guest call at once, `value` becoming its answer; host frames between are skipped, so the caller holds nothing that needs a drop. Refused (Err) when another guest call is nested inside the outermost one. QEMU: arms `atl_engine.end_call`, answers `value`, and the host function's return ends the call. Identity: never returns on success (the stack goes back to the outermost `atl_nb_call_guest`).
    ///
    /// # Safety
    /// `cpu` is the calling thread's own record, called from inside a host function the guest called.
    unsafe fn end_call(&self, cpu: &mut Self::Cpu, value: usize) -> Result<usize, String>;
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
    let tp = Tp::Guest(cpu.guest_tp());
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
    #[cfg(atl_nb_identity)]
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
            .ok_or("the guest linker has no __dl___linker_init")?;
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
        thread.set_guest_tp(tp);
        let start_thread: *mut HostThread = thread;
        thread::adopt(start_thread);
        Ok(Guest {
            linker,
            vdso,
            exe_entry,
            #[cfg(atl_nb_identity)]
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

    /// Puts the host thread pointer back on the starting thread. Only for a probe that found the identity crossing left the guest's in place (g1's break): host TLS is unreadable then, so the record comes from `self`.
    #[cfg(atl_nb_identity)]
    pub fn force_host_fs(&self) {
        // SAFETY: the starting thread's record (the probe runs on it, which has not exited); host_tp is the value the crossing read before entering the guest.
        unsafe { identity::set_fs_base((*self.start_thread).host_tp()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A core without the identity engine refuses the host ISA (and none) and still runs QEMU's.
    #[cfg(not(atl_nb_identity))]
    #[test]
    fn selection() {
        assert!(decide(None, None).is_err());
        assert!(decide(None, Some(HOST_ISA)).is_err());
        assert_eq!(decide(None, Some("arm64")), Ok(QEMU64));
        assert_eq!(decide(None, Some("arm")), Ok(QEMU32));
        assert!(decide(None, Some("mips")).is_err());
        assert_eq!(DEFAULT, QEMU64);
    }

    const QEMU64: Active = Active::Qemu(qemu::Arch::Aarch64);
    const QEMU32: Active = Active::Qemu(qemu::Arch::Arm);

    /// Each QEMU guest's facts come from its one row (qemu/arch.rs): ISA name = arches.toml row, pointer width = its `pointer_bits`, ELF machine; an arm app is refused before any claim until P4a/P4b.
    #[test]
    fn qemu_guests() {
        assert_eq!(
            (QEMU64.guest_isa(), QEMU64.guest_pointer_bits()),
            ("arm64", 64)
        );
        assert_eq!(
            (QEMU32.guest_isa(), QEMU32.guest_pointer_bits()),
            ("arm", 32)
        );
        assert_eq!(QEMU32.machine(), Ok(object::elf::EM_ARM));
        assert!(select(Some("arm")).is_err());
    }

    /// D-3: host ISA (or none) -> identity, QEMU's ISA -> QEMU, an unknown ISA and a second, different engine refused. Red under breaks `host_qemu` and `second_isa`.
    #[cfg(atl_nb_identity)]
    #[test]
    fn selection() {
        assert_eq!(decide(None, None), Ok(Active::Identity));
        assert_eq!(
            decide(None, Some(std::env::consts::ARCH)),
            Ok(Active::Identity)
        );
        assert_eq!(decide(None, Some("arm64")), Ok(QEMU64));
        assert_eq!(decide(None, Some("arm")), Ok(QEMU32));
        assert!(decide(None, Some("mips")).is_err());
        assert_eq!(decide(Some(QEMU64), Some("arm64")), Ok(QEMU64));
        assert!(decide(Some(QEMU64), Some("arm")).is_err());
        assert!(decide(Some(QEMU64), Some(std::env::consts::ARCH)).is_err());
        assert!(decide(Some(Active::Identity), Some("arm64")).is_err());
    }
}
