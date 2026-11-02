// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): links the converter's libbinder_rs and the AIDL crate of its `aidl` sources (Cargo.toml `[package.metadata.atl]`).
//! Without it nothing: the library is empty, so plain workspace builds and tests need no converter output.

use std::path::PathBuf;

use anyhow::Result;
use atl_bridgegen::converter;

fn main() -> Result<()> {
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    for l in converter::link_package(&manifest)? {
        println!("{l}");
    }
    Ok(())
}
