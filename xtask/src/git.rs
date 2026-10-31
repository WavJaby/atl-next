// SPDX-License-Identifier: MIT
//! Running git: one place that reports stderr, and an isolated mode whose output does not depend on
//! the user's git configuration.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

/// `git <args>` (in `dir` if given); stdout trimmed, stderr in the error.
pub fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    run(cmd, args)
}

/// Committer for commits xtask makes in upstream checkouts (`am`, `rebase`).
pub struct Ident {
    pub name: String,
    pub email: String,
}

impl Ident {
    /// The identity configured in the repo checkout (`git config user.name/user.email`).
    pub fn of_repo(repo: &Path) -> Result<Self> {
        let get = |key: &str| {
            git(Some(repo), &["config", key])
                .with_context(|| format!("{key} is not set in {}", repo.display()))
        };
        Ok(Ident {
            name: get("user.name")?,
            email: get("user.email")?,
        })
    }
}

/// `git <args>` in `dir` with global and system config ignored, so the same commits give the same
/// bytes on every machine (format-patch) and commits get `ident` as committer.
pub fn git_isolated(dir: &Path, args: &[&str], ident: Option<&Ident>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(i) = ident {
        cmd.env("GIT_COMMITTER_NAME", &i.name)
            .env("GIT_COMMITTER_EMAIL", &i.email);
    }
    run(cmd, args)
}

/// `git <args>` in `dir`, stdout untrimmed (file contents, e.g. `git show <ref>:<path>`).
pub fn git_raw(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    raw(cmd, args)
}

fn run(cmd: Command, args: &[&str]) -> Result<String> {
    Ok(raw(cmd, args)?.trim().to_owned())
}

fn raw(mut cmd: Command, args: &[&str]) -> Result<String> {
    let out = cmd.args(args).output().context("running git")?;
    ensure!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
