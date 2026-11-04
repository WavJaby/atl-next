// SPDX-License-Identifier: MIT
//! The virtual keyboard's KeyCharacterMap, staged at build time by `xtask native` (rung 2 of "AOSP's own code first": the environment supplies the file). AOSP's `validatekeymaps` (patches/aosp/frameworks/base 0004, `--parcel-out`) loads `data/keyboards/Virtual.kcm` (type FULL) with libinput's own loader and writes the map's Parcel form; the input service serves those bytes verbatim with every InputDevice (atl-services input.rs). No Rust parses or writes the map.
//! The tool runs with the .kcm's directory as cwd and the bare name, so the load file name serialized into the Parcel is `Virtual.kcm` on every host and the staged bytes do not depend on the checkout's path.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

use crate::farm;

/// The tool as `xtask native` builds it (goal `validatekeymaps`, a cc_binary_host).
const TOOL: &str = "device/validatekeymaps/validatekeymaps";
/// The keyboards directory of the AOSP tree, and the map loaded from it.
const KEYBOARDS: &str = "frameworks/base/data/keyboards";
const KCM: &str = "Virtual.kcm";

/// Where the staged Parcel form lives below the native out directory.
pub fn staged(out: &Path) -> PathBuf {
    out.join("keychars/Virtual.kcm.parcel")
}

/// Runs the built tool on Virtual.kcm and writes its Parcel form to [`staged`]. A missing tool or a refused map is an error naming it.
pub fn stage(layout: &Layout, out: &Path) -> Result<PathBuf> {
    let tool = out.join(TOOL);
    ensure!(
        tool.is_file(),
        "{} missing: `xtask native` builds it from the goal validatekeymaps",
        tool.display()
    );
    let dir = farm::shared_tree(layout).join(KEYBOARDS);
    let target = staged(out);
    std::fs::create_dir_all(target.parent().context("staged map has no directory")?)?;
    let status = Command::new(&tool)
        .current_dir(&dir)
        .args(["-q", "--parcel-out"])
        .arg(&target)
        .arg(KCM)
        .status()
        .with_context(|| format!("running {}", tool.display()))?;
    ensure!(
        status.success(),
        "{} refused {KCM} ({status})",
        tool.display()
    );
    Ok(target)
}
