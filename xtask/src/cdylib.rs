// SPDX-License-Identifier: MIT
//! Workspace libraries a runtime process `dlopen`s by file name through LD_LIBRARY_PATH (`mapper.atl.so`, `libatl_native_bridge.so`): built by plain cargo, no converter link.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

/// Builds cdylib `package` (release, into `<out>/cargo`) and installs it as `<out>/<dir>/<name>`; returns `<out>/<dir>`, for LD_LIBRARY_PATH.
pub fn stage(root: &Path, out: &Path, package: &str, dir: &str, name: &str) -> Result<PathBuf> {
    let status = crate::unit_use::status(
        crate::cargo_command()
            .args(["build", "--release", "-p", package, "--target-dir"])
            .arg(out.join("cargo"))
            .current_dir(root),
    )?;
    ensure!(status.success(), "building {package} failed");
    let dir = out.join(dir);
    std::fs::create_dir_all(&dir)?;
    let built = out.join(format!("cargo/release/lib{}.so", package.replace('-', "_")));
    // a running process maps this file: replace it by rename, never rewrite it in place (that changed pages under a live process)
    let dst = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::copy(&built, &tmp).with_context(|| format!("installing {}", built.display()))?;
    std::fs::rename(&tmp, &dst).with_context(|| format!("installing {}", dst.display()))?;
    Ok(dir)
}
