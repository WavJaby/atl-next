// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): compiles the cxx glue of src/bridge.rs and the adapters in wrap/ with the C++ argv of the three AOSP modules they include (libaudioclient, libaudioprocessing_base, libaudioutils: graph.json `cxx`), so the adapters see AOSP's class layouts.
//! Without the feature nothing: the crate is empty.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::converter::{self, Converter};

/// The converter modules whose C++ argv the adapters compile with and whose shared libraries they link.
const MODULES: [&str; 3] = [
    "device/libaudioclient",
    "device/libaudioprocessing_base",
    "device/libaudioutils",
];

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=src/bridge.rs");
    println!("cargo::rerun-if-changed=wrap");
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR")?);
    let out = Converter::from_env()?;
    let mut build = cxx_build::bridge("src/bridge.rs");
    build
        .cpp(true)
        .compiler(&out.link_driver)
        .no_default_flags(true)
        .include(&manifest)
        .file("wrap/mix.cpp");
    // every module's flags in order, as atl-media-player passes its two: the adapters include headers from all three. Each token is kept: de-duplicating single tokens drops the second `-include` of a pair, and clang then reads its header as a source file
    for module in MODULES {
        for flag in &out.artifact(module)?.cxx {
            build.flag(flag);
        }
    }
    build.compile("atl_audio_bridge");
    for line in converter::link_package(&manifest)? {
        println!("{line}");
    }
    // AOSP's libaudioclient and libaudioprocessing_base are libc++ C++: its runtime comes last
    println!("cargo::rustc-link-lib=dylib=c++");
    Ok(())
}
