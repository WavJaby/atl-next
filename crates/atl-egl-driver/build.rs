// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen, twice and failing unless byte-identical, `OUT_DIR/<name>.rs` per entry of [`IMPORTS`] (bridge controls 1 and 2):
//! per function the header's fn-pointer type (`import_module`; lib.rs checks each export against it and types every host call with it), the constants the driver uses, and for `window` the `ANativeWindow` / `ANativeWindowBuffer` records with clang's layout asserted.
//! Headers are AOSP's (the EGL / GLES headers the fronts compile against, libnativewindow's `system/window.h`), parsed in bionic's view as every AOSP C header here is; the driver itself runs on the host libc, whose C types have the same sizes on every arches.toml host.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::{Lang, Source};

struct Import {
    name: &'static str,
    /// The translation unit, relative to the AOSP tree.
    source: &'static str,
    /// Headers it includes whose declarations belong to the interface.
    also: &'static [&'static str],
    include_dirs: &'static [&'static str],
    /// `-D` defines: the Khronos headers declare extension prototypes only under `*_EXT_PROTOTYPES`.
    defines: &'static [&'static str],
    /// A header the source needs first and does not include itself (Khronos `eglext.h` / `gl2ext.h` use the core header's types; the registry's convention is that the caller includes both).
    force_include: Option<&'static str>,
    functions: &'static [&'static str],
    types: &'static [&'static str],
    macros: &'static [&'static str],
}

const EGL_DIR: &str = "frameworks/native/opengl/include";

/// EGL entries the driver exports (Loader.cpp dlsyms them) and calls on the host for every handle that is not its own.
pub const EGL_EXPORTS: &[&str] = &[
    "eglGetProcAddress",
    "eglGetError",
    "eglGetDisplay",
    "eglTerminate",
    "eglQueryString",
    "eglChooseConfig",
    "eglGetConfigAttrib",
    "eglCreateWindowSurface",
    "eglDestroySurface",
    "eglQuerySurface",
    "eglSurfaceAttrib",
    "eglMakeCurrent",
    "eglSwapBuffers",
    "eglSwapBuffersWithDamageKHR",
    "eglSwapInterval",
    "eglCreateImageKHR",
];

/// GL entries the driver answers through eglGetProcAddress so that framebuffer 0 means the window surface's FBO, plus the host calls that build it.
const GL_FUNCTIONS: &[&str] = &[
    "glBindFramebuffer",
    "glGetIntegerv",
    "glInvalidateFramebuffer",
    "glInvalidateSubFramebuffer",
    "glDiscardFramebufferEXT",
    "glDrawBuffers",
    "glReadBuffer",
    "glDeleteFramebuffers",
    "glIsFramebuffer",
    "glGenFramebuffers",
    "glGenRenderbuffers",
    "glBindRenderbuffer",
    "glRenderbufferStorage",
    "glFramebufferRenderbuffer",
    "glFramebufferTexture2D",
    "glCheckFramebufferStatus",
    "glGenTextures",
    "glBindTexture",
    "glTexParameteri",
    "glDeleteTextures",
    "glDeleteRenderbuffers",
    "glEGLImageTargetTexture2DOES",
    "glViewport",
    "glScissor",
    "glFinish",
];

const IMPORTS: &[Import] = &[
    Import {
        name: "egl",
        source: "frameworks/native/opengl/include/EGL/eglext.h",
        also: &["frameworks/native/opengl/include/EGL/egl.h"],
        include_dirs: &[EGL_DIR],
        defines: &["EGL_EGLEXT_PROTOTYPES"],
        force_include: Some("EGL/egl.h"),
        functions: EGL_EXPORTS,
        types: &[],
        macros: &[
            "EGL_FALSE",
            "EGL_TRUE",
            "EGL_SUCCESS",
            "EGL_BAD_ACCESS",
            "EGL_BAD_ALLOC",
            "EGL_BAD_ATTRIBUTE",
            "EGL_BAD_CONFIG",
            "EGL_BAD_CONTEXT",
            "EGL_BAD_DISPLAY",
            "EGL_DONT_CARE",
            "EGL_BAD_MATCH",
            "EGL_BAD_NATIVE_WINDOW",
            "EGL_BAD_PARAMETER",
            "EGL_BAD_SURFACE",
            "EGL_NONE",
            "EGL_VENDOR",
            "EGL_VERSION",
            "EGL_EXTENSIONS",
            "EGL_CLIENT_APIS",
            "EGL_SURFACE_TYPE",
            "EGL_WINDOW_BIT",
            "EGL_PBUFFER_BIT",
            "EGL_SWAP_BEHAVIOR_PRESERVED_BIT",
            "EGL_NATIVE_VISUAL_ID",
            "EGL_NATIVE_RENDERABLE",
            "EGL_RED_SIZE",
            "EGL_GREEN_SIZE",
            "EGL_BLUE_SIZE",
            "EGL_ALPHA_SIZE",
            "EGL_DEPTH_SIZE",
            "EGL_STENCIL_SIZE",
            "EGL_WIDTH",
            "EGL_HEIGHT",
            "EGL_CONFIG_ID",
            "EGL_RENDER_BUFFER",
            "EGL_BACK_BUFFER",
            "EGL_SWAP_BEHAVIOR",
            "EGL_BUFFER_DESTROYED",
            "EGL_BUFFER_PRESERVED",
            "EGL_GL_COLORSPACE_KHR",
            "EGL_RECORDABLE_ANDROID",
            "EGL_FRAMEBUFFER_TARGET_ANDROID",
            "EGL_NATIVE_BUFFER_ANDROID",
        ],
    },
    Import {
        name: "gles",
        source: "frameworks/native/opengl/include/GLES2/gl2ext.h",
        also: &["frameworks/native/opengl/include/GLES3/gl3.h"],
        include_dirs: &[EGL_DIR],
        defines: &["GL_GLEXT_PROTOTYPES"],
        force_include: Some("GLES3/gl3.h"),
        functions: GL_FUNCTIONS,
        types: &[],
        macros: &[
            "GL_FALSE",
            "GL_TRUE",
            "GL_NO_ERROR",
            "GL_FRAMEBUFFER",
            "GL_DRAW_FRAMEBUFFER",
            "GL_READ_FRAMEBUFFER",
            "GL_FRAMEBUFFER_BINDING",
            "GL_READ_FRAMEBUFFER_BINDING",
            "GL_RENDERBUFFER",
            "GL_RENDERBUFFER_BINDING",
            "GL_TEXTURE_2D",
            "GL_TEXTURE_BINDING_2D",
            "GL_TEXTURE_MIN_FILTER",
            "GL_TEXTURE_MAG_FILTER",
            "GL_NEAREST",
            "GL_COLOR",
            "GL_DEPTH",
            "GL_STENCIL",
            "GL_BACK",
            "GL_COLOR_ATTACHMENT0",
            "GL_DEPTH_ATTACHMENT",
            "GL_STENCIL_ATTACHMENT",
            "GL_DEPTH_STENCIL_ATTACHMENT",
            "GL_DEPTH24_STENCIL8",
            "GL_DEPTH_COMPONENT16",
            "GL_STENCIL_INDEX8",
            "GL_FRAMEBUFFER_COMPLETE",
        ],
    },
    Import {
        name: "window",
        source: "frameworks/native/libs/nativewindow/include/system/window.h",
        also: &[
            "frameworks/native/libs/nativebase/include/nativebase/nativebase.h",
            "system/core/libcutils/include/cutils/native_handle.h",
        ],
        include_dirs: &[
            "frameworks/native/libs/nativewindow/include",
            "frameworks/native/libs/nativebase/include",
            "frameworks/native/libs/arect/include",
            "system/core/libcutils/include",
            "system/core/libsystem/include",
        ],
        defines: &[],
        force_include: None,
        functions: &[],
        types: &["ANativeWindow", "ANativeWindowBuffer_t", "native_handle_t"],
        macros: &["ANDROID_NATIVE_WINDOW_MAGIC", "ANDROID_NATIVE_BUFFER_MAGIC"],
    },
];

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let mut rerun = vec![arches];
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    for import in IMPORTS {
        let path = up.file(&format!("aosp/{}", import.source))?;
        let also = import
            .also
            .iter()
            .map(|f| up.file(&format!("aosp/{f}")))
            .collect::<Result<Vec<_>>>()?;
        let mut target = buildscript::bionic_libc_target(&up, &host, None);
        target.args.extend(
            import
                .include_dirs
                .iter()
                .map(|d| format!("-I{}", up.aosp(d).display())),
        );
        target
            .args
            .extend(import.defines.iter().map(|d| format!("-D{d}")));
        if let Some(h) = import.force_include {
            target.args.extend(["-include".to_owned(), h.to_owned()]);
        }
        let generate = || -> Result<String> {
            let src = Source {
                path: &path,
                lang: Lang::C,
                also: &also,
                macros: import.macros,
            };
            let iface = atl_bridgegen::interface(
                import.name,
                import.source,
                &src,
                std::slice::from_ref(&target),
            )
            .context(import.source)?;
            atl_bridgegen::rust::import_module(&iface, &host.arch, import.functions, import.types)
        };
        let code = generate()?;
        ensure!(
            code == generate()?,
            "atl-bridgegen output differs between two runs on the same input"
        );
        std::fs::write(out.join(format!("{}.rs", import.name)), code)?;
        for d in import.include_dirs {
            rerun.extend(up.rerun_paths(&format!("aosp/{d}")));
        }
        rerun.extend(up.rerun_paths(&format!("aosp/{}", import.source)));
    }
    for path in rerun {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    Ok(())
}
