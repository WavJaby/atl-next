// SPDX-License-Identifier: MIT
//! The loader both executables of this package are (`main.rs` = atl-app-launcher, `bin/atl-installer.rs`): its global scope is glibc + the converter's libsigchain (DT_NEEDED ahead of libc, build.rs), nothing of AOSP: the process's runtime (`libatl_app_runtime.so`, this package's cdylib, lib.rs) is
//! `dlopen`ed `RTLD_LOCAL`, so AOSP objects resolve inside its group and host objects loaded later (the UI module's GTK closure) never bind them, nor they host ones (launcher-closure.toml).
//! The ABI is one C function per executable: `<entry>(argc, argv)` -> exit status, which ends the process with `_exit` (ART's FastExit: no static destructors while runtime daemons may still run).
//! Boundary code (`dlopen`): `unsafe` only here and in the library's `ffi`.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::Write as _;
use std::os::unix::ffi::OsStringExt as _;

#[allow(dead_code, non_upper_case_globals, non_camel_case_types)]
mod generated {
    pub mod dlfcn {
        include!(concat!(env!("OUT_DIR"), "/dlfcn.rs"));
    }
    pub mod unistd {
        include!(concat!(env!("OUT_DIR"), "/unistd.rs"));
    }
    pub mod signal {
        include!(concat!(env!("OUT_DIR"), "/signal.rs"));
    }
}
use generated::{dlfcn, signal, unistd};

// libsigchain, the loader's one non-glibc library: this crate's own native library, so it precedes std's -lc on the link line and leads DT_NEEDED (cargo gives build.rs `rustc-link-lib` lines to the library target only).
#[link(name = "sigchain", kind = "dylib")]
unsafe extern "C" {
    fn sigaction(sig: c_int, act: *const signal::sigaction, old: *mut signal::sigaction) -> c_int;
}
const _: signal::sigaction_fn = sigaction;
/// A reference from the loader's own object: std's signal calls come from rlibs after -lsigchain on the line, too late for ld.bfd's --as-needed, which would drop libsigchain (`-u` does not count; the `as-needed` link modifier is nightly-only).
#[used]
static SIGCHAIN: signal::sigaction_fn = sigaction;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
    fn _exit(status: c_int);
}
// the declarations above have the host headers' signatures
const _: dlfcn::dlopen = dlopen;
const _: dlfcn::dlsym = dlsym;
const _: dlfcn::dlerror = dlerror;
const _: unistd::_exit = _exit;

/// The runtime, by file name: found on the runtime library path (xtask installs it beside the converter's libraries, as Android's linker finds app_process's).
const RUNTIME: &CStr = c"libatl_app_runtime.so";
type Entry = unsafe extern "C" fn(c_int, *const *const c_char) -> c_int;

fn dl_error() -> String {
    // SAFETY: dlerror returns NULL or a NUL-terminated string valid until the next dl call on this thread
    let e = unsafe { dlerror() };
    if e.is_null() {
        "no dlerror".into()
    } else {
        // SAFETY: non-null, NUL-terminated (above)
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

fn run(entry: &CStr) -> Result<c_int, String> {
    // argv as the kernel gave it: an argument is a C string, so it holds no NUL
    let args: Vec<CString> = std::env::args_os()
        .map(|a| CString::new(a.into_vec()).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    let mut argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let argc = c_int::try_from(args.len()).map_err(|e| e.to_string())?;
    // SAFETY: a NUL-terminated file name and the header's flag values
    let h = unsafe { dlopen(RUNTIME.as_ptr(), dlfcn::RTLD_NOW | dlfcn::RTLD_LOCAL) };
    if h.is_null() {
        return Err(format!("{}: {}", RUNTIME.to_string_lossy(), dl_error()));
    }
    // SAFETY: a live handle and a NUL-terminated name
    let f = unsafe { dlsym(h, entry.as_ptr()) };
    if f.is_null() {
        return Err(format!("{}: {}", entry.to_string_lossy(), dl_error()));
    }
    // SAFETY: the runtime defines every entry with this signature (lib.rs ffi)
    let entry: Entry = unsafe { std::mem::transmute::<*mut c_void, Entry>(f) };
    // SAFETY: argv: argc C strings + NULL, alive (args) for the call
    Ok(unsafe { entry(argc, argv.as_ptr()) })
}

/// The executable's whole run: the runtime's `entry` with this process's argv, then `_exit` with its status; `name` prefixes the loader's own errors.
pub fn main(entry: &CStr, name: &str) -> ! {
    let status = run(entry).unwrap_or_else(|e| {
        let _ = writeln!(std::io::stderr(), "{name}: {e}");
        1
    });
    let _ = std::io::stdout().flush();
    // SAFETY: ends the process; nothing after it runs
    unsafe { _exit(status) };
    unreachable!("_exit returned")
}
