// SPDX-License-Identifier: MIT
//! The runtime harness (crate xtask-harness) as a process: `cargo xtask test|run-apk|remote` and check step `gl-proxies` run its binary.
//! It links the product crates, whose build scripts read fetched trees (bionic headers, native_bridge.h), so it builds only after `xtask fetch`; this tool must not link it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use atl_layout::Layout;

/// The harness package; its binary has the same name.
const PACKAGE: &str = "xtask-harness";

/// Builds the harness (a no-op when current) and returns its binary.
/// The binary is this executable's sibling: both are workspace members built with the same profile into the same target dir.
fn build(root: &Path) -> Result<PathBuf> {
    let status = crate::cargo_command()
        .args(["build", "--quiet", "--package", PACKAGE])
        .current_dir(root)
        .status()
        .with_context(|| format!("spawning cargo build --package {PACKAGE}"))?;
    if !status.success() {
        let fetched = Layout::of(root)?.locate(Path::new("aosp")).dir.is_dir();
        let hint = if fetched {
            ""
        } else {
            " (no fetched AOSP tree: the product crates' build scripts read it; run `cargo xtask fetch --execute` first)"
        };
        bail!("cargo build --package {PACKAGE} exited with {status}{hint}");
    }
    let exe = std::env::current_exe().context("locating the running xtask binary")?;
    let bin = exe.with_file_name(PACKAGE);
    if !bin.is_file() {
        bail!(
            "{} missing after cargo build --package {PACKAGE}",
            bin.display()
        );
    }
    Ok(bin)
}

/// The harness command running `args` in `root`.
pub fn command<I, S>(root: &Path, args: I) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut cmd = Command::new(build(root)?);
    cmd.args(args.into_iter().map(Into::into)).current_dir(root);
    Ok(cmd)
}

/// Runs harness command `name args...` with this process's stdio; its exit code is ours, its verdict line its own.
pub fn pass_through(root: &Path, name: &str, args: &[OsString]) -> Result<ExitCode> {
    let status = command(
        root,
        std::iter::once(OsString::from(name)).chain(args.iter().cloned()),
    )?
    .status()
    .context("spawning xtask-harness")?;
    // a signal leaves no code: report it as a failure, as cargo does
    let code = status.code().unwrap_or(1);
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

/// The line a harness check step prints for "not checked: <why>"; absent = checked.
pub const SKIP_PREFIX: &str = "skip: ";

/// Runs harness check step `step` on slot `out`: `Some(why)` when it skipped, an error with its output when it failed.
pub fn check_step(root: &Path, step: &str, out: &Path) -> Result<Option<String>> {
    let run = command(
        root,
        [
            OsString::from("check-step"),
            step.into(),
            "--out".into(),
            out.into(),
        ],
    )?
    .output()
    .with_context(|| format!("spawning xtask-harness check-step {step}"))?;
    let stdout = String::from_utf8_lossy(&run.stdout);
    if !run.status.success() {
        let stderr = String::from_utf8_lossy(&run.stderr);
        bail!("{}\n{}", stdout.trim_end(), stderr.trim_end());
    }
    Ok(stdout
        .lines()
        .find_map(|l| l.strip_prefix(SKIP_PREFIX))
        .map(str::to_owned))
}
