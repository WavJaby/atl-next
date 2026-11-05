// SPDX-License-Identifier: MIT
//! The C ABI of `libatl_native_bridge.so`: `NativeBridgeItf` and the functions behind its entries.
//! The entry shells are generated from native_bridge.h (build.rs, atl-bridgegen `Table`): each has the header's exact signature and calls the function of the same name below inside `catch_unwind`. The functions below turn raw pointers into safe types at their first line (thin-shell pattern, AGENTS.md); their signatures are checked by the compiler at the shells' calls.
//! Step 2 (routing only): `isSupported` classifies, `loadLibrary` refuses with `LOAD_REFUSED` + the path; the namespace API (v3+) is unused by our libnativeloader (`ART_TARGET_LINUX`: path-only calls, bridge-1 plan section 2) and answers failure.
//! Names are the header's field names, hence the lint allowance.
#![allow(non_snake_case)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::OnceLock;

use crate::elf::{self, Verdict};
use crate::generated::{
    NativeBridgeCallbacks, NativeBridgeRuntimeCallbacks, NativeBridgeRuntimeValues,
    native_bridge_namespace_t, siginfo_t,
};
use object::elf as obj_elf;

/// The interface version we implement: the pinned header's last entry, `isNativeBridgeFunctionPointer`, is v8's (libnativebridge `IDENTIFY_NATIVELY_BRIDGED_FUNCTION_POINTERS_VERSION`). A header with more entries fails to build here first (a shell without a function below).
pub const VERSION: u32 = 8;

/// The text of `loadLibrary`'s refusal; ART's `UnsatisfiedLinkError` carries it (`NativeBridgeGetError`).
pub const LOAD_REFUSED: &str = "atl-native-bridge: loading bionic libraries is not implemented yet";

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

/// The host's ELF machine, read once.
fn host_machine() -> Option<obj_elf::Machine> {
    static HOST: OnceLock<Option<obj_elf::Machine>> = OnceLock::new();
    *HOST.get_or_init(|| elf::host_machine().ok())
}

/// # Safety
/// `p` is null or a NUL-terminated string valid for the call.
unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a CStr> {
    // SAFETY: non-null and NUL-terminated per the caller's contract.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) })
}

pub unsafe fn initialize(
    _runtime_cbs: *const NativeBridgeRuntimeCallbacks,
    _private_dir: *const c_char,
    _instruction_set: *const c_char,
) -> bool {
    true
}

/// # Safety
/// `libpath` is null or a NUL-terminated string (native_bridge.h).
pub unsafe fn loadLibrary(libpath: *const c_char, _flag: i32) -> *mut c_void {
    // SAFETY: the header's contract for `libpath`.
    let path = unsafe { str_arg(libpath) };
    fail(format!(
        "{LOAD_REFUSED}: {}",
        path.map_or("(null)".into(), CStr::to_string_lossy)
    ));
    std::ptr::null_mut()
}

pub unsafe fn getTrampoline(
    _handle: *mut c_void,
    _name: *const c_char,
    _shorty: *const c_char,
    _len: u32,
) -> *mut c_void {
    std::ptr::null_mut()
}

/// # Safety
/// `libpath` is null or a NUL-terminated string (native_bridge.h).
pub unsafe fn isSupported(libpath: *const c_char) -> bool {
    // SAFETY: the header's contract for `libpath`.
    let Some(path) = (unsafe { str_arg(libpath) }) else {
        return false;
    };
    let Some(host) = host_machine() else {
        return false;
    };
    let Ok(path) = path.to_str() else {
        return false;
    };
    std::fs::read(path).is_ok_and(|data| elf::classify(&data, host) == Verdict::Bridged)
}

pub unsafe fn getAppEnv(_instruction_set: *const c_char) -> *const NativeBridgeRuntimeValues {
    std::ptr::null()
}

pub unsafe fn isCompatibleWith(bridge_version: u32) -> bool {
    bridge_version <= VERSION
}

pub unsafe fn getSignalHandler(
    _signal: i32,
) -> Option<unsafe extern "C" fn(i32, *mut siginfo_t, *mut c_void) -> bool> {
    None
}

/// Nothing is ever loaded, so no handle is ours.
pub unsafe fn unloadLibrary(_handle: *mut c_void) -> i32 {
    -1
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

pub unsafe fn getTrampolineWithJNICallType(
    _handle: *mut c_void,
    _name: *const c_char,
    _shorty: *const c_char,
    _len: u32,
    _jni_call_type: u32,
) -> *mut c_void {
    std::ptr::null_mut()
}

pub unsafe fn getTrampolineForFunctionPointer(
    _method: *const c_void,
    _shorty: *const c_char,
    _len: u32,
    _jni_call_type: u32,
) -> *mut c_void {
    std::ptr::null_mut()
}

/// Nothing is loaded, so no pointer is a bridged function.
pub unsafe fn isNativeBridgeFunctionPointer(_method: *const c_void) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_routes_and_refuses() {
        let itf = &NativeBridgeItf;
        assert_eq!(itf.version, VERSION);
        let path = c"/data/app/lib/x86_64/libatl-jni-test.so";
        // SAFETY: entries called with valid NUL-terminated strings, as libnativeloader does.
        unsafe {
            assert!(itf.isCompatibleWith.unwrap()(3));
            assert!(!itf.isCompatibleWith.unwrap()(VERSION + 1));
            assert!(itf.loadLibrary.unwrap()(path.as_ptr(), 2).is_null());
            let err = CStr::from_ptr(itf.getError.unwrap()()).to_str().unwrap();
            assert_eq!(err, format!("{LOAD_REFUSED}: {}", path.to_str().unwrap()));
            // a glibc object (this test binary) is not ours
            assert!(!itf.isSupported.unwrap()(c"/proc/self/exe".as_ptr()));
            assert!(!itf.isSupported.unwrap()(std::ptr::null()));
        }
    }
}
