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

fn main() -> anyhow::Result<()> {
    import_headers(IMPORTS)
}
