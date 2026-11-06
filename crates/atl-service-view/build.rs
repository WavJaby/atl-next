// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): compiles the cxx glue of src/bridge.rs and the adapters in wrap/ with the C++ argv of `device/libbinder_ndk`'s own compiles (graph.json `cxx`): libc++, device defines, libbinder's and libbinder_ndk's headers.
//! That compile is also the gate on the test-named libbinder helper the adapter calls (wrap/view.cpp): a pin bump that removes it or changes its signature fails here, at `xtask native`, naming the header line.
//! Without the feature nothing: the crate is empty.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::converter::{self, Converter};

/// The converter module whose C++ argv the adapters compile with: it sees libbinder's exported headers and the NDK's platform header `android/binder_libbinder.h`.
const LIBBINDER_NDK: &str = "device/libbinder_ndk";

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=src/bridge.rs");
    println!("cargo::rerun-if-changed=wrap");
    // libbinder_rs's declare_binder_interface! tests these cfgs in the expanding crate (libs/binder/rust/src/binder.rs); unset here, as for a host build
    println!("cargo::rustc-check-cfg=cfg(android_vndk, android_ndk)");
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR")?);
    let out = Converter::from_env()?;
    let module = out.artifact(LIBBINDER_NDK)?;
    let mut build = cxx_build::bridge("src/bridge.rs");
    build
        .cpp(true)
        .compiler(&out.link_driver)
        .no_default_flags(true)
        .include(&manifest)
        .file("wrap/view.cpp");
    for flag in &module.cxx {
        build.flag(flag);
    }
    build.compile("atl_service_view");
    for line in converter::link_package(&manifest)? {
        println!("{line}");
    }
    // the adapters and libbinder are libc++ C++ (the converter's STL): its runtime comes last
    println!("cargo::rustc-link-lib=dylib=c++");
    Ok(())
}
