// SPDX-License-Identifier: MIT
//! Library load, QEMU's `main`, and guest calls on the CPU state P1's hook hands over.

use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::OnceLock;

use crate::layout::CPUArchState as Env;

/// What reached the host from the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// The bootstrap trap: the guest's x0. The answer becomes the guest's x0 (its exit status in the spike fixture).
    Boot(u64),
    /// A host call: the guest's x0..x7. The answer becomes the guest's x0.
    Call([u64; 8]),
    /// A value reported for a gate: the guest's x0. The answer is ignored.
    Report(u64),
}

/// The host side of every trap. Runs on the guest thread inside QEMU's `cpu_loop`; may call [`Guest::call`].
pub type OnTrap = fn(Guest, Trap) -> u64;

type MainFn = unsafe extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char) -> c_int;
type CpuLoopFn = unsafe extern "C" fn(*mut c_void);
pub(crate) type InCodeGenBufferFn = unsafe extern "C" fn(*const c_void) -> bool;

/// The library's entry points and data, resolved once by [`run`].
pub(crate) struct Lib {
    cpu_loop: CpuLoopFn,
    pub(crate) in_code_gen_buffer: InCodeGenBufferFn,
    /// `tcg_splitwx_diff` (tcg.h): rw minus rx address of the code buffer; 0 without split w^x.
    pub(crate) splitwx_diff: &'static isize,
    /// `guest_base` (QEMU `main.c`, set by `-B`): host = guest + guest_base.
    guest_base: &'static usize,
    /// dlopen handle, for the per-thread symbols resolved later (`helper_retaddr`).
    handle: usize,
    pub(crate) on_trap: OnTrap,
}

static LIB: OnceLock<Lib> = OnceLock::new();

pub(crate) fn lib() -> &'static Lib {
    LIB.get().expect("atl-qemu-engine: run() not called")
}

thread_local! {
    /// The guest's `svc #RET` stub, from its BOOT trap (x1): guest calls return there.
    static RET_STUB: Cell<u64> = const { Cell::new(0) };
}

/// dlsym of `name` in `handle`, or the dlerror text.
pub(crate) fn sym(handle: usize, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: handle came from dlopen and is never closed; name is NUL-terminated.
    let p = unsafe { libc::dlsym(handle as *mut c_void, name.as_ptr()) };
    if p.is_null() {
        Err(format!("{}: {}", name.to_string_lossy(), dlerror()))
    } else {
        Ok(p)
    }
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

/// Loads `library` and runs QEMU's `main` with `argv` (argv[0] first) and this process's environment. Returns `main`'s status if it returns; the guest normally exits the process itself.
/// Call once per process; the calling thread becomes the guest thread.
pub fn run(library: &Path, argv: &[&str], on_trap: OnTrap) -> Result<i32, String> {
    let path = CString::new(library.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // RTLD_LOCAL: P1's call to `atl_svc_hook` still binds to the executable's definition, which comes first in global lookup order
    // SAFETY: path is NUL-terminated; the library's initialisers are QEMU's own (QOM type registration).
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("dlopen {}: {}", library.display(), dlerror()));
    }
    let handle = handle as usize;
    let main = sym(handle, c"atl_qemu_user_main")?;
    let cpu_loop = sym(handle, c"cpu_loop")?;
    let in_cgb = sym(handle, c"in_code_gen_buffer")?;
    let splitwx = sym(handle, c"tcg_splitwx_diff")?;
    let guest_base = sym(handle, c"guest_base")?;
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
        handle,
        on_trap,
    };
    LIB.set(lib)
        .map_err(|_| "atl-qemu-engine: run() called twice".to_owned())?;
    crate::signal::capture_previous();
    let args: Vec<CString> = argv
        .iter()
        .map(|a| CString::new(*a).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
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

/// The guest CPU as P1's hook received it (`CPUArchState *`), valid while the hook runs.
#[derive(Clone, Copy, Debug)]
pub struct Guest {
    env: *mut u8,
}

impl Guest {
    pub(crate) fn new(env: *mut c_void) -> Self {
        Self { env: env.cast() }
    }

    pub(crate) fn set_ret_stub(addr: u64) {
        RET_STUB.with(|s| s.set(addr));
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

    pub fn pc(&self) -> u64 {
        self.reg(Env::pc)
    }

    /// The host address of guest address `addr` (`g2h`: plus `guest_base`; identity at `-B 0`).
    pub fn host_addr(&self, addr: u64) -> usize {
        (addr as usize).wrapping_add(*lib().guest_base)
    }

    /// Calls guest function `f` with `args` in x0.. and returns its x0. Nested: may run from inside a trap callback, which may itself call again.
    /// The whole CPU state is saved and restored around the call, so the interrupted guest code resumes exactly where its trap left it.
    pub fn call(&self, f: u64, args: &[u64]) -> u64 {
        assert!(args.len() <= 8, "{} arguments, x0..x7 only", args.len());
        let ret = RET_STUB.with(Cell::get);
        assert!(ret != 0, "no BOOT trap yet: no return stub");
        let mut saved = vec![0u8; Env::SIZE];
        // SAFETY: env is a live CPUArchState of Env::SIZE bytes (clang layout); saved is that long.
        unsafe { std::ptr::copy_nonoverlapping(self.env, saved.as_mut_ptr(), Env::SIZE) };
        for (i, a) in args.iter().enumerate() {
            self.set_x(i, *a);
        }
        // AAPCS64: sp 16-aligned; below the interrupted frame (no red zone on aarch64, the margin is for a stray store)
        let sp = (self.x(31) - 256) & !15;
        self.set_x(31, sp);
        self.set_x(30, ret);
        self.set_reg(Env::pc, f);
        // SAFETY: cpu_loop on this thread's CPU; P1 makes it return when the guest reaches `svc #RET` (trap.rs).
        unsafe { (lib().cpu_loop)(self.env.cast()) };
        let r = self.x(0);
        // SAFETY: as for the save.
        unsafe { std::ptr::copy_nonoverlapping(saved.as_ptr(), self.env, Env::SIZE) };
        r
    }
}
