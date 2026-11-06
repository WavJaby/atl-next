// SPDX-License-Identifier: MIT
//! Links the converter's rlibs (libbinder_rs, the codecs, the AIDL stubs) when feature `converter` is on; without it only the plain-Rust modules build.

use std::path::PathBuf;

use anyhow::Result;
use atl_bridgegen::converter;

fn main() -> Result<()> {
    // libbinder_rs's declare_binder_interface! tests these cfgs in the expanding crate (libs/binder/rust/src/binder.rs); unset here, as for a host build
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
