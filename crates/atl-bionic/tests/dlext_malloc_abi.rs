// SPDX-License-Identifier: MIT
//! `android_dlopen_ext`, `android_get_exported_namespace` and `android_mallopt` as C calls them. Expected
//! values: bionic's sources (dlext.h, malloc.h, linker.cpp, dlfcn.cpp, malloc_heapprofd.cpp) and glibc's
//! (`dl-open.c`, `dlerror.c`), never this crate's.

use std::ffi::{CStr, c_char, c_int, c_void};

// the exports live in the crate; nothing else of it is named here
use atl_bionic as _;

#[repr(C)]
struct Namespace {
    _opaque: [u8; 0],
}

/// bionic's android/dlext.h `android_dlextinfo`.
#[repr(C)]
struct DlextInfo {
    flags: u64,
    reserved_addr: *mut c_void,
    reserved_size: usize,
    relro_fd: c_int,
    library_fd: c_int,
    library_fd_offset: i64,
    library_namespace: *mut Namespace,
}

unsafe extern "C" {
    fn android_dlopen_ext(name: *const c_char, flags: c_int, info: *const DlextInfo)
    -> *mut c_void;
    fn android_get_exported_namespace(name: *const c_char) -> *mut Namespace;
    fn android_mallopt(opcode: c_int, arg: *mut c_void, arg_size: usize) -> bool;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlerror() -> *const c_char;
}

/// glibc's `RTLD_NOW`, the same on every glibc arch (bits/dlfcn.h).
const RTLD_NOW: c_int = 2;
/// bionic dlext.h bits.
const RESERVED_ADDRESS: u64 = 0x1;
const USE_LIBRARY_FD_OFFSET: u64 = 0x20;
const USE_NAMESPACE: u64 = 0x200;
/// The first bit past bionic's last flag (`ANDROID_DLEXT_RESERVED_ADDRESS_RECURSIVE` = 0x400).
const INVALID: u64 = 0x800;

fn info(flags: u64, ns: *mut Namespace) -> DlextInfo {
    DlextInfo {
        flags,
        reserved_addr: std::ptr::null_mut(),
        reserved_size: 0,
        relro_fd: -1,
        library_fd: -1,
        library_fd_offset: 0,
        library_namespace: ns,
    }
}

/// `dlerror()`, which also clears it.
fn take_dlerror() -> Option<String> {
    // SAFETY: dlerror returns null or a NUL-terminated string valid until the next dl call
    unsafe {
        let p = dlerror();
        (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
    }
}

#[test]
fn every_exported_name_is_the_one_namespace() {
    // SAFETY: NUL-terminated names, or null
    unsafe {
        let sphal = android_get_exported_namespace(c"sphal".as_ptr());
        assert!(!sphal.is_null());
        assert_eq!(
            android_get_exported_namespace(c"com_android_media".as_ptr()),
            sphal
        );
        // bionic get_exported_namespace: a null name is no namespace
        assert!(android_get_exported_namespace(std::ptr::null()).is_null());
    }
}

#[test]
fn loads_in_the_namespace_or_plain() {
    // SAFETY: NUL-terminated names, dlopen flags, an android_dlextinfo or null, handles from the loads
    unsafe {
        let ns = android_get_exported_namespace(c"sphal".as_ptr());
        let i = info(USE_NAMESPACE, ns);
        // glibc's own libc is always loadable
        let handle = android_dlopen_ext(c"libc.so.6".as_ptr(), RTLD_NOW, &i);
        assert!(!handle.is_null());
        assert_eq!(dlclose(handle), 0);
        let plain = android_dlopen_ext(c"libc.so.6".as_ptr(), RTLD_NOW, std::ptr::null());
        assert!(!plain.is_null());
        assert_eq!(dlclose(plain), 0);
        // what dlopen cannot find, it reports as null with its own error
        take_dlerror();
        let none = android_dlopen_ext(c"libatl-no-such-library.so".as_ptr(), RTLD_NOW, &i);
        assert!(none.is_null());
        assert!(take_dlerror().is_some());
    }
}

#[test]
fn refusals_carry_bionic_text() {
    // dlerror = bionic's "dlopen failed: <linker detail>" (dlfcn.cpp, linker.cpp do_dlopen), as the
    // object name of glibc's refusal of a mode without binding (dl-open.c), then strerror(EINVAL)
    let glibc = ": invalid mode for dlopen(): Invalid argument";
    let cases = [
        (
            INVALID | USE_NAMESPACE,
            true,
            "dlopen failed: invalid extended flags to android_dlopen_ext: 0xa00",
        ),
        (
            USE_LIBRARY_FD_OFFSET,
            true,
            "dlopen failed: invalid extended flag combination (ANDROID_DLEXT_USE_LIBRARY_FD_OFFSET without ANDROID_DLEXT_USE_LIBRARY_FD): 0x20",
        ),
        (
            USE_NAMESPACE,
            false,
            "dlopen failed: ANDROID_DLEXT_USE_NAMESPACE is set but extinfo->library_namespace is null",
        ),
        // valid in bionic, not taken here: our text in bionic's form
        (
            RESERVED_ADDRESS,
            true,
            "dlopen failed: unsupported extended flags to android_dlopen_ext: 0x1 (only ANDROID_DLEXT_USE_NAMESPACE is)",
        ),
    ];
    // SAFETY: NUL-terminated names, dlopen flags, an android_dlextinfo
    unsafe {
        let ns = android_get_exported_namespace(c"sphal".as_ptr());
        for (flags, with_ns, detail) in cases {
            let i = info(flags, if with_ns { ns } else { std::ptr::null_mut() });
            take_dlerror();
            let handle = android_dlopen_ext(c"libc.so.6".as_ptr(), RTLD_NOW, &i);
            assert!(handle.is_null(), "{flags:#x} loaded");
            assert_eq!(
                take_dlerror().as_deref(),
                Some(&*format!("{detail}{glibc}"))
            );
            // read once, as bionic's
            assert_eq!(take_dlerror(), None);
        }
    }
}

#[test]
fn mallopt_refuses_every_option() {
    // bionic malloc.h opcodes 1..=12 (8 unassigned) plus ones it never assigned; glibc ENOTSUP is
    // EOPNOTSUPP, 95 in Linux asm-generic/errno.h (the same on every tier-1 arch)
    const ENOTSUP: i32 = 95;
    let mut limit: usize = 64 << 20;
    for opcode in (0..=13).chain([-1, 1000]) {
        // SAFETY: a valid pointer and its size, as M_SET_ALLOCATION_LIMIT_BYTES takes
        let ok = unsafe {
            android_mallopt(
                opcode,
                std::ptr::from_mut(&mut limit).cast(),
                size_of::<usize>(),
            )
        };
        assert!(!ok, "opcode {opcode}");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(ENOTSUP),
            "opcode {opcode}"
        );
    }
}
