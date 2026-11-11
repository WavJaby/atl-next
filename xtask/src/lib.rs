// SPDX-License-Identifier: MIT
//! `cargo xtask` -- the single build/check driver of this repo. Read-only or dry-run by default. The binary (main.rs) only calls `cli::main`.
#![forbid(unsafe_code)]
// Terminal output is this tool's product, not logging.
#![allow(clippy::print_stdout, clippy::print_stderr)]
#![expect(
    clippy::disallowed_types,
    reason = "a build tool: it runs compilers, git and test drivers on the host; the runtime processes of its gates start through atl_broker"
)]

mod android_paths;
mod aosp;
mod apk_sig;
mod app;
mod app_imports;
mod arches;
mod atld_run;
mod audio_app;
mod audio_pcm;
mod binder;
mod broadcast;
mod build_pool;
mod build_state;
mod bwrap_flags;
mod cdylib;
mod check;
mod codec2_store;
mod composer;
mod compositor;
mod converter;
mod cross;
mod crossing;
mod cts;
mod cts_report;
mod cts_table;
mod deps;
mod drive;
mod drive_gate;
mod egl_driver;
mod fetch;
mod gate_goals;
mod gdk_dmabuf;
mod git;
mod git_dirs;
mod gl_dispatch;
mod gl_probe;
mod gl_proxies;
mod gl_scene;
mod gles2_fixture;
mod gralloc;
mod host_art;
mod host_disk;
mod instrument;
mod isa_gate;
mod jni_fixture;
mod keepset;
mod land;
mod libcxx_gate;
mod licence_gate;
mod licenses;
mod manifest;
mod modules;
mod native;
mod native_load;
mod nb_identity;
mod nb_isa;
mod ndk_surface;
mod nested_wayland;
mod none_gate;
mod notice;
mod observe;
mod packages;
mod par;
mod parcel;
mod patch;
mod platform_ui;
mod provided_gate;
mod provider;
mod providers;
mod prune;
mod qemu;
mod qemu_abi;
mod qemu_gate;
mod qemu_gl;
mod qemu_jni;
mod qemu_linker;
mod qemu_spike;
mod qemu_thread;
mod remote;
mod run_apk;
mod runpath_gate;
mod runtimes;
mod sandbox;
mod screencopy;
mod seam_gate;
mod sensors;
mod service_mirrors;
mod services;
mod slot;
mod slot_prune;
mod soong;
mod sources;
mod spdx;
mod system;
mod test_apks;
mod test_fonts;
mod timing;
mod tree;
mod tree_gate;
mod triage;
mod unit_use;
mod upstream;
mod verdict;
mod wayland_proxy;
mod window;
mod workspace;
mod ws_config;

pub mod cli;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `cargo` of the pinned toolchain: `CARGO` is set by `cargo run`.
fn cargo_command() -> std::process::Command {
    std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// The checkout the command runs in (main repo or worktree): the git toplevel of the cwd. Not the checkout that compiled the binary; a binary built in main also runs in worktrees.
fn checkout_root() -> Result<PathBuf> {
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
