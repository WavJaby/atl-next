// SPDX-License-Identifier: MIT
//! The C ABI of `libatl_native_bridge.so`: `NativeBridgeItf` and the functions behind its entries.
//! The entry shells are generated from native_bridge.h (build.rs, atl-bridgegen `Table`): each has the header's exact signature and calls the function of the same name below inside `catch_unwind`. The functions below turn raw pointers into safe types at their first line (thin-shell pattern, AGENTS.md); their signatures are checked by the compiler at the shells' calls.
//! `isSupported` classifies, `loadLibrary` / `unloadLibrary` go to the process's guest (loader.rs); the namespace API (v3+) is unused by our libnativeloader (`ART_TARGET_LINUX`: path-only calls, bridge-1 plan section 2) and answers failure.
//! Names are the header's field names, hence the lint allowance.
#![allow(non_snake_case)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, OsStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use crate::elf::{self, Verdict};
use crate::engine::identity::Down;
use crate::engine::{self, Config};
use crate::generated::{
    NativeBridgeCallbacks, NativeBridgeRuntimeCallbacks, NativeBridgeRuntimeValues,
    kJNICallTypeCriticalNative, kJNICallTypeRegular, native_bridge_namespace_t, siginfo_t,
};
use crate::loader;
use object::elf as obj_elf;

/// The interface version we implement: the pinned header's last entry, `isNativeBridgeFunctionPointer`, is v8's (libnativebridge `IDENTIFY_NATIVELY_BRIDGED_FUNCTION_POINTERS_VERSION`). A header with more entries fails to build here first (a shell without a function below).
pub const VERSION: u32 = 8;

/// What libnativebridge `dlsym`s (`kNativeBridgeInterfaceSymbol`).
#[unsafe(no_mangle)]
pub static NativeBridgeItf: NativeBridgeCallbacks = NativeBridgeCallbacks::shells();

thread_local! {
    /// The last failure on this thread: libnativeloader asks `getError` right after the failing call, on the same thread.
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn fail(msg: String) {
    // an interior NUL cannot come from our text; a path cannot contain one
    let c = CString::new(msg).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = c);
}

/// A refused `initialize`, said where it can be seen: libnativebridge only dlcloses the bridge (no getError call, no log), so the app silently runs without one.
#[allow(
    clippy::print_stderr,
    reason = "the bridge has no logger; the launcher's stderr is the app process's log"
)]
fn refused(msg: &str) {
    eprintln!("{msg}");
}

/// The ELF machine of the process engine's libraries, read once.
fn engine_machine() -> Option<obj_elf::Machine> {
    static MACHINE: OnceLock<Option<obj_elf::Machine>> = OnceLock::new();
    *MACHINE.get_or_init(|| engine::active().machine().ok())
}

/// # Safety
/// `p` is null or a NUL-terminated string valid for the call.
unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a CStr> {
    // SAFETY: non-null and NUL-terminated per the caller's contract.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) })
}

/// # Safety
/// `instruction_set` is null or a NUL-terminated string (native_bridge.h).
pub unsafe fn initialize(
    _runtime_cbs: *const NativeBridgeRuntimeCallbacks,
    _private_dir: *const c_char,
    instruction_set: *const c_char,
) -> bool {
    // SAFETY: the header's contract for `instruction_set`.
    let isa = unsafe { str_arg(instruction_set) };
    let isa = match isa.map(CStr::to_str).transpose() {
        Ok(isa) => isa,
        Err(_) => {
            fail("initialize: instruction set is not UTF-8".into());
            return false;
        }
    };
    // the app's ISA picks the process's one engine (QEMU bridge plan 2.3): host ISA -> identity, a foreign ISA -> the engine `.so`
    if let Err(e) = engine::select(isa) {
        let msg = format!("atl-native-bridge: initialize: {e}");
        refused(&msg);
        fail(msg);
        return false;
    }
    // ART calls this after FaultManager::Init: its claimed signals get the host-FS entry before any guest code runs
    engine::signal::rewrap_all();
    true
}

/// # Safety
/// `libpath` is null or a NUL-terminated string (native_bridge.h).
pub unsafe fn loadLibrary(libpath: *const c_char, _flag: i32) -> *mut c_void {
    // SAFETY: the header's contract for `libpath`.
    let Some(path) = (unsafe { str_arg(libpath) }) else {
        fail("loadLibrary: null path".into());
        return std::ptr::null_mut();
    };
    let path = Path::new(OsStr::from_bytes(path.to_bytes()));
    // a rewrap point (engine/signal.rs): host handlers installed since the last one
    crate::engine::signal::rewrap_all();
    match loader::load(path, &Config::installed) {
        Ok(h) => h as *mut c_void,
        Err(e) => {
            fail(format!("atl-native-bridge: {e}"));
            std::ptr::null_mut()
        }
    }
}

/// Pre-v7 entry, not called by ART 16: the Regular call type (as berberis forwards it).
///
/// # Safety
/// `name` is null or a NUL-terminated string (native_bridge.h).
pub unsafe fn getTrampoline(
    handle: *mut c_void,
    name: *const c_char,
    shorty: *const c_char,
    len: u32,
) -> *mut c_void {
    // SAFETY: same contract.
    unsafe { getTrampolineWithJNICallType(handle, name, shorty, len, kJNICallTypeRegular) }
}

/// What a JNI function's first argument is: ART calls `JNI_OnLoad` and `JNI_OnUnload` with a JavaVM (java_vm_ext.cc, through the trampoline lookup with no shorty); `@CriticalNative` methods get no env; every other method a JNIEnv.
fn down_kind(name: Option<&CStr>, jni_call_type: u32) -> Down {
    if name == Some(c"JNI_OnLoad") || name == Some(c"JNI_OnUnload") {
        Down::Vm
    } else if jni_call_type == kJNICallTypeCriticalNative {
        Down::Plain
    } else {
        Down::Env
    }
}

/// The JNI shorty of a method as ART passes it, or None. ART's `JNI_OnLoad` and `JNI_OnUnload` lookups pass NULL; the lookup of a method passes its shorty with `len` 0, so the string is read to its NUL.
///
/// # Safety
/// `shorty` is null or a NUL-terminated string (native_bridge.h).
unsafe fn shorty_arg(shorty: *const c_char) -> Option<Vec<u8>> {
    // SAFETY: the header's contract.
    (!shorty.is_null()).then(|| unsafe { CStr::from_ptr(shorty) }.to_bytes().to_vec())
}

/// The trampoline of guest function `f`, or null with the reason for `getError`.
fn trampoline(f: usize, kind: Down, shorty: Option<&[u8]>) -> *mut c_void {
    loader::trampoline(f, kind, shorty)
        .map_err(fail)
        .map_or(std::ptr::null_mut(), |t| t as *mut c_void)
}

/// # Safety
/// `libpath` is null or a NUL-terminated string (native_bridge.h).
pub unsafe fn isSupported(libpath: *const c_char) -> bool {
    // SAFETY: the header's contract for `libpath`.
    let Some(path) = (unsafe { str_arg(libpath) }) else {
        return false;
    };
    let Some(machine) = engine_machine() else {
        return false;
    };
    let Ok(path) = path.to_str() else {
        return false;
    };
    std::fs::read(path).is_ok_and(|data| elf::classify(&data, machine) == Verdict::Bridged)
}

pub unsafe fn getAppEnv(_instruction_set: *const c_char) -> *const NativeBridgeRuntimeValues {
    std::ptr::null()
}

pub unsafe fn isCompatibleWith(bridge_version: u32) -> bool {
    bridge_version <= VERSION
}

/// ART asks for every signal after `initialize` (native_bridge_art_interface.cc `InitializeNativeBridge`, interface >= 2) and registers each answer as a sigchain special with a full mask and `SIGCHAIN_ALLOW_NORETURN`.
pub unsafe fn getSignalHandler(
    signal: i32,
) -> Option<unsafe extern "C" fn(i32, *mut siginfo_t, *mut c_void) -> bool> {
    engine::active().signal_handler(signal)
}

/// Guest `dlclose`; 0 or -1 with the text for `getError`.
pub unsafe fn unloadLibrary(handle: *mut c_void) -> i32 {
    match loader::unload(handle as usize) {
        Ok(()) => 0,
        Err(e) => {
            fail(format!("atl-native-bridge: {e}"));
            -1
        }
    }
}

/// The thread's last failure text; empty when none (libnativeloader `strdup`s it, so never null).
pub unsafe fn getError() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

pub unsafe fn isPathSupported(_library_path: *const c_char) -> bool {
    false
}

pub unsafe fn unused_initAnonymousNamespace(_a: *const c_char, _b: *const c_char) -> bool {
    false
}

pub unsafe fn createNamespace(
    _name: *const c_char,
    _ld_library_path: *const c_char,
    _default_library_path: *const c_char,
    _type: u64,
    _permitted_when_isolated_path: *const c_char,
    _parent_ns: *mut native_bridge_namespace_t,
) -> *mut native_bridge_namespace_t {
    std::ptr::null_mut()
}

pub unsafe fn linkNamespaces(
    _from: *mut native_bridge_namespace_t,
    _to: *mut native_bridge_namespace_t,
    _shared_libs_sonames: *const c_char,
) -> bool {
    false
}

pub unsafe fn loadLibraryExt(
    libpath: *const c_char,
    flag: i32,
    _ns: *mut native_bridge_namespace_t,
) -> *mut c_void {
    // SAFETY: same contract as `loadLibrary`.
    unsafe { loadLibrary(libpath, flag) }
}

pub unsafe fn getVendorNamespace() -> *mut native_bridge_namespace_t {
    std::ptr::null_mut()
}

pub unsafe fn getExportedNamespace(_name: *const c_char) -> *mut native_bridge_namespace_t {
    std::ptr::null_mut()
}

pub unsafe fn preZygoteFork() {}

/// Guest `dlsym` -> a down trampoline whose crossing hands the guest its own env/vm (engine/jni.rs) and places the arguments by the shorty (QEMU: dispatch.rs `down_params`; identity passes them through).
///
/// # Safety
/// `name` is null or a NUL-terminated string, `shorty` null or NUL-terminated (native_bridge.h).
pub unsafe fn getTrampolineWithJNICallType(
    handle: *mut c_void,
    name: *const c_char,
    shorty: *const c_char,
    _len: u32,
    jni_call_type: u32,
) -> *mut c_void {
    // SAFETY: the header's contract.
    let Some(name) = (unsafe { str_arg(name) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: the header's contract.
    let shorty = unsafe { shorty_arg(shorty) };
    loader::symbol(handle as usize, name).map_or(std::ptr::null_mut(), |f| {
        trampoline(f, down_kind(Some(name), jni_call_type), shorty.as_deref())
    })
}

/// `RegisterNatives` with a guest pointer (jni_internal.cc, after `isNativeBridgeFunctionPointer`): the same trampoline as a looked-up method.
///
/// # Safety
/// `shorty` is null or NUL-terminated (native_bridge.h).
pub unsafe fn getTrampolineForFunctionPointer(
    method: *const c_void,
    shorty: *const c_char,
    _len: u32,
    jni_call_type: u32,
) -> *mut c_void {
    // SAFETY: the header's contract.
    let shorty = unsafe { shorty_arg(shorty) };
    trampoline(
        method as usize,
        down_kind(None, jni_call_type),
        shorty.as_deref(),
    )
}

/// Whether `method` is guest code: inside an object the guest linker loaded.
pub unsafe fn isNativeBridgeFunctionPointer(method: *const c_void) -> bool {
    // deliberate break of gate 4.4: ART then registers the guest pointer as host code
    if cfg!(atl_nb_nbfp_false) {
        return false;
    }
    loader::is_guest_code(method as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_routes_without_a_guest() {
        let itf = &NativeBridgeItf;
        assert_eq!(itf.version, VERSION);
        let path = c"/data/app/lib/x86_64/libatl-jni-test.so";
        // SAFETY: entries called with valid NUL-terminated strings, as libnativeloader does.
        unsafe {
            assert!(itf.isCompatibleWith.unwrap()(3));
            assert!(!itf.isCompatibleWith.unwrap()(VERSION + 1));
            // identity takes the kernel-level rewrap, no sigchain special
            assert!(itf.getSignalHandler.unwrap()(libc::SIGSEGV).is_none());
            // no guest installed on a build host: the start fails and says why
            assert!(itf.loadLibrary.unwrap()(path.as_ptr(), 2).is_null());
            let err = CStr::from_ptr(itf.getError.unwrap()()).to_str().unwrap();
            assert!(err.contains("guest start failed"), "{err}");
            // a glibc object (this test binary) is not ours
            assert!(!itf.isSupported.unwrap()(c"/proc/self/exe".as_ptr()));
            assert!(!itf.isSupported.unwrap()(std::ptr::null()));
        }
    }
}
