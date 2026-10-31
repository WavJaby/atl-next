// SPDX-License-Identifier: MIT
//! Loading a library in a linker namespace: the one place every namespaced load goes through --
//! `android_dlopen_ext`, and through it libvndksupport's SP-HAL loads (its `android_load_sphal_library`
//! calls `android_dlopen_ext` as AOSP's does) -- and the namespace graph libdl_android's API builds
//! (`android_create_namespace`, `android_link_namespaces`; libgraphicsenv). Our libnativeloader's
//! namespaces plug in here.
//!
//! This host has exactly one namespace: the process's glibc link map. Every exported namespace name
//! resolves to it, and a load in it is `dlopen(name, flags)`, so a namespace argument is accepted and
//! ignored. bionic returns null for a name it does not export; here every name is exported, as a load
//! in any of them lands in the same map. Creating a namespace returns it; linking two is a no-op that
//! keeps bionic's argument refusals, as every library is already visible to every load.

use core::ffi::{CStr, c_char, c_int, c_void};

/// A linker namespace; only `DEFAULT` exists.
pub struct Namespace {
    /// bionic's name for the default namespace.
    name: &'static str,
}

/// The one namespace: every load's, whether the caller names a namespace or not (not zero-sized: its
/// address is the handle C code holds).
pub static DEFAULT: Namespace = Namespace { name: "(default)" };

/// bionic `create_namespace`: a new namespace with the given name, paths, type and parent; here the one
/// namespace (its paths are glibc's search). bionic's only failure -- an `ALSO_USED_AS_ANONYMOUS`
/// namespace once the anonymous one is set -- has no meaning with one namespace.
pub fn create() -> &'static Namespace {
    &DEFAULT
}

/// Why `android_link_namespaces` fails; `Display` is bionic's `DL_ERR` text (`linker.cpp`
/// `link_namespaces`), the `dlerror` detail after `android_link_namespaces failed: `.
#[derive(Debug, PartialEq, Eq)]
pub enum LinkRefusal {
    /// `namespace_from` null.
    NullFrom,
    /// The soname list null or empty; names `from`, `to`.
    NoSonames(&'static str, &'static str),
}

impl std::fmt::Display for LinkRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            LinkRefusal::NullFrom => write!(f, "error linking namespaces: namespace_from is null."),
            LinkRefusal::NoSonames(from, to) => write!(
                f,
                "error linking namespaces \"{from}\"->\"{to}\": the list of shared libraries is empty."
            ),
        }
    }
}

/// bionic `link_namespaces`: makes `sonames` (`:`-separated) of `to` (none: the default namespace)
/// visible from `from`. With one namespace they already are; bionic's refusals stay, in its order.
pub fn link(
    from: Option<&Namespace>,
    to: Option<&Namespace>,
    sonames: Option<&[u8]>,
) -> Result<(), LinkRefusal> {
    let to = to.unwrap_or(&DEFAULT);
    let from = from.ok_or(LinkRefusal::NullFrom)?;
    match sonames {
        Some(s) if !s.is_empty() => Ok(()),
        _ => Err(LinkRefusal::NoSonames(from.name, to.name)),
    }
}

unsafe extern "C" {
    // glibc's libc (>= 2.34 holds libdl)
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
}

/// bionic `android_get_exported_namespace`: the namespace exported as `name`.
pub fn exported(_name: &[u8]) -> &'static Namespace {
    &DEFAULT
}

/// Loads `name` in `ns` with `dlopen` flags `flags`: `dlopen`'s result, its error in `dlerror`.
///
/// # Safety
/// `dlopen`'s contract: `name` null (the main program) or a NUL-terminated path.
pub unsafe fn open(name: *const c_char, flags: c_int, _ns: &Namespace) -> *mut c_void {
    // SAFETY: the caller's contract
    unsafe { dlopen(name, flags) }
}

/// Fails a load with `detail` for `dlerror`: glibc owns `dlerror` and has no call to set it, so glibc's
/// own `dlopen` refuses a call whose mode has no binding (`RTLD_LAZY` / `RTLD_NOW`) with `detail` as
/// the object name -- checked before any file is looked up. `dlerror` then reads `<detail>: invalid
/// mode for dlopen(): Invalid argument` (glibc `dl-open.c`, `dlerror.c`).
pub fn fail(detail: &CStr) {
    // SAFETY: a NUL-terminated string; mode 0 is refused before glibc reads the name as a path
    let handle = unsafe { dlopen(detail.as_ptr(), 0) };
    debug_assert!(handle.is_null(), "glibc loaded with mode 0");
}
