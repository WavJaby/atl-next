// SPDX-License-Identifier: MIT
//! Bindings of the pinned libselinux public headers (`external/selinux/libselinux/include/selinux/{selinux,avc}.h`) for the symbols this crate defines in place of libselinux's kernel-interface files:
//! `struct av_decision`, the id types and every replaced function's declaration, from the header (src/lib.rs checks each export's signature against its declaration), never hand-copied.
//! Parsed in the host's pinned sysroot (arches.toml `[<arch>.host] sysroot`), so no build machine header decides a layout.
use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::buildscript;

/// libselinux's public include dir, relative to `src/`.
const INCLUDE: &str = "aosp/external/selinux/libselinux/include";

fn main() -> Result<()> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = buildscript::Upstreams::of(&checkout)?;
    up.file(&format!("{INCLUDE}/selinux/avc.h"))?;
    let include = up.path(INCLUDE);
    let rust_target = std::env::var("TARGET")?;
    let triple = buildscript::clang_triple_from_cargo()?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let mut builder = bindgen::Builder::default()
        // avc.h declares the status page functions and includes selinux.h
        .header_contents("selinux.h", "#include <selinux/avc.h>\n")
        .clang_arg(format!("-I{}", include.display()))
        .clang_arg(format!("--target={triple}"))
        .allowlist_type("av_decision")
        .allowlist_type("security_class_t")
        .allowlist_type("access_vector_t")
        .allowlist_function("is_selinux_(mls_)?enabled")
        .allowlist_function("security_getenforce")
        .allowlist_function("selinux_status_.*")
        .allowlist_function("security_compute_av(_flags)?(_raw)?")
        .allowlist_function("string_to_security_class|mode_to_security_class|string_to_av_perm")
        .allowlist_function("security_class_to_string|security_av_perm_to_string|security_av_string")
        .allowlist_function("print_access_vector|selinux_flush_class_cache")
        .allowlist_function("(get|set)(con|execcon|fscreatecon|sockcreatecon|keycreatecon)(_raw)?")
        .allowlist_function("get(prevcon|pidcon|pidprevcon)(_raw)?");
    if let Some((args, rerun)) = buildscript::host_sysroot_args(&checkout, &rust_target)? {
        builder = builder.clang_args(args);
        for p in rerun {
            buildscript::rerun_if_changed(&p);
        }
    }
    builder
        .generate()
        .context("generating the libselinux bindings")?
        .write_to_file(out.join("selinux.rs"))
        .context("writing selinux.rs")?;
    buildscript::rerun_if_changed(&checkout.join("arches.toml"));
    for p in up.rerun_paths(INCLUDE) {
        buildscript::rerun_if_changed(&p);
    }
    Ok(())
}
