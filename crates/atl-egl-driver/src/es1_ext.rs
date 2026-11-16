// SPDX-License-Identifier: MIT
//! ES1 extension entries ANGLE lacks (gl-honesty O2). libEGL fills its ES1 table by `dlsym(libGLESv1_CM_atl, name)` -- which also searches that library's DT_NEEDED tree, so every entry ANGLE's `libGLESv2_angle` exports comes from ANGLE -- and only the rest from this driver's eglGetProcAddress, the same call that fills the ES2 table (Loader.cpp `init_api`). One address thus serves both tables.
//! Measured (ANGLE 1166eec4c0b1): 82 of entries_gles1.in's 298 names reach the driver so (OES_fixed_point, APPLE/QCOM/IMG extensions, ...); ANGLE has none of them. Without a shell the ES1 table would call the host's entry with ANGLE's context acquired: a function the ES1 context never advertised, run on the carrier behind ANGLE's state.
//! So for every name of AOSP's `entries_gles1.in` that ES1's `GLES/glext.h` declares, eglGetProcAddress hands out a generated shell (build.rs, atl-bridgegen `selector_module`) that picks per call: an ES1 context current on the thread -> no implementation (0 of the return type, as AOSP's `gl_unimplemented`; the name logged once and counted, `es1 entry missing <name> count=N`), else the ES2/3 answer (fbo0 wrapper or host).
//! A name ANGLE does export never reaches a shell under ES1: libEGL took ANGLE's address for that table.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

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
/// Per shell: its ES1 call was logged.
static REPORTED: [AtomicBool; shells::COUNT] = [const { AtomicBool::new(false) }; shells::COUNT];
/// Names called under ES1 with no ES1 entry: the counter of the missing-entry line.
static MISSING: AtomicU64 = AtomicU64::new(0);

#[allow(
    clippy::print_stderr,
    reason = "one line per name at its first ES1 call: the app's log names the entries its ES1 context lacks"
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

/// The implementation shell `i` calls: none under ES1 (logged and counted once per name), else the ES2/3 answer.
fn select(i: usize, es1_current: bool) -> *mut c_void {
    if !es1_current {
        return HOST[i].load(Ordering::Acquire);
    }
    if !REPORTED[i].swap(true, Ordering::Relaxed) {
        let n = MISSING.fetch_add(1, Ordering::Relaxed) + 1;
        log(&format!("es1 entry missing {} count={n}", shells::NAMES[i]));
    }
    std::ptr::null_mut()
}

/// The selector the generated shells call (build.rs).
///
/// # Safety
/// `F` is the fn-pointer type of shell `i`'s C prototype.
pub unsafe fn pick<F: Copy>(i: usize) -> Option<F> {
    let p = select(i, es1::is_current());
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
        // in entries_gles1.in and ES1 glext.h: names ANGLE lacks (measured gap: OES_fixed_point, EXT_multi_draw_arrays, APPLE_sync) and names ANGLE exports (harmless: only the ES2 table ever calls their shell)
        for name in [
            "glAlphaFuncxOES",
            "glMultiDrawArraysEXT",
            "glFenceSyncAPPLE",
            "glEGLImageTargetTexture2DOES",
        ] {
            assert!(shells::lookup(name.as_bytes()).is_some(), "{name}");
        }
        // core ES 1.1 (GLES/gl.h): ANGLE's ES1 library exports it
        for name in ["glClear", "glDrawArrays", "glActiveTexture"] {
            assert!(shells::lookup(name.as_bytes()).is_none(), "{name}");
        }
        // ES2/3-only names are in no ES1 table
        assert!(shells::lookup(b"glCreateShader").is_none());
    }

    #[test]
    fn host_unless_es1_current_and_es1_counted_once() {
        let i = index("glMultiDrawArraysEXT");
        let host = 0x1000usize as *mut c_void;
        HOST[i].store(host, Ordering::Release);
        assert_eq!(select(i, false), host);
        let before = MISSING.load(Ordering::Relaxed);
        assert!(select(i, true).is_null());
        assert!(select(i, true).is_null());
        // one name, one count however often it is called
        assert_eq!(MISSING.load(Ordering::Relaxed), before + 1);
        assert_eq!(select(i, false), host);
    }

    static SEEN: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn host_alpha(_func: u32, reference: i32) {
        SEEN.store(reference as u64, Ordering::Relaxed);
    }

    #[test]
    fn shell_forwards_to_host_on_es2() {
        // no ES1 context on the test thread: the shell calls the recorded ES2/3 answer with its arguments unchanged
        let addr = shell(b"glAlphaFuncxOES", host_alpha as *const ()).expect("shell");
        // SAFETY: the shell has glAlphaFuncxOES's prototype (GLES/glext.h: GLenum, GLfixed).
        let f: unsafe extern "C" fn(u32, i32) = unsafe { std::mem::transmute(addr) };
        // SAFETY: host_alpha only records its argument.
        unsafe { f(0x0207, 7) };
        assert_eq!(SEEN.load(Ordering::Relaxed), 7);
    }
}
