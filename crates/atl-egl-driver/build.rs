// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen (bridge controls 1 and 2) `OUT_DIR/window.rs`: libnativewindow's `ANativeWindow` / `ANativeWindowBuffer` / `native_handle_t` records with clang's layout asserted.
//! The EGL / GLES ABI is atl-egl-host's (one typing of every entry, its build.rs); this crate adds only the window side.

use atl_bridgegen::buildscript::{HeaderImport, import_headers};

const IMPORTS: &[HeaderImport] = &[HeaderImport {
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
}];

/// `ATL_EGL_BREAK=<name>[,<name>]` selects [`BREAKS`] (atl-native-bridge's pattern). Read here, so the cfg reaches this package only; the run that stages the driver sets it (`xtask test app`).
const BREAK_ENV: &str = "ATL_EGL_BREAK";
/// The deliberate breaks: name -> the cfg it turns on. A break is a gate failing on purpose: red with it, green without.
const BREAKS: &[(&str, &str)] = &[
    // the app gate's firstConfig row (exports.rs `handed_out`): every host config is handed out again, the 16-bit ones first
    ("no_config_filter", "atl_egl_no_config_filter"),
    // the window connect (surface.rs `connect`): no window is connected to EGL, so every dequeue has no connected producer (the Happy Jump failure)
    ("no_window_connect", "atl_egl_no_window_connect"),
];

/// The cfgs `ATL_EGL_BREAK` names; an unknown name fails the build, naming it and the known ones.
fn selected_breaks() -> anyhow::Result<Vec<&'static str>> {
    let Ok(value) = std::env::var(BREAK_ENV) else {
        return Ok(Vec::new());
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            BREAKS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|&(_, cfg)| cfg)
                .ok_or_else(|| {
                    let known: Vec<&str> = BREAKS.iter().map(|&(n, _)| n).collect();
                    anyhow::anyhow!(
                        "{BREAK_ENV}={name}: no such break (known: {})",
                        known.join(", ")
                    )
                })
        })
        .collect()
}

#[allow(
    clippy::print_stdout,
    reason = "a build script's stdout is cargo's instruction channel"
)]
fn main() -> anyhow::Result<()> {
    // the driver's own egl* exports bind to themselves: AOSP's libEGL, loaded first, exports the same names, and a preemptible reference (an address the driver hands out, e.g. from eglGetProcAddress) would resolve to the front, which calls back into the driver (`test gl` elf check)
    println!("cargo::rustc-cdylib-link-arg=-Wl,-Bsymbolic");
    // every break's cfg is declared, so an unselected one is no unexpected-cfg warning
    for &(_, cfg) in BREAKS {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
    }
    println!("cargo::rerun-if-env-changed={BREAK_ENV}");
    for cfg in selected_breaks()? {
        println!("cargo::rustc-cfg={cfg}");
    }
    import_headers(IMPORTS)
}
