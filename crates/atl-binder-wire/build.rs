// SPDX-License-Identifier: MIT
//! Generates the binder uapi (`OUT_DIR/binder.rs`) for the target cargo builds for, from bionic's pinned
//! copy of the kernel header, with atl-bridgegen. Generates twice and fails unless both are
//! byte-identical (the determinism gate runs on every build and every target).
//!
//! Header choice: bionic's `libc/kernel/uapi` copy, pinned by sources.toml with the same AOSP tag as
//! libbinder, so every machine generates from the same bytes. The host libbinder build compiles against
//! the distro's kernel headers instead (providers.toml `libc_uapi_headers`); the binder uapi is a kernel
//! ABI and append-only, so both agree on every layout and code the older of the two defines.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript;

/// Header path inside the fetched upstream tree.
const HEADER: &str = "aosp/bionic/libc/kernel/uapi/linux/android/binder.h";
/// How generated text names it (tree-relative, so output does not depend on the workspace path).
const LABEL: &str = "bionic/libc/kernel/uapi/linux/android/binder.h";
/// Object-like macros the wire layer needs: the handshake version and the `_IOC_SIZE` field.
const MACROS: &[&str] = &[
    "BINDER_CURRENT_PROTOCOL_VERSION",
    "_IOC_SIZESHIFT",
    "_IOC_SIZEMASK",
];

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let workspace = buildscript::workspace(&checkout)?;
    let header = buildscript::upstream_file(&workspace, HEADER)?;
    let arches = checkout.join("arches.toml");
    let rust_target = std::env::var("TARGET")?;
    let host = buildscript::host_arch(&std::fs::read_to_string(&arches)?, &rust_target)?;
    let triple = buildscript::clang_triple_from_cargo()?;
    let aosp = workspace.join("src/aosp");

    let generate = || -> Result<(String, String)> {
        let target = buildscript::bionic_uapi_target(&aosp, &host, &triple);
        let src = atl_bridgegen::Source::c(&header, MACROS);
        let iface = atl_bridgegen::interface("binder", LABEL, &src, &[target])?;
        Ok((
            atl_bridgegen::rust::host_module(&iface, &host.arch)?,
            atl_bridgegen::to_json(&iface)?,
        ))
    };
    let first = generate()?;
    ensure!(
        first == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );

    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("binder.rs"), &first.0).context("writing binder.rs")?;
    // the IR next to it, for review and upgrade diffs
    std::fs::write(out.join("binder.ir.json"), &first.1).context("writing binder.ir.json")?;

    println!("cargo::rerun-if-changed={}", arches.display());
    for dir in buildscript::bionic_uapi_dirs(&aosp, &host) {
        println!("cargo::rerun-if-changed={}", dir.display());
    }
    Ok(())
}
