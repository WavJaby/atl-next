// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): links the converter's libbinder_rs and libatl_parcelables (Cargo.toml `[package.metadata.atl]`), and compiles the golden's cxx glue (src/capture.rs) with the adapters in wrap/capture.cpp.
//! The adapters take the C++ argv of `device/libgui` and `device/libbinder_ndk` (graph.json `cxx`): libgui's headers and libbinder_ndk's platform header (android/binder_libbinder.h). Without the feature nothing.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::converter::{self, Converter};

/// The converter modules whose C++ argv the adapters compile with.
const LIBGUI: &str = "device/libgui";
const LIBBINDER_NDK: &str = "device/libbinder_ndk";

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=src/capture.rs");
    println!("cargo::rerun-if-changed=wrap");
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR")?);
    let out = Converter::from_env()?;
    let mut build = cxx_build::bridge("src/capture.rs");
    build
        .cpp(true)
        .compiler(&out.link_driver)
        .no_default_flags(true)
        .include(&manifest)
        .file("wrap/capture.cpp");
    for module in [LIBGUI, LIBBINDER_NDK] {
        for flag in &out.artifact(module)?.cxx {
            build.flag(flag);
        }
    }
    build.compile("atl_parcel_truth_capture");
    for line in converter::link_package(&manifest)? {
        println!("{line}");
    }
    // the adapters and libgui are libc++ C++ (the converter's STL): its runtime comes last
    println!("cargo::rustc-link-lib=dylib=c++");
    // sp<>'s inline code in the adapters calls libutils' sp_report_race: libutils is on the link line (Cargo.toml converter list)
    Ok(())
}
