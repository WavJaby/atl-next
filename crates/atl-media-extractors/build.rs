// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): compiles the cxx glue of src/bridge.rs and the forwarder in wrap/extractors.cpp with the C++ argv of `device/libstagefright`'s own compiles (graph.json `cxx`): libc++, device defines, the media headers.
//! Without the feature nothing: the crate is empty.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::converter::{self, Converter};

/// The converter module whose C++ argv the forwarder compiles with; its library is the one the call links.
const STAGEFRIGHT: &str = "device/libstagefright";

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=src/bridge.rs");
    println!("cargo::rerun-if-changed=wrap");
    // libbinder_rs's declare_binder_interface! tests these cfgs in the expanding crate; unset here, as for a host build
    println!("cargo::rustc-check-cfg=cfg(android_vndk, android_ndk)");
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR")?);
    let out = Converter::from_env()?;
    let module = out.artifact(STAGEFRIGHT)?;
    let mut build = cxx_build::bridge("src/bridge.rs");
    build
        .cpp(true)
        .compiler(&out.link_driver)
        .no_default_flags(true)
        .include(&manifest)
        .file("wrap/extractors.cpp");
    for flag in &module.cxx {
        build.flag(flag);
    }
    build.compile("atl_media_extractors");
    for line in converter::link_package(&manifest)? {
        println!("{line}");
    }
    // the forwarder and libstagefright are libc++ C++ (the converter's STL): its runtime comes last
    println!("cargo::rustc-link-lib=dylib=c++");
    Ok(())
}
