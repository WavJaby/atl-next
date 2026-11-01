// SPDX-License-Identifier: MIT
//! AOSP libEGL's private API on the host EGL, exported under the Itanium-mangled names framework code links against: `private/EGL/cache.h`, `display.h` (C++ `namespace android`), and `eglBeginFrame` (global, C++ linkage, declared by its caller only).
//! `crate::generated::{cache, display}` check those exports' signatures against AOSP's header; `eglBeginFrame` has no header to check against. This module's tests check each symbol against the mangling rules.
//! Boundary module: `unsafe` is the exported names and one host EGL call.

use core::ffi::{c_char, c_int, c_void};

/// egl.h `EGL_VERSION` (Khronos registry value).
const EGL_VERSION: c_int = 0x3054;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglQueryString(dpy: *mut c_void, name: c_int) -> *const c_char;
}

/// AOSP `egl_get_init_count` = the display's eglInitialize reference count (`egl_display_t::refs`: +1 per eglInitialize, -1 per eglTerminate; AOSP's eglTerminate is ref-counted, "specific to Android"), 0 for a handle libEGL never returned.
/// The host EGL keeps no count: EGL 1.5 §3.2 makes eglInitialize on an initialized display a no-op and one eglTerminate final, and EGL_KHR_display_reference (Khronos' opt-in counting) exposes only whether tracking is on, never a count -- and WSL's Mesa 25.0.7 does not offer it (client extensions, measured 2026-11-01).
/// So: 1 while the display is initialized (its EGL_VERSION string exists), else 0. Known differences: 1 where AOSP counts more than one eglInitialize; eglQueryString sets the thread's EGL error (EGL_SUCCESS, or EGL_NOT_INITIALIZED / EGL_BAD_DISPLAY), which AOSP's lookup leaves alone.
/// Impact: the one caller is `EGLImpl.getInitCount`, a native of the hidden `com.google.android.gles_jni.EGLImpl` with no caller in the pinned frameworks/base (2026-11-01).
fn init_count(dpy: *mut c_void) -> c_int {
    // EGL_NO_DISPLAY: libglvnd answers EGL_VERSION for it with the client library's version (EGL 1.5 §3.3), not a display's; AOSP's lookup fails on 0
    if dpy.is_null() {
        return 0;
    }
    // SAFETY: eglQueryString accepts any handle: libglvnd and Mesa look it up in their display lists and fail with EGL_BAD_DISPLAY on one they never returned (measured with a forged handle, 2026-11-01); EGL_VERSION is a valid name
    let version = unsafe { eglQueryString(dpy, EGL_VERSION) };
    c_int::from(!version.is_null())
}

/// One export per declaration: Rust name, signature and body, with its mangled symbol. The test re-derives each symbol, so the table cannot drift from the attribute.
macro_rules! exports {
    ($($(#[doc = $doc:literal])* $symbol:literal => fn $name:ident($($arg:ident: $ty:ty),*) $(-> $ret:ty)? $body:block)*) => {
        /// C++ `namespace android`.
        pub mod android {
            use core::ffi::{c_char, c_int, c_void};
            $(
                $(#[doc = $doc])*
                // SAFETY: a name nothing else in the process defines (AOSP's libEGL is not built; the host's has no such symbol); the signature is the header's (checked by `crate::generated`), and a free C++ function's ABI is the C one
                #[unsafe(export_name = $symbol)]
                pub extern "C" fn $name($($arg: $ty),*) $(-> $ret)? $body
            )*
        }

        #[cfg(test)]
        const SYMBOLS: &[(&str, &str)] = &[$((stringify!($name), $symbol)),*];
    };
}

exports! {
    /// No-op: hwui passes the app's shader cache file here for AOSP libEGL's EGL_ANDROID_blob_cache cache. Mesa keeps its own on-disk shader cache (per user, `MESA_SHADER_CACHE_DIR` overrides where), so there is nothing to point at the file.
    /// An Android-style per-app cache would hook in through EGL_ANDROID_blob_cache (`eglSetBlobCacheFuncsANDROID`) where the host driver offers it.
    "_ZN7android22egl_set_cache_filenameEPKc" => fn egl_set_cache_filename(_filename: *const c_char) {}
    /// See `init_count`.
    "_ZN7android18egl_get_init_countEPv" => fn egl_get_init_count(dpy: *mut c_void) -> c_int {
        super::init_count(dpy)
    }
}

/// `eglBeginFrame` mangled: a global-scope function with C++ linkage. hwui `renderthread/EglManager.cpp` declares it itself, outside any `extern "C"`, as `EGLAPI void EGLAPIENTRY eglBeginFrame(EGLDisplay dpy, EGLSurface surface)`; AOSP's `eglApi.cpp` defines it the same way, so libEGL exports the mangled name.
/// A macro so the attribute and the test read one literal.
macro_rules! begin_frame {
    () => {
        "_Z13eglBeginFramePvS_"
    };
}

/// No-op. AOSP's (`eglApi.cpp` -> `egl_platform_entries.cpp` `eglBeginFrameImpl`) emits a systrace slice (`ATRACE_CALL`), sets the thread's EGL error to EGL_SUCCESS, then to EGL_BAD_DISPLAY / EGL_NOT_INITIALIZED or EGL_BAD_SURFACE for a bad handle; it draws, swaps and returns nothing.
/// Known difference: the thread's EGL error is left as it was. hwui calls it from `EglManager::beginFrame` and reads no EGL error after it.
// SAFETY: a name nothing else in the process defines (AOSP's libEGL is not built; the host's exports only C names); the signature is hwui's declaration (both handles `void*` in egl.h), and a free C++ function's ABI is the C one
#[unsafe(export_name = begin_frame!())]
pub extern "C" fn egl_begin_frame(_dpy: *mut c_void, _surface: *mut c_void) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Itanium C++ ABI 5.1 (mangling) of a free function: `_Z` <len><name> + parameter types at global scope, `_Z` `N` <len><ns> <len><name> `E` + parameter types in namespace `ns`.
    fn mangle(ns: Option<&str>, name: &str, params: &str) -> String {
        let source = |s: &str| format!("{}{s}", s.len());
        match ns {
            Some(ns) => format!("_ZN{}{}E{params}", source(ns), source(name)),
            None => format!("_Z{}{params}", source(name)),
        }
    }

    #[test]
    fn mangled_names_match_the_declarations() {
        // parameter types as the headers declare them, Itanium 5.1.5: `const char*` = P (pointer) K (const) c (char); EGLDisplay = `void*` (egl.h typedef, transparent to mangling) = P v
        let params = |name: &str| match name {
            "egl_set_cache_filename" => "PKc",
            "egl_get_init_count" => "Pv",
            other => panic!("{other}: no declared parameter types"),
        };
        assert_eq!(SYMBOLS.len(), 2);
        for (name, symbol) in SYMBOLS {
            assert_eq!(
                &mangle(Some("android"), name, params(name)),
                symbol,
                "{name}"
            );
        }
        // (EGLDisplay, EGLSurface) = (void*, void*): the first `Pv` is substitution candidate 0 (5.1.10; builtin `v` is none), so the second is `S_`
        assert_eq!(mangle(None, "eglBeginFrame", "PvS_"), begin_frame!());
    }

    #[test]
    fn no_display_has_no_count() {
        // EGL_NO_DISPLAY never reaches the host: AOSP returns 0 for it
        assert_eq!(init_count(core::ptr::null_mut()), 0);
    }
}
