// SPDX-License-Identifier: MIT
//! ES1 extension entries (gl-honesty O2). libEGL fills its ES1 table from `libGLESv1_CM_atl.so` (ANGLE), and every name that library does not export -- most ES1 extensions -- from this driver's eglGetProcAddress, the same call that fills the ES2 table (Loader.cpp `init_api`). One address thus serves both tables.
//! For every name of AOSP's `entries_gles1.in` that ES1's `GLES/glext.h` declares, eglGetProcAddress hands out a generated shell (build.rs, atl-bridgegen `selector_module`) that picks per call: an ES1 context current on the thread -> ANGLE's entry, else the ES2/3 answer (fbo0 wrapper or host).
//! A name ANGLE has no entry for is counted once and logged (`es1 entry missing`); its calls answer 0 of the return type, as AOSP's `gl_unimplemented`.

use std::ffi::{CString, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::es1;

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
mod shells {
    include!(concat!(env!("OUT_DIR"), "/es1_ext.rs"));
}

/// The ES2/3 answer per shell: what eglGetProcAddress would have handed out without the shell (null: none).
static HOST: [AtomicPtr<c_void>; shells::COUNT] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; shells::COUNT];
/// ANGLE's entry per shell, resolved at its first ES1 call (0: none).
static ES1: [OnceLock<usize>; shells::COUNT] = [const { OnceLock::new() }; shells::COUNT];
/// Names with no ES1 entry: the counter of the missing-entry line.
static MISSING: AtomicU64 = AtomicU64::new(0);

#[allow(
    clippy::print_stderr,
    reason = "one line per name ANGLE lacks: the app's log names the entry its ES1 calls skip"
)]
fn log(line: &str) {
    eprintln!("libEGL_atl: {line}");
}

/// The shell for `name` with `host` (the ES2/3 answer, null for none) recorded behind it; `None` for a name that is no ES1 extension entry.
pub fn shell(name: &[u8], host: *const ()) -> Option<*const ()> {
    let (i, addr) = shells::lookup(name)?;
    HOST[i].store(host.cast_mut().cast(), Ordering::Release);
    Some(addr)
}

/// The implementation a shell calls: `es1_current` picks ANGLE's (resolved once by `resolve`), else the ES2/3 answer.
fn select(i: usize, es1_current: bool, resolve: impl FnOnce() -> *mut c_void) -> *mut c_void {
    if !es1_current {
        return HOST[i].load(Ordering::Acquire);
    }
    *ES1[i].get_or_init(|| {
        let p = resolve();
        if p.is_null() {
            let n = MISSING.fetch_add(1, Ordering::Relaxed) + 1;
            log(&format!("es1 entry missing {} count={n}", shells::NAMES[i]));
        }
        p as usize
    }) as *mut c_void
}

/// The selector the generated shells call (build.rs).
///
/// # Safety
/// `F` is the fn-pointer type of shell `i`'s C prototype.
pub unsafe fn pick<F: Copy>(i: usize) -> Option<F> {
    let p = select(i, es1::is_current(), || {
        CString::new(shells::NAMES[i]).map_or(std::ptr::null_mut(), |c| es1::entry(&c))
    });
    // SAFETY: per the caller, `F` is the pointer-sized fn type of this entry; a non-null address of that entry.
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, F>(&p) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(name: &str) -> usize {
        shells::lookup(name.as_bytes()).expect(name).0
    }

    #[test]
    fn bounded_by_es1_extensions() {
        // in entries_gles1.in and ES1 glext.h: extensions whose ES2 counterpart shares the name (gl-honesty O2 examples), and an ES1-only one
        for name in [
            "glBindVertexArrayOES",
            "glEGLImageTargetTexture2DOES",
            "glDiscardFramebufferEXT",
            "glDrawTexiOES",
        ] {
            assert!(shells::lookup(name.as_bytes()).is_some(), "{name}");
        }
        // core ES 1.1 (GLES/gl.h): ANGLE's ES1 library exports it, the ES1 table never asks the driver
        for name in ["glClear", "glDrawArrays", "glActiveTexture"] {
            assert!(shells::lookup(name.as_bytes()).is_none(), "{name}");
        }
        // ES2/3-only names are in no ES1 table
        assert!(shells::lookup(b"glCreateShader").is_none());
    }

    #[test]
    fn host_unless_es1_current() {
        let i = index("glBindVertexArrayOES");
        let (host, angle) = (0x1000usize as *mut c_void, 0x2000usize as *mut c_void);
        HOST[i].store(host, Ordering::Release);
        assert_eq!(select(i, false, || angle), host);
        assert_eq!(select(i, true, || angle), angle);
        // resolved once: a later resolution is not asked
        assert_eq!(select(i, true, || std::ptr::null_mut()), angle);
    }

    #[test]
    fn missing_es1_entry_counted() {
        let i = index("glDrawTexiOES");
        let before = MISSING.load(Ordering::Relaxed);
        assert!(select(i, true, std::ptr::null_mut).is_null());
        assert_eq!(MISSING.load(Ordering::Relaxed), before + 1);
    }

    static SEEN: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn host_discard(_target: u32, count: i32, _attachments: *const u32) {
        SEEN.store(count as u64, Ordering::Relaxed);
    }

    #[test]
    fn shell_forwards_to_host_on_es2() {
        // no ES1 context on the test thread: the shell calls the recorded ES2/3 answer with its arguments unchanged
        let addr = shell(b"glDiscardFramebufferEXT", host_discard as *const ()).expect("shell");
        // SAFETY: the shell has glDiscardFramebufferEXT's prototype (GLES/glext.h).
        let f: unsafe extern "C" fn(u32, i32, *const u32) = unsafe { std::mem::transmute(addr) };
        // SAFETY: host_discard reads no attachment.
        unsafe { f(0x8D40, 7, std::ptr::null()) };
        assert_eq!(SEEN.load(Ordering::Relaxed), 7);
    }
}
