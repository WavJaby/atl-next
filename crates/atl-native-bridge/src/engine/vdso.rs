// SPDX-License-Identifier: MIT
//! Host side of NBS `libnative_bridge_vdso.so` (soname `linux-vdso.so.1`), the guest's door to the host: the guest linker takes it as the kernel vdso (auxv AT_SYSINFO_EHDR, bionic linker_main.cpp `add_vdso`), and the guest libc imports `native_bridge_intercept_symbol` from it.
//! Its entries are 2-3-byte `ud2` bodies a translator traps on (berberis binds them per symbol, `MakeElfSymbolTrampolineCallable`); a native engine cannot patch code that small, so each entry's `st_value` in our mapped copy is rewritten to point at a host thunk (bias + value wraps to the thunk address). The file stays unpatched.
//! `native_bridge_intercept_symbol` binds a guest libc stub (NBS `interceptable_functions.h` x86_64: `movabs $0, %r11; jmp *%r11`) by writing the thunk of its host function into the `movabs` immediate. Names without a host function stay unbound: the stub jumps to 0 if called (spike g1 binds what the linker's init path calls; full set = step 4.3).

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use super::arch;
use super::image::Image;

/// Stubs bound to a host function / left unbound, since process start.
pub static BOUND: AtomicU32 = AtomicU32::new(0);
pub static UNBOUND: AtomicU32 = AtomicU32::new(0);
/// Names left unbound, for the startup report.
pub static UNBOUND_NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The host function of guest stub `name`: the bionic provider's (provider.rs), our own (intercepts.rs, signal.rs), else glibc's own where the contract is the C one (memory, allocation, time). NBS names the allocator entries `native_bridge_<fn>` (one heap: the guest libc's malloc calls them).
fn host_fn(name: &str) -> Option<usize> {
    if let Some(p) = super::provider::host_fn(name)
        .or_else(|| super::intercepts::host_fn(name))
        .or_else(|| super::signal::host_fn(name))
    {
        return Some(p);
    }
    Some(match name {
        "memcpy" => libc::memcpy as *const () as usize,
        "memset" => libc::memset as *const () as usize,
        "memcmp" => libc::memcmp as *const () as usize,
        "native_bridge_malloc" => libc::malloc as *const () as usize,
        "native_bridge_free" => libc::free as *const () as usize,
        "native_bridge_calloc" => libc::calloc as *const () as usize,
        "native_bridge_realloc" => libc::realloc as *const () as usize,
        "native_bridge_memalign" => libc::memalign as *const () as usize,
        "native_bridge_posix_memalign" => libc::posix_memalign as *const () as usize,
        "native_bridge_aligned_alloc" => libc::aligned_alloc as *const () as usize,
        "native_bridge_malloc_usable_size" => libc::malloc_usable_size as *const () as usize,
        "clock_gettime" => libc::clock_gettime as *const () as usize,
        "clock_getres" => libc::clock_getres as *const () as usize,
        "gettimeofday" => libc::gettimeofday as *const () as usize,
        "time" => libc::time as *const () as usize,
        _ => return None,
    })
}

/// Rewrites the imm64 of the stub at `addr` to `target`; false if `addr` is not a stub.
fn patch_stub(addr: usize, target: usize) -> bool {
    // SAFETY: `addr` is a guest libc text address the guest itself passed; reading its first two bytes is in bounds of its 64-byte slot.
    let head = unsafe { std::slice::from_raw_parts(addr as *const u8, 2) };
    if head != arch::STUB_PREFIX {
        return false;
    }
    let imm = addr + 2;
    let pg = page_of(imm);
    let len = page_of(imm + 8 - 1) + page_size() - pg;
    // SAFETY: the guest libc's text pages holding the stub: RWX for the 8-byte write, then RX again.
    unsafe {
        libc::mprotect(
            pg as *mut _,
            len,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        );
        std::ptr::write_unaligned(imm as *mut usize, target);
        libc::mprotect(pg as *mut _, len, libc::PROT_READ | libc::PROT_EXEC);
    }
    true
}

fn page_size() -> usize {
    // SAFETY: no memory effects.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

fn page_of(a: usize) -> usize {
    a & !(page_size() - 1)
}

extern "C" fn intercept_symbol(addr: *mut c_void, library: *const c_char, symbol: *const c_char) {
    // SAFETY: NUL-terminated literals of the guest's stub tables (INIT_INTERCEPTABLE_STUB_FUNCTION).
    let (lib, name) = unsafe { (CStr::from_ptr(library), CStr::from_ptr(symbol)) };
    let name = name.to_string_lossy();
    let lib = lib.to_string_lossy();
    let addr = addr as usize;
    // SAFETY: as patch_stub.
    if unsafe { std::slice::from_raw_parts(addr as *const u8, 2) } != arch::STUB_PREFIX {
        // the exported entry (`jmp name_stub`) or a variable: reaches the host through the stub
        return;
    }
    // a proxy library's stub resolves by (library, name) (proxy.rs), never by the global scope; libc's by name
    let host = match super::proxy::target(&lib, &name) {
        Some(t) => t.address(),
        None => host_fn(&name),
    };
    match host.and_then(arch::bind) {
        Some(thunk) if patch_stub(addr, thunk) => {
            BOUND.fetch_add(1, Ordering::Relaxed);
        }
        _ => {
            UNBOUND.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut v) = UNBOUND_NAMES.lock() {
                v.push(format!("{lib}:{name}"));
            }
        }
    }
}

use super::unsupported;

extern "C" fn post_init() {
    unsupported(
        "guest native_bridge_post_init: the guest main executable ran (g1 never jumps to it)",
    )
}

extern "C" fn call_guest() {
    unsupported("guest native_bridge_call_guest: not supported yet")
}

/// `native_bridge_trace(format, ...)`: guest debug trace, dropped.
extern "C" fn trace() {}

/// `__native_bridge_get_host_pthread()`: the bionic thread record the guest main thread reuses (NBS linker `__libc_init_main_thread_early`: `__init_tcb(temp_tcb, host_thread)`, then `TLS_SLOT_BIONIC_TLS = host_thread->bionic_tls`).
/// Built by `thread::current` (our host is glibc, so the record is ours, not host `pthread_self()` as berberis's bionic host answers).
extern "C" fn get_host_pthread() -> usize {
    super::thread::current()
}

/// `__native_bridge_config_static_tls(const NativeBridgeStaticTlsConfig*)`: how the host creates guest static TLS, for the main thread too (NBS: "the host will replace the guest main thread's static TLS"): `thread::configure`; `Guest::start` then installs it.
pub static STATIC_TLS_CONFIGS: AtomicU32 = AtomicU32::new(0);
extern "C" fn config_static_tls(config: *const c_void) {
    STATIC_TLS_CONFIGS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the linker passes its `NativeBridgeStaticTlsConfig` (NBS __libc_init_main_thread_final).
    unsafe { super::thread::configure(config.cast()) };
}

/// x86_64 `movabs $imm64, %r11; jmp *%r11`: 13 bytes.
const JUMP_LEN: usize = 13;

/// Rewrites the linker's two NB hooks (NBS linker `init_static_tls_x86_64.cc`: naked `ud2` bodies, 16-byte aligned) to jump to host thunks, as berberis `InitializeLinker` binds them; before `seal`.
pub fn bind_linker(linker: &Image) -> Result<(), String> {
    let hooks: [(&str, usize); 2] = [
        (
            "__dl___native_bridge_get_host_pthread",
            get_host_pthread as *const () as usize,
        ),
        (
            "__dl___native_bridge_config_static_tls",
            config_static_tls as *const () as usize,
        ),
    ];
    for (name, host) in hooks {
        let at = linker
            .symbol(name)
            .ok_or_else(|| format!("linker64 lacks {name}"))?;
        // SAFETY: the hook's bytes in our still-writable mapping of the linker's text.
        let body = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, JUMP_LEN) };
        // ud2, then alignment padding (int3) up to the next 16-byte function: room for the jump
        if body[..2] != [0x0f, 0x0b] || body[2..].iter().any(|&b| b != 0xcc) {
            return Err(format!(
                "{name}: not the 16-byte ud2 hook this engine rewrites"
            ));
        }
        let thunk = arch::bind(host).ok_or("out of thunks")?;
        body[..2].copy_from_slice(&arch::STUB_PREFIX);
        body[2..10].copy_from_slice(&thunk.to_ne_bytes());
        body[10..].copy_from_slice(&[0x41, 0xff, 0xe3]);
    }
    Ok(())
}

/// Points every entry of the mapped NB vdso at a host thunk; before `seal`.
pub fn bind(vdso: &Image) -> Result<(), String> {
    for (name, value_at) in vdso.dynsym_values() {
        let host: usize = match name.as_str() {
            "native_bridge_intercept_symbol" => intercept_symbol as *const () as usize,
            "native_bridge_post_init" => post_init as *const () as usize,
            "native_bridge_call_guest" => call_guest as *const () as usize,
            "native_bridge_trace" => trace as *const () as usize,
            _ => return Err(format!("NB vdso exports {name}, which has no host side")),
        };
        let thunk = arch::bind(host).ok_or("out of thunks")?;
        // SAFETY: the st_value field of a .dynsym entry in our still-writable mapping of the vdso.
        unsafe { std::ptr::write_unaligned(value_at as *mut usize, thunk.wrapping_sub(vdso.bias)) };
    }
    Ok(())
}
