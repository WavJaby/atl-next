// SPDX-License-Identifier: MIT
//! With feature `converter` (`xtask native` turns it on): links the converter's libbinder_rs (Cargo.toml `[package.metadata.atl] converter`) and generates the Rust stubs of `INTERFACES` with the converter's host aidl into `OUT_DIR/aidl.rs` (atl-bridgegen converter.rs `aidl_rust`: each root and every file it imports).
//! Without it nothing: the library is empty, so plain workspace builds and tests need no converter output.

use std::path::PathBuf;

use anyhow::Result;
use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::converter;

/// The converter's host aidl, as graph.json names it: libbinder compiles its own AIDL with it, so every closure holding libbinder_rs has it.
const AIDL: &str = "host/aidl";

/// (import root in the AOSP tree, root files below it): libbinder's AIDL, IServiceManager and what it imports.
const INTERFACES: &[(&str, &[&str])] = &[(
    "frameworks/native/libs/binder/aidl",
    &["android/os/IServiceManager.aidl"],
)];

fn main() -> Result<()> {
    if std::env::var_os("CARGO_FEATURE_CONVERTER").is_none() {
        return Ok(());
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let (conv, lines) = converter::link_package(&manifest)?;
    for l in lines {
        println!("{l}");
    }
    let up = Upstreams::of(&buildscript::checkout(&manifest)?)?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let mut rerun = Vec::new();
    for (dir, roots) in INTERFACES {
        rerun.extend(converter::aidl_rust(
            conv.tool(AIDL)?,
            &up.aosp(dir),
            roots,
            &out,
        )?);
        rerun.extend(up.rerun_paths(&format!("aosp/{dir}")));
    }
    for p in rerun {
        println!("cargo::rerun-if-changed={}", p.display());
    }
    Ok(())
}
