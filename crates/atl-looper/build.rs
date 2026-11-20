// SPDX-License-Identifier: MIT
//! Generates `OUT_DIR/looper.rs` with atl-bridgegen from NDK `looper.h` (bionic view), twice and failing unless byte-identical: per function its fn-pointer type and the `ALOOPER_*` constants, so a header change fails the build, not a run.

use anyhow::Result;
use atl_bridgegen::buildscript::{HeaderImport, import_headers};

fn main() -> Result<()> {
    import_headers(&[HeaderImport {
        name: "looper",
        source: "frameworks/native/include/android/looper.h",
        also: &[],
        include_dirs: &[],
        defines: &[],
        force_include: None,
        functions: &[
            "ALooper_prepare",
            "ALooper_addFd",
            "ALooper_removeFd",
            "ALooper_pollOnce",
        ],
        types: &[],
        macros: &[],
    }])
}
