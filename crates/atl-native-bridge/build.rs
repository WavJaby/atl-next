// SPDX-License-Identifier: MIT
//! Generates with atl-bridgegen, from AOSP's `nativebridge/native_bridge.h` parsed in the host's (glibc) view -- the NB core lives in ART's process -- `OUT_DIR/native_bridge.rs`:
//! `NativeBridgeCallbacks` and the records it passes with clang's sizes and offsets asserted, and its entry shells (`Table`): one `unsafe extern "C"` function per field with the header's exact signature, panic-shielded, calling `crate::exports::<field>`, plus `NativeBridgeCallbacks::shells()` with `version` = `crate::exports::VERSION` (bridge controls 1 and 2, plan S11).
//! A field added by an AOSP upgrade becomes a shell calling a function `exports` lacks: a compile error, not a silent null entry.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use atl_bridgegen::buildscript::{self, Upstreams};
use atl_bridgegen::ir::Ty;
use atl_bridgegen::rust::{Table, api_module_with_table};
use atl_bridgegen::{Lang, Source, Target};

/// The interface header, relative to the AOSP tree.
const HEADER: &str = "art/libnativebridge/include/nativebridge/native_bridge.h";
/// Where its `#include "jni.h"` resolves (libnativebridge's `export_header_lib_headers: ["jni_headers"]`).
const INCLUDE_DIRS: &[&str] = &["libnativehelper/include_jni"];

/// What a panic in an entry answers: the failure value each entry documents (native_bridge.h) -- false, null, or for `unloadLibrary` non-zero.
fn on_panic(field: &str, ret: &Ty) -> Result<String> {
    Ok(match (field, ret.resolved()) {
        ("unloadLibrary", _) => "-1",
        // a fn-pointer return: no handler
        ("getSignalHandler", _) => "None",
        (_, Ty::Pointer { is_const: true, .. }) => "::core::ptr::null()",
        (_, Ty::Pointer { .. }) => "::core::ptr::null_mut()",
        (_, Ty::Bool) => "false",
        _ => anyhow::bail!("{field}: no panic answer for {ret:?}"),
    }
    .to_owned())
}

fn main() -> Result<()> {
    // `--cfg atl_g1_break`: the deliberate break of spike gate g1 (engine::x86_64: the host->guest crossing without its FS restore)
    println!("cargo::rustc-check-cfg=cfg(atl_g1_break)");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let checkout = buildscript::checkout(&manifest)?;
    let up = Upstreams::of(&checkout)?;
    let arches = checkout.join("arches.toml");
    let host = buildscript::host_arch(
        &std::fs::read_to_string(&arches)?,
        &std::env::var("TARGET")?,
    )?;
    let path = up.file(&format!("aosp/{HEADER}"))?;
    let target = Target {
        arch: host.arch.clone(),
        triple: buildscript::clang_triple_from_cargo()?,
        args: INCLUDE_DIRS
            .iter()
            .map(|d| format!("-I{}", up.aosp(d).display()))
            .collect(),
    };
    let generate = || -> Result<String> {
        let src = Source {
            path: &path,
            // C: the header's `namespace android { extern "C" {` is `#ifdef __cplusplus`, so C sees the same records at top level with C linkage and the same layout
            lang: Lang::C,
            also: &[],
            macros: &[],
        };
        let iface =
            atl_bridgegen::interface("native_bridge", HEADER, &src, std::slice::from_ref(&target))
                .context(HEADER)?;
        api_module_with_table(
            &iface,
            &host.arch,
            &[],
            &["NativeBridgeCallbacks"],
            "crate::exports",
            &Table {
                record: "NativeBridgeCallbacks",
                implementation: "crate::exports",
                on_panic: &on_panic,
                values: &[("version", "crate::exports::VERSION")],
            },
        )
    };
    let code = generate()?;
    ensure!(
        code == generate()?,
        "atl-bridgegen output differs between two runs on the same input"
    );
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("native_bridge.rs"), code)?;
    println!("cargo::rerun-if-changed={}", arches.display());
    for rel in std::iter::once(&HEADER).chain(INCLUDE_DIRS) {
        for p in up.rerun_paths(&format!("aosp/{rel}")) {
            println!("cargo::rerun-if-changed={}", p.display());
        }
    }
    Ok(())
}
