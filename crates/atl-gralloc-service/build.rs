// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): links the converter's libbinder_rs, atl-aidl's stubs and AOSP's allocator crate (Cargo.toml `[package.metadata.atl]`).
//! Without it nothing: the binder front end is not built.

use std::path::PathBuf;

use anyhow::Result;
use atl_bridgegen::converter;

fn main() -> Result<()> {
    // libbinder_rs's declare_binder_interface! tests these cfgs in the expanding crate; unset here, as for a host build
    println!("cargo::rustc-check-cfg=cfg(android_vndk, android_ndk)");
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    for l in converter::link_package(&manifest)? {
        println!("{l}");
    }
    Ok(())
}
