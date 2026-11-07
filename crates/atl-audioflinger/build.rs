// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): puts the converter's rlibs (the AIDL stubs and their types, the libbinder_rs crates) on the link line, as atl-resource-manager does. Without the feature nothing: the crate is empty.

use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::converter;

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=Cargo.toml");
    // libbinder_rs's declare_binder_interface! tests these cfgs in the expanding crate; unset here, as for a host build
    println!("cargo::rustc-check-cfg=cfg(android_vndk, android_ndk)");
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR")?);
    for line in converter::link_package(&manifest)? {
        println!("{line}");
    }
    Ok(())
}
