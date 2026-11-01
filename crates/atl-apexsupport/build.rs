// SPDX-License-Identifier: MIT
//! Generates, with atl-bridgegen for the target cargo builds for: `OUT_DIR/capi/include/android/apexsupport.h`
//! from AOSP's pinned header (declarations of exactly what `crate::ffi` exports; cargo-c installs it),
//! and `OUT_DIR/<api>.rs` per API (types, and the check that each export -- and each bionic function
//! `crate::ffi` imports -- has its upstream signature).

use anyhow::Result;
use atl_bridgegen::Lang;
use atl_bridgegen::buildscript::{self, Api, DECLARATIONS};

const APIS: &[Api] = &[
    // `AApexInfo_*` (an executable's own APEX) are not implemented: nothing built here calls them, and
    // no executable here runs from an APEX.
    Api {
        name: "apexsupport",
        source: "system/apex/libs/libapexsupport/include/android/apexsupport.h",
        lang: Lang::C,
        header: Some("android/apexsupport.h"),
        functions: &["AApexSupport_loadLibrary"],
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
