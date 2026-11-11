// SPDX-License-Identifier: MIT
//! `cargo xtask` -- the single build/check driver of this repo, the bootstrap tool: links only definition crates (no product crate, no build script reading fetched trees), so it builds in a fresh checkout. Read-only or dry-run by default. The binary (main.rs) only calls `cli::main`.
//! The runtime gates (`test`, `run-apk`, `remote`) live in xtask-harness, which links this library and the product; `cli` passes those commands through to it.
#![forbid(unsafe_code)]
// Terminal output is this tool's product, not logging.
#![allow(clippy::print_stdout, clippy::print_stderr)]
#![expect(
    clippy::disallowed_types,
    reason = "a build tool: it runs compilers, git and cargo on the host"
)]

pub mod android_paths;
pub mod aosp;
pub mod apk_sig;
pub mod arches;
pub mod build_pool;
pub mod build_state;
pub mod bwrap_flags;
pub mod cdylib;
pub mod check;
pub mod codec2_store;
pub mod converter;
pub mod cross;
pub mod cts_table;
pub mod deps;
pub mod fetch;
pub mod gate_goals;
pub mod git;
pub mod git_dirs;
pub mod harness;
pub mod host_art;
pub mod host_disk;
pub mod isa_gate;
pub mod keepset;
pub mod land;
pub mod libcxx_gate;
pub mod licence_gate;
pub mod licenses;
pub mod modules;
pub mod native;
pub mod ndk;
pub mod ndk_surface;
pub mod none_gate;
pub mod notice;
pub mod packages;
pub mod par;
pub mod patch;
pub mod provided_gate;
pub mod providers;
pub mod prune;
pub mod qemu;
pub mod runpath_gate;
pub mod runtimes;
pub mod seam_gate;
pub mod service_mirrors;
pub mod slot;
pub mod slot_prune;
pub mod soong;
pub mod sources;
pub mod spdx;
pub mod stage_env;
pub mod system;
pub mod test_apks;
pub mod timing;
pub mod tree;
pub mod triage;
pub mod unit_use;
pub mod upstream;
pub mod verdict;
pub mod workspace;
pub mod ws_config;

pub mod cli;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `cargo` of the pinned toolchain: `CARGO` is set by `cargo run`.
pub fn cargo_command() -> std::process::Command {
    std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// The checkout the command runs in (main repo or worktree): the git toplevel of the cwd. Not the checkout that compiled the binary; a binary built in main also runs in worktrees.
pub fn checkout_root() -> Result<PathBuf> {
    toplevel_in(&std::env::current_dir().context("reading the current directory")?)
}

/// Git toplevel of `dir`. Outside a checkout it is an error, never a fallback to a compiled-in path.
fn toplevel_in(dir: &Path) -> Result<PathBuf> {
    git::git(Some(dir), &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "{} is not inside a checkout: run xtask from the main repo or a worktree",
                dir.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Repo `<tmp>/<name>/main` with worktree `<tmp>/<name>/wt` on branch `task`; canonical paths.
    fn repo_and_worktree(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("xtask-root-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (main, wt) = (base.join("main"), base.join("wt"));
        std::fs::create_dir_all(&main).unwrap();
        let git_in = |dir: &Path, args: &[&str]| git::git(Some(dir), args).unwrap();
        git_in(&main, &["init", "-q", "-b", "main"]);
        git_in(&main, &["config", "user.name", "xtask"]);
        git_in(&main, &["config", "user.email", "xtask@example.invalid"]);
        git_in(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git_in(
            &main,
            &["worktree", "add", "-q", "-b", "task", wt.to_str().unwrap()],
        );
        (main.canonicalize().unwrap(), wt.canonicalize().unwrap())
    }

    #[test]
    fn checkout_root_is_the_toplevel_of_where_it_runs() {
        let (main, wt) = repo_and_worktree("toplevel");
        let sub = wt.join("xtask").join("src");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(toplevel_in(&main).unwrap(), main);
        assert_eq!(toplevel_in(&wt).unwrap(), wt);
        assert_eq!(toplevel_in(&sub).unwrap(), wt);
        std::fs::remove_dir_all(main.parent().unwrap()).unwrap();
    }

    /// Assumes the OS temp dir is not inside a checkout.
    #[test]
    fn outside_a_checkout_is_an_error() {
        let dir = std::env::temp_dir().join(format!("xtask-root-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(toplevel_in(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
