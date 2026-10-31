// SPDX-License-Identifier: MIT
//! Generates, with atl-bridgegen from AOSP's pinned `debuggerd/client.h` for the target cargo builds for:
//! `OUT_DIR/capi/include/debuggerd/client.h` (declarations of exactly what `crate::ffi` exports, and the
//! `"dump_type.h"` include whose `DebuggerdDumpType` its callers name; cargo-c installs it) and
//! `OUT_DIR/client.rs` (the check that each export has the header's signature).
//!
//! The header is C++ with C++ linkage; the printed one declares the exports `extern "C"`, which is what
//! this crate defines: callers compile against the printed header only. `enum DebuggerdDumpType :
//! uint8_t` travels as its underlying type.

use anyhow::Result;
use atl_bridgegen::Lang;
use atl_bridgegen::buildscript::{self, Api};
use atl_bridgegen::c::Kept;

/// What the native closure calls (2026-10-31: libandroid_runtime android_os_Debug.cpp
/// `dumpTraces`; `debuggerd_trigger_dump` / `dump_backtrace_to_file` have no caller there).
const APIS: &[Api] = &[Api {
    name: "client",
    source: "system/core/debuggerd/include/debuggerd/client.h",
    lang: Lang::Cxx,
    header: Some("debuggerd/client.h"),
    functions: &["dump_backtrace_to_file_timeout"],
    macros: &[],
    // its <android-base/unique_fd.h> and "dump_type.h" (libdebuggerd_common_headers, which the
    // module re-exports to its dependents)
    include_dirs: &[
        "system/libbase/include",
        "system/core/debuggerd/common/include",
    ],
    kept: Kept {
        includes: &["\"dump_type.h\""],
        macros: &[],
        types: &[],
    },
}];

fn main() -> Result<()> {
    for path in buildscript::api_crate(APIS, "crate::ffi")? {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    Ok(())
}
