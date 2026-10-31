// SPDX-License-Identifier: MIT
//! Generates, with atl-bridgegen for the target cargo builds for: `OUT_DIR/capi/include/vndksupport/linker.h`
//! from AOSP's pinned header (declarations of exactly what `crate::ffi` exports; cargo-c installs it),
//! and `OUT_DIR/<api>.rs` per API (types, and the check that each export -- and each bionic function
//! `crate::ffi` imports -- has its upstream signature).

use anyhow::Result;
use atl_bridgegen::Lang;
use atl_bridgegen::buildscript::{self, Api, DECLARATIONS};

const APIS: &[Api] = &[
    // `android_is_in_vendor_process` (deprecated in the header) is not implemented: nothing built here
    // calls it, and a single namespace has no vendor process to report.
    Api {
        name: "linker",
        source: "system/core/libvndksupport/include/vndksupport/linker.h",
        lang: Lang::C,
        header: Some("vndksupport/linker.h"),
        functions: &["android_load_sphal_library", "android_unload_sphal_library"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // imported from atl-bionic, declared as bionic does: android_dlextinfo and the call
    Api {
        name: "dlext",
        source: "bionic/libc/include/android/dlext.h",
        lang: Lang::C,
        header: None,
        functions: &["android_dlopen_ext"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    Api {
        name: "libdl_android",
        source: "bionic/libdl/libdl_android.cpp",
        lang: Lang::Cxx,
        header: None,
        functions: &["android_get_exported_namespace"],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
];

fn main() -> Result<()> {
    for path in buildscript::api_crate(APIS, "crate::ffi")? {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    Ok(())
}
