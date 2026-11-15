// SPDX-License-Identifier: MIT
//! Workspace libraries a runtime process `dlopen`s by file name through LD_LIBRARY_PATH (`mapper.atl.so`, `libatl_native_bridge.so`): built by plain cargo, no converter link.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

/// Builds cdylib `package` (release, into `<out>/cargo`) and installs it as `<out>/<dir>/<name>`; returns `<out>/<dir>`, for LD_LIBRARY_PATH.
pub fn stage(root: &Path, out: &Path, package: &str, dir: &str, name: &str) -> Result<PathBuf> {
    stage_with(root, out, package, dir, name, &[])
}

/// [`stage`] with each `env` name's inherited value replaced (`None` removes it): a build script's selector (e.g. atl-egl-driver's `ATL_EGL_TIER`).
pub fn stage_with(
    root: &Path,
    out: &Path,
    package: &str,
    dir: &str,
    name: &str,
    env: &[(&str, Option<&str>)],
) -> Result<PathBuf> {
    let (mut cmd, release) = release_build(root, out, package)?;
    for &(k, v) in env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let status = crate::unit_use::status(&mut cmd)?;
    ensure!(status.success(), "building {package} failed");
    let dir = out.join(dir);
    std::fs::create_dir_all(&dir)?;
    let built = release.join(format!("lib{}.so", package.replace('-', "_")));
    // a running process maps this file: replace it by rename, never rewrite it in place (that changed pages under a live process)
    let dst = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::copy(&built, &tmp).with_context(|| format!("installing {}", built.display()))?;
    std::fs::rename(&tmp, &dst).with_context(|| format!("installing {}", dst.display()))?;
    Ok(dir)
}

/// `cargo build --release -p <package>` into `<out>/cargo`, and cargo's output dir of it. A cross host's out dir (native::cross_host_of) builds for that host with cross::cargo_target, as `xtask native` builds its crates: output `<out>/cargo/<triple>/release`.
pub fn release_build(root: &Path, out: &Path, package: &str) -> Result<(Command, PathBuf)> {
    let cross = crate::native::cross_host_of(root, out)?
        .map(|host| crate::cross::cargo_target(root, &host))
        .transpose()?;
    let mut cmd = crate::cargo_command();
    cmd.args(["build", "--release", "-p", package, "--target-dir"])
        .arg(out.join("cargo"))
        .current_dir(root);
    let mut release = out.join("cargo");
    if let Some(c) = &cross {
        cmd.args(["--target", &c.triple])
            .envs(c.env.iter().map(|(k, v)| (k, v)));
        release.push(&c.triple);
    }
    Ok((cmd, release.join("release")))
}
