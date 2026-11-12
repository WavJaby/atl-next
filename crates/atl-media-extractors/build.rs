// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): compiles the cxx glue of src/bridge.rs and the adapter in wrap/extractors.cpp with the C++ argv of `device/libmediaextractorservice`'s own compiles
//! (graph.json `cxx`) and of `device/libbinder_ndk` (AIBinder_fromPlatformBinder's header), then links the service.
//! The service's header sits in its own directory, which the module does not export: that directory is added here, where this checkout reads AOSP (a task copy included).
//! Without the feature nothing: the crate is empty.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::converter::{self, Converter};

/// The converter module whose library the service links, and whose C++ argv the adapter compiles with.
const MES: &str = "device/libmediaextractorservice";
/// Its platform side: AIBinder_fromPlatformBinder's header lives in include_platform, which MES's argv does not carry.
const NDK: &str = "device/libbinder_ndk";
/// MediaExtractorService.h's directory, as an AOSP tree path.
const SERVICE_DIR: &str = "frameworks/av/services/mediaextractor";

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
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let service_dir = format!("aosp/{SERVICE_DIR}");
    for path in up.rerun_paths(&service_dir) {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    let out = Converter::from_env()?;
    let mut build = cxx_build::bridge("src/bridge.rs");
    build
        .cpp(true)
        .compiler(&out.link_driver)
        .no_default_flags(true)
        .include(&manifest)
        .include(up.aosp(SERVICE_DIR))
        .file("wrap/extractors.cpp");
    for module in [MES, NDK] {
        for flag in &out.artifact(module)?.cxx {
            build.flag(flag);
        }
    }
    build.compile("atl_media_extractors");
    for line in converter::link_package(&manifest)? {
        println!("{line}");
    }
    // the adapter and the service are libc++ C++ (the converter's STL): its runtime comes last
    println!("cargo::rustc-link-lib=dylib=c++");
    Ok(())
}
