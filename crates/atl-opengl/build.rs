// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen, for the target cargo builds for, each twice and failing unless byte-identical:
//! - `OUT_DIR/glext.rs` from AOSP's pinned Khronos headers (`frameworks/native/opengl/include`): one exported shell per EGL / GLES extension function, forwarding to what `eglGetProcAddress` returns (bridge control 1; `crate::ffi::resolve`).
//! - `PRIVATE`: `OUT_DIR/capi/include/private/EGL/{cache,display}.h` (cargo-c installs them) declaring exactly the libEGL private functions `crate::private` exports, and `OUT_DIR/{cache,display,gles_cm}.rs`, the check that each export has its declaration's signature.
//!
//! Extension function = declared by an extension header (`EXT_HEADERS`) and by no core header (`CORE_HEADERS`). The headers decide, not the host libraries' exports: the set is the same on every host and at every build, and the host loaders (libglvnd) export exactly the core headers' functions -- measured 2026-10-31 on Debian 13: libEGL 44 symbols, libGLESv2 358, libGLESv1_CM 145, no extension function among them besides GLES 1.1's core `glPointSizePointerOES`.
//! Headers are parsed as the converter compiles their callers: glibc host target, `ANDROID` (a Soong global define) and `__ANDROID__` (the device triple's, owner 2026-11-01) -- eglplatform.h's Android native types --, prototypes enabled.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript::{self, Api, DECLARATIONS, Upstreams};
use atl_bridgegen::ir::Ty;
use atl_bridgegen::{Lang, Source, Target};

/// AOSP's Khronos headers, relative to the AOSP tree.
const INCLUDE: &str = "frameworks/native/opengl/include";

/// AOSP libEGL's / libGLESv1_CM's private API that framework code calls: only these declaring files, each only its functions, so declared == implemented == checked. The rest of `frameworks/native/opengl/libs/EGL/include` declares what nothing here implements.
/// Callers (2026-11-01): hwui `android_graphics_HardwareRenderer.cpp` (`egl_set_cache_filename`), libandroid_runtime `com_google_android_gles_jni_EGLImpl.cpp` (`egl_get_init_count`), libandroid_runtime's GLES 1 bindings (`gl*PointerBounds`).
const PRIVATE: &[Api] = &[
    Api {
        name: "cache",
        source: "frameworks/native/opengl/libs/EGL/include/private/EGL/cache.h",
        lang: Lang::Cxx,
        header: Some("private/EGL/cache.h"),
        functions: &["android::egl_set_cache_filename"],
        macros: &[],
        // its <cutils/compiler.h> (ANDROID_API)
        include_dirs: &["system/core/libcutils/include"],
        kept: DECLARATIONS,
    },
    Api {
        name: "display",
        source: "frameworks/native/opengl/libs/EGL/include/private/EGL/display.h",
        lang: Lang::Cxx,
        header: Some("private/EGL/display.h"),
        functions: &["android::egl_get_init_count"],
        macros: &[],
        // its <EGL/egl.h> (EGLDisplay, which the printed header includes again) and <cutils/compiler.h>
        include_dirs: &[INCLUDE, "system/core/libcutils/include"],
        kept: DECLARATIONS,
    },
    // libGLESv1_CM's "extensions for the framework": no header declares them; AOSP's implementation file does (C linkage), and each caller repeats the same block (libandroid_runtime's glgen output: `android_opengl_GLES10.cpp`, `GLES10Ext.cpp`, `GLES11.cpp`, `GLES11Ext.cpp`, `com_google_android_gles_jni_GLImpl.cpp`).
    // The implementation's declarations are checked: they are what AOSP's libGLESv1_CM exports.
    Api {
        name: "gles_cm",
        source: "frameworks/native/opengl/libs/GLES_CM/gl.cpp",
        lang: Lang::Cxx,
        header: None,
        functions: &[
            "glColorPointerBounds",
            "glNormalPointerBounds",
            "glTexCoordPointerBounds",
            "glVertexPointerBounds",
            "glPointSizePointerOESBounds",
            "glMatrixIndexPointerOESBounds",
            "glWeightPointerOESBounds",
        ],
        macros: &[],
        // <log/log.h>, <cutils/properties.h>, the Khronos headers, and hooks.h's <bionic/tls.h> (bionic's platform header)
        include_dirs: &[
            INCLUDE,
            "system/logging/liblog/include",
            "system/core/libcutils/include",
            "bionic/libc/platform",
        ],
        kept: DECLARATIONS,
    },
];

/// Core API headers: what the host libEGL / libGLESv1_CM / libGLESv2 export (GLES 3.x core is libGLESv2's).
const CORE_HEADERS: &[&str] = &[
    "EGL/egl.h",
    "GLES/gl.h",
    "GLES2/gl2.h",
    "GLES3/gl3.h",
    "GLES3/gl31.h",
    "GLES3/gl32.h",
];

/// Extension headers: EGL, GLES 2.0+ and GLES 1.x extensions (`GLES3/gl3ext.h` declares no function).
const EXT_HEADERS: &[&str] = &["EGL/eglext.h", "GLES2/gl2ext.h", "GLES/glext.h"];

/// What a shell does when `eglGetProcAddress` has no implementation (the driver lacks the extension).
/// EGL 1.5 §3.10 leaves calling an unsupported extension function undefined and gives no way to set the EGL error from outside the implementation, so each return type gets the failure value every EGL / GLES function returning it documents:
/// - void: nothing happens (GL functions report errors through glGetError only).
/// - pointer or handle (`EGLImageKHR`, `EGLSyncKHR`, `EGLDisplay`, `EGLClientBuffer`, `GLsync`, `void *`): null = `EGL_NO_*` / `NULL`, each function's failure value.
/// - `EGLBoolean` / `GLboolean`: `EGL_FALSE` / `GL_FALSE`.
/// - any other (integers, floats): no failure value common to the type (`EGLint`: -1 = `EGL_NO_NATIVE_FENCE_FD_ANDROID` from eglDupNativeFenceFDANDROID, `EGL_FALSE` from eglClientWaitSyncKHR, an error code from eglLabelObjectKHR), so the process aborts naming the function (`crate::ffi::unsupported`) rather than return a value its caller may take for success.
fn unsupported(name: &str, ret: &Ty) -> Result<String> {
    if let Ty::Typedef { name: t, .. } = ret
        && (t == "EGLBoolean" || t == "GLboolean")
    {
        return Ok("return 0".to_owned());
    }
    Ok(match ret.resolved() {
        Ty::Void => "return".to_owned(),
        Ty::Pointer { pointee, .. } if matches!(pointee.resolved(), Ty::Function(_)) => {
            "return None".to_owned()
        }
        Ty::Pointer { is_const: true, .. } => "return ::core::ptr::null()".to_owned(),
        Ty::Pointer { .. } => "return ::core::ptr::null_mut()".to_owned(),
        _ => format!("crate::ffi::unsupported(c\"{name}\")"),
    })
}

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let include = up.aosp(INCLUDE);
    let rel = |h: &str| format!("aosp/{INCLUDE}/{h}");
    let headers: Vec<&str> = CORE_HEADERS.iter().chain(EXT_HEADERS).copied().collect();
    for h in &headers {
        up.file(&rel(h))?;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    // one unit including every header: each extension header expects its core header's types first
    let unit = out.join("glext.c");
    let text: String = headers
        .iter()
        .map(|h| format!("#include <{h}>\n"))
        .collect();
    std::fs::write(&unit, text)?;
    let target = Target {
        arch: host.arch.clone(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: vec![
            format!("-I{}", include.display()),
            "-DANDROID".to_owned(),
            "-D__ANDROID__".to_owned(),
            "-DEGL_EGLEXT_PROTOTYPES".to_owned(),
            "-DGL_GLEXT_PROTOTYPES".to_owned(),
        ],
    };
    let generate = || -> Result<String> {
        let parse = |owned: &[&str]| {
            let also: Vec<PathBuf> = owned.iter().map(|h| include.join(h)).collect();
            let src = Source {
                path: &unit,
                lang: Lang::C,
                also: &also,
                macros: &[],
            };
            atl_bridgegen::interface("glext", INCLUDE, &src, std::slice::from_ref(&target))
                .with_context(|| owned.join(", "))
        };
        let core = parse(CORE_HEADERS)?;
        let ext = parse(EXT_HEADERS)?;
        let core = &core.arches[&host.arch].functions;
        let functions: Vec<&str> = ext.arches[&host.arch]
            .functions
            .keys()
            .filter(|f| !core.contains_key(*f))
            .map(String::as_str)
            .collect();
        atl_bridgegen::rust::resolver_module(
            &ext,
            &host.arch,
            &functions,
            "crate::ffi::resolve",
            &unsupported,
        )
    };
    let code = generate()?;
    ensure!(
        code == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    std::fs::write(out.join("glext.rs"), code)?;
    println!("cargo::rerun-if-changed={}", arches.display());
    for h in &headers {
        for path in up.rerun_paths(&rel(h)) {
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
    for path in buildscript::api_crate(PRIVATE, "crate::private")? {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    Ok(())
}
