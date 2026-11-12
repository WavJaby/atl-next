// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): links the converter's libsigchain (Cargo.toml `[package.metadata.atl]`) as the launcher links it, ahead of libc in DT_NEEDED. Without the feature nothing.

use std::path::PathBuf;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    println!("cargo::rerun-if-changed=Cargo.toml");
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR")?);
    for line in atl_bridgegen::converter::link_package(&manifest)? {
        println!("{line}");
    }
    Ok(())
}
