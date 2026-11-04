// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): compiles the cxx glue of src/bridge.rs and the adapters in wrap/ with the C++ argv of `device/libinput`'s own compiles (graph.json `cxx`): libc++, device defines, include paths, so the adapters see AOSP's class layouts.
//! The compiler is the converter's link driver. The static adapter library is emitted before the converter's libinput.so, so `--as-needed` keeps the shared library.
//! Without the feature nothing: the crate is empty and its bin and test are not built.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::converter::{self, Converter};

/// The converter module whose C++ argv the adapters compile with and whose shared library they link.
const LIBINPUT: &str = "device/libinput";

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
    let module = out.artifact(LIBINPUT)?;
    let mut build = cxx_build::bridge("src/bridge.rs");
    build
        .cpp(true)
        .compiler(&out.link_driver)
        .no_default_flags(true)
        .include(&manifest)
        .file("wrap/input.cpp");
    for flag in &module.cxx {
        build.flag(flag);
    }
    build.compile("atl_libinput");
    for line in converter::link_package(&manifest)? {
        println!("{line}");
    }
    // the adapters and libinput are libc++ C++ (the converter's STL): its runtime comes last, after the modules that need it
    println!("cargo::rustc-link-lib=dylib=c++");
    Ok(())
}
