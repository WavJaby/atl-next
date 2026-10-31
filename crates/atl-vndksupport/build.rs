// SPDX-License-Identifier: MIT
//! Generates, with atl-bridgegen from AOSP's pinned `vndksupport/linker.h` for the target cargo builds
//! for: `OUT_DIR/capi/include/vndksupport/linker.h` (declarations of exactly what `crate::ffi`
//! exports; cargo-c installs it) and `OUT_DIR/linker.rs` (the check that each export has the header's
//! signature). Generates twice and fails unless byte-identical.

use std::path::PathBuf;

use anyhow::{Result, ensure};
use atl_bridgegen::Lang;
use atl_bridgegen::buildscript::{self, Api, DECLARATIONS, Upstreams};

/// `android_is_in_vendor_process` (deprecated in the header) is not implemented: nothing built here
/// calls it, and a single namespace has no vendor process to report.
const API: Api = Api {
    name: "linker",
    source: "system/core/libvndksupport/include/vndksupport/linker.h",
    lang: Lang::C,
    header: Some("vndksupport/linker.h"),
    functions: &["android_load_sphal_library", "android_unload_sphal_library"],
    macros: &[],
    include_dirs: &[],
    kept: DECLARATIONS,
};

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    up.file(&format!("aosp/{}", API.source))?;
    // the header includes nothing; bionic's view is the one every bionic-side header is read in
    let generate = || {
        buildscript::api_files(
            &up,
            &API,
            buildscript::bionic_libc_target(&up, &host, None),
            "crate::ffi",
        )
    };
    let files = generate()?;
    ensure!(
        files == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let capi = out.join("capi");
    if capi.exists() {
        std::fs::remove_dir_all(&capi)?;
    }
    for (rel, text) in &files {
        let path = out.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text)?;
    }
    println!("cargo::rerun-if-changed={}", arches.display());
    for path in up.rerun_paths(&format!("aosp/{}", API.source)) {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    Ok(())
}
