// SPDX-License-Identifier: MIT
//! `cargo xtask` -- the single build/check driver of this repo. Read-only or dry-run by default.
#![forbid(unsafe_code)]
// Terminal output is this tool's product, not logging.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod aosp;
mod arches;
mod check;
mod deps;
mod fetch;
mod git;
mod licenses;
mod modules;
mod native;
mod notice;
mod par;
mod patch;
mod providers;
mod prune;
mod soong;
mod sources;
mod spdx;
mod upstream;
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
    /// Upstream checkouts under <workspace>/src at the pins in sources.toml.
    ///
    /// Dry run by default (offline): shows what each checkout needs. The AOSP manifest (pinned in
    /// sources.toml) is fetched first: it maps tree paths to repositories and holds the tree's link/copy
    /// files; the other checkouts then run in parallel. Checkouts are shallow (the pinned tag only);
    /// `git fetch --unshallow` inside one before bisecting. A checkout on a branch belongs to
    /// `xtask patch`; one with uncommitted changes is never moved.
    Fetch(fetch::Args),
    /// Which AOSP project defines a module, from an index of every manifest project's `Android.bp`.
    ///
    /// `--index` fetches only the `Android.bp` files of every project in the pinned manifest (shallow,
    /// blobless, sparse; under <workspace>/cache/aosp-bp/<tag>, kept between runs) and rebuilds the
    /// index; `--which` reads it. Names come from each module's literal `name:` only: a name built
    /// from variables is not indexed.
    Aosp(aosp::Args),
    /// AOSP modules built by upstream Soong in the pinned subset tree (phase-1 route).
    ///
    /// The product (ATL host arch = AOSP device arch, guest arch = AOSP native-bridge arch) is generated
    /// from arches.toml into <tree>/device/atl/; TARGET_RELEASE follows the tag's BUILD_ID. Output in
    /// <workspace>/out/aosp/<product>, one log per build under out/aosp/logs. With --fetch-missing a
    /// failed build's missing modules and paths are pinned (sources.toml) and fetched, then it builds
    /// again; every added project is printed with the log line that asked for it.
    Soong(soong::Args),
    /// AOSP native modules built for this Linux host by our Blueprint converter (tools/bp2ninja).
    ///
    /// Runtime libraries come from each module's android source set compiled for glibc, build tools
    /// (aidl) from their host source set; bionic itself is replaced by the system C library. Output in
    /// <workspace>/out/native/<host>; failures are summarised per module (`ninja -k 0`).
    Native(native::Args),
    /// Our commits on upstream checkouts: branch atl-patches <-> patches/<upstream>/*.patch.
    ///
    /// Without flags: per upstream, whether checkout, branch and patches/ are in step (read-only;
    /// `xtask check` step `drift` runs the same test). Develop by committing on atl-patches inside the
    /// checkout; every commit carries `Upstream-Status:` and `Patch-Reason:` trailers. After a pin
    /// bump: `xtask fetch`, `--rebase`, resolve, `--export`.
    Patch(patch::Args),
    /// Licence of every source file in a fetched upstream checkout (read-only).
    ///
    /// Per file: a `paths` entry of the upstream in licenses.toml (longest prefix), else its SPDX tag,
    /// else its notice read by fixed phrase rules, else the upstream's `unlabelled` licence when the
    /// file has no notice at all. An unrecognised notice is reported, never guessed. The licence
    /// gate applies the same resolution to what a build links.
    Licence(notice::Args),
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let root = repo_root()?;
    match cli.command {
        Command::Check(args) => check::run(&root, &args),
        Command::Deps(args) => deps::run(&root, &args),
        Command::Fetch(args) => fetch::run(&root, &args),
        Command::Aosp(args) => aosp::run(&root, &args),
        Command::Soong(args) => soong::run(&root, &args),
        Command::Native(args) => native::run(&root, &args),
        Command::Patch(args) => patch::run(&root, &args),
        Command::Licence(args) => notice::run(&root, &args),
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
