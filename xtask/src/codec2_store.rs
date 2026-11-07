// SPDX-License-Identifier: MIT
//! The Codec2 store's soft-codec libraries: every `emplace("lib….so")` in AOSP's `C2Store.cpp`, the list the in-process store's constructor dlopens. A listed library that will not load aborts the process (ComponentModule::init), so every process that can decode needs all of them.
//! Read from the source the build reads ([`tree::build_file`]: the task copy when frameworks/av is edited, else the shared tree), so patches/aosp/frameworks/av 0002's drops are already out of it; nothing here restates the list.
//! Consumers: gate_goals.rs builds the list into each runtime gate's goals (`Goals::codec_store`), native_load.rs breaks one of its libraries on request (`--break-codec`).

use std::path::Path;

use anyhow::{Context, Result, ensure};

use crate::tree;

/// The store source, as [`tree::build_file`] takes it.
pub const SOURCE: &str = "frameworks/av/media/codec2/vndk/C2Store.cpp";

/// The store's libraries by module name (`libcodec2_soft_amrnbdec`), in the source's order. Fails when the source lists none.
pub fn libraries(root: &Path) -> Result<Vec<String>> {
    let path = tree::build_file(root, SOURCE)?;
    let src =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let libs = parse(&src);
    ensure!(
        !libs.is_empty(),
        "{}: no emplace(\"lib….so\") line",
        path.display()
    );
    Ok(libs)
}

/// Module names the store source `src` emplaces; commented-out lines are not listed.
fn parse(src: &str) -> Vec<String> {
    src.lines()
        .filter_map(|l| l.trim().strip_prefix("emplace(\""))
        .filter_map(|rest| rest.split_once(".so\""))
        .map(|(m, _)| m.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commented_lines_are_not_listed() {
        let src = "    emplace(\"libcodec2_soft_a.so\");\n    //emplace(\"libcodec2_soft_b.so\"); // gone\n  emplace(\"libcodec2_soft_c.so\");\n";
        // a and c are emplaced; b's line is a comment
        assert_eq!(parse(src), ["libcodec2_soft_a", "libcodec2_soft_c"]);
    }
}
