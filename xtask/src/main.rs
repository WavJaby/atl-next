// SPDX-License-Identifier: MIT
//! `cargo xtask` -- the single build/check driver of this repo. Read-only or dry-run by default.
#![forbid(unsafe_code)]
// Terminal output is this tool's product, not logging.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod arches;
mod check;
mod deps;
mod licenses;
mod modules;
mod providers;
mod sources;
mod spdx;
mod workspace;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

/// Single build/check driver of the ATL-next repo. Read-only or dry-run by default.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Definition of done: every step must pass before a change is handed back or merged.
    ///
    /// Runs every step in order and keeps going after a failure, so one run reports every problem.
    /// Read-only: nothing is reformatted or fixed (`cargo fmt` itself does that).
    Check(check::Args),
    /// Host dependencies from deps.toml.
    ///
    /// Distro rows: package installed (dpkg-query / pacman), `min` met (pkg-config for libraries,
    /// package version otherwise). Cargo rows: exact version in `cargo install --list`. Read-only.
    Deps(deps::Args),
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let root = repo_root()?;
    match cli.command {
        Command::Check(args) => check::run(&root, &args),
        Command::Deps(args) => deps::run(&root, &args),
    }
}

/// Subcommand names, the targets of `xtask:<name>` references in deps.toml.
pub fn subcommands() -> BTreeSet<String> {
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect()
}

/// `cargo` of the pinned toolchain: `CARGO` is set by `cargo run`.
pub fn cargo_command() -> std::process::Command {
    std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// The checkout being worked on (main repo or a worktree), i.e. the parent of `xtask/`.
fn repo_root() -> Result<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(PathBuf::from)
        .context("xtask manifest dir has no parent")
}
