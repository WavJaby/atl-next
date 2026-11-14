// SPDX-License-Identifier: MIT
//! The command line: subcommands, their dispatch and verdict names.

use std::collections::BTreeSet;
use std::process::ExitCode;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

use crate::*;

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
    /// Sysroot rows (`sysroot_debs`): `--fetch NAME` downloads and unpacks them without root; dry run without `--execute`.
    Deps(deps::Args),
    /// Runs cargo for another arch of arches.toml (riscv64 today): target, ISA flags, link driver and sysroot from its rows.
    ///
    /// Dry run by default: prints the environment and the cargo command. `--execute` runs it. The sysroot must be fetched first (`deps --fetch`).
    Cross(cross::Args),
    /// Static gate over a cross host's ELF outputs: each riscv64 ELF's Tag_RISCV_arch within the row's isa and its GLIBC needs within glibc_floor; each product ELF RISC-V and each build ELF the build machine's; each product DT_NEEDED resolved (isa_gate.rs).
    IsaGate(isa_gate::Args),
    /// Upstream checkouts under <workspace>/src at the pins in sources.toml. `--test-apks` fetches the test APKs of test-apks.toml instead.
    ///
    /// Dry run by default (offline): shows what each checkout needs. The AOSP manifest (pinned in
    /// sources.toml) is fetched first: it maps tree paths to repositories and holds the tree's link/copy
    /// files; the other checkouts then run in parallel. Checkouts are shallow (the pinned tag only);
    /// `git fetch --unshallow` inside one before bisecting.
    /// An upstream with patches/<upstream>/ ends on branch atl-patches = pin + those patches; commits on atl-patches that patches/ lacks are never dropped (fetch refuses and names the command to run), nor is a checkout with uncommitted changes moved.
    /// src/ holds origin/main's pins and patches: a shared checkout is changed only where this repo checkout's pin and patches/<upstream>/ equal origin/main's; elsewhere fetch refuses, and a task's own go to its task copy (`xtask patch --edit`), which fetch settles instead.
    Fetch(fetch::Args),
    /// Which AOSP project defines a module, from an index of every manifest project's `Android.bp`.
    ///
    /// `--index` fetches only the `Android.bp` files of every project in the pinned manifest (shallow,
    /// blobless, sparse; under <workspace>/cache/aosp-bp/<tag>, kept between runs) and rebuilds the
    /// index; `--which` reads it. Names come from each module's literal `name:` only: a name built
    /// from variables is not indexed.
    Aosp(aosp::Args),
    /// The `[aosp.sparse]` keep-sets derived from the build graphs: each project's reads, the minimal dir cover, and rows checked against them (`--check`) or written (`--write --project`).
    ///
    /// Reads come from every out dir's build.ninja, deps log and graph.json, the Android.bp of the modules they name, `cts.toml` rows and `arches.toml` host rows. Read-only unless `--write`; a checkout changes only through `xtask fetch --recreate`.
    Keepset(keepset::Args),
    /// AOSP native modules built for this Linux host by our Blueprint converter (tools/bp2ninja).
    ///
    /// Runtime libraries come from each module's android source set compiled for glibc, build tools
    /// (aidl) from their host source set; bionic itself is replaced by the system C library. Output in
    /// this checkout's build-output slot (`xtask slot`), else <workspace>/out/native/<host>; failures are summarised per module (`ninja -k 0`) against the goals: which failed modules each goal reaches, those with no failed dependency first (most modules waiting on them first), each with its cause (compiler `error:` lines, a link's undefined symbols).
    /// Gates over what was built, each failing the run: the provided gate (provided libraries win symbol lookup) and the licence gate (every source and header linked into a device artifact has a licence licenses.toml allows in the runtime).
    /// Preflight: every AOSP checkout and every upstream built for providers.toml must be its pin + patches/ (the drift test), since the build compiles checkouts as they are and never writes patches/.
    /// Task copies (`xtask patch --edit`) replace their src/ checkout. With AOSP ones, `native` and `check` re-execute inside a mount namespace (`bwrap --dev-bind / /`) where each is bind-mounted over src/aosp/<project>, so the tree path is always src/aosp; the slot's `built-from` record names each bound copy and its state.
    /// Risk: `ninja` run by hand outside the namespace reads the unpatched src/aosp; `native --shell <goal> --host <h>` opens $SHELL inside it.
    Native(native::Args),
    /// The key set product APKs are signed with (keys.rs): per Soong certificate role (`platform`, `testkey`) `<role>.x509.pem` + `<role>.pk8`.
    ///
    /// Default dir: the workspace's dev set <workspace>/keys, shared by every slot; `native` signs with it unless given `--keys`. Never AOSP's published keys, never committed.
    /// Dry run by default: lists the roles present and missing. `--execute` makes the missing ones with openssl as AOSP's make_key does.
    Keys(keys::Args),
    /// The pinned QEMU's user-mode executable for a guest arch (`qemu-<target>`), built with no patch.
    ///
    /// Dry run unless `--execute`: then QEMU's own `configure` and ninja build `qemu-<target>` into `<out>/qemu/<target>-linux-user`, and its `--version` must name the sources.toml pin.
    Qemu(qemu::Args),
    /// Build-output slots: the out dir each checkout builds into by default (`show`, `list`, `assign`).
    ///
    /// The pool is `build.slots` dirs <workspace>/out/slot-<n> (<workspace>/workspace.toml, default 3; `list` prints the effective limits); a checkout holds at most one, recorded in its git dir. `native`, `check` and `test` build into it without `--out` / `--java-out`. A slotted checkout's `target/` links to the slot's `target/`, so its cargo builds go there too.
    Slot {
        #[command(subcommand)]
        slot: slot::Slot,
    },
    /// Our commits on upstream checkouts: patch branch -> patches/<upstream>/*.patch.
    ///
    /// Without flags: per upstream, whether checkout, branch and patches/ are in step (read-only; `xtask check` step `drift` and `xtask native` run the same test). Untracked files count, except those xtask puts there (manifest link/copy files, prune markers).
    /// `xtask fetch --execute` applies patches/. Develop on a task branch: `--edit <upstream>` makes the task copy src-wt/<task>/<upstream> on atl-patches-<task>; commit there, every commit with `Upstream-Status:` and `Patch-Reason:` trailers, then `--export`; `--done` removes the copy (`--task <branch>` from main).
    /// After a pin bump `xtask fetch --execute` re-applies patches/ on the new pin; where they no longer apply: `--rebase`, resolve, `--export`.
    Patch(patch::Args),
    /// Lands a task branch on origin/main, after the branch's own patches pass the full check.
    ///
    /// Dry run by default: prints the steps after a read-only preflight (the branch is a fast-forward of origin/main; the main checkout is on origin/main, clean and in step; the branch's worktree is clean).
    /// `--execute` runs them: task copies for the upstreams whose patches/ or pin differ from origin/main's, `cargo xtask check` in the branch's worktree, push, `fetch --execute` in the main checkout, `cargo xtask check` there, then removal of the copies, the checkout's target, worktree and branch.
    /// Nothing is pushed unless the branch's check passes. `--execute` fetches origin first; a dry run judges origin/main as last fetched.
    Land(land::Args),
    /// Licence of every source file in a fetched upstream checkout (read-only).
    ///
    /// Per file: a `paths` entry of the upstream in licenses.toml (longest prefix), else its SPDX tag,
    /// else its notice read by fixed phrase rules, else the upstream's `unlabelled` licence when the
    /// file has no notice at all. An unrecognised notice is reported, never guessed. The licence
    /// gate applies the same resolution to what a build links.
    Licence(notice::Args),
    /// The licence gate as build.ninja's edge over the device link outputs (licence_gate.rs): ninja runs it, `native` reads its report.
    #[command(hide = true)]
    LicenceGate(licence_gate::EdgeArgs),
    /// Churn of the AOSP code the Rust system services mirror (services.toml) between two AOSP revisions: what a tag bump must be reviewed against.
    ///
    /// Per service, most churn first; per entry lines added + deleted, of the whole file or of the named declaration's body (overloads together), GONE when it vanished. Read-only.
    /// The src/aosp checkouts are shallow (pinned tag only): fetch the other tag there blobless, or pass `--repo <project>=<git dir>` holding both; a missing revision fails with the command it needs.
    UpgradeDiff(service_mirrors::Args),
    /// Runs a test APK row (test-apks.toml), or an APK file given by path, under atld the way an app is (xtask-harness; `run-apk --help` for its options).
    #[command(disable_help_flag = true)]
    RunApk(Passthrough),
    /// Runs WSL-built outputs on a Linux desktop over ssh (xtask-harness; `remote --help` for its commands).
    #[command(disable_help_flag = true)]
    Remote(Passthrough),
    /// Runtime tests over build outputs (xtask-harness; `test --help` lists them). Not `cargo test`: they need built AOSP artifacts and ART.
    #[command(disable_help_flag = true)]
    Test(Passthrough),
}

/// A harness command's arguments, passed through unparsed: the harness owns their syntax, help and verdict line.
#[derive(clap::Args)]
struct Passthrough {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<std::ffi::OsString>,
}

/// Parses the command line and runs the subcommand; the binary's whole `main`.
pub fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let root = checkout_root()?;
    let name = verdict_name(&cli.command);
    // the commands that read src/aosp see the task copies through it (tree.rs)
    let shell = match &cli.command {
        Command::Native(a) => Some(a.shell),
        Command::Check(_) => Some(false),
        _ => None,
    };
    if let Some(shell) = shell {
        let overlays = tree::overlays_of(&root)?;
        // `native --shell`: the shell gets the compiler-cache variables of its own source view, as each ninja run does
        let env = match &cli.command {
            Command::Native(a) if a.shell => {
                native::shell_env(a, &root, &tree::describe(&overlays))?
            }
            _ => Vec::new(),
        };
        if let Some(code) = tree::enter(&overlays, shell, &env)? {
            return Ok(code);
        }
    }
    // commands whose cargo builds write the checkout's target dir: their stale incremental sessions go when they end (slot::after_build)
    let builds_trees = matches!(
        cli.command,
        Command::Native(_)
            | Command::Check(_)
            | Command::Cross(_)
            | Command::Test(_)
            | Command::RunApk(_)
            | Command::Remote(_)
    );
    let result = match cli.command {
        Command::Check(args) => check::run(&root, &args),
        Command::Deps(args) => deps::run(&root, &args),
        Command::Cross(args) => cross::run(&root, &args),
        Command::IsaGate(args) => isa_gate::run(&root, &args),
        Command::Fetch(args) => fetch::run(&root, &args),
        Command::Aosp(args) => aosp::run(&root, &args),
        Command::Keepset(args) => keepset::run(&root, &args),
        Command::Native(args) => native::run(&root, &args),
        Command::Keys(args) => keys::run(&root, &args),
        Command::Qemu(args) => qemu::run(&root, &args),
        Command::Slot { slot } => slot::run(&root, &slot),
        Command::Patch(args) => patch::run(&root, &args),
        Command::Land(args) => land::run(&root, &args),
        Command::Licence(args) => notice::run(&root, &args),
        Command::LicenceGate(args) => licence_gate::run_edge(&root, &args),
        Command::UpgradeDiff(args) => {
            let layout = atl_layout::Layout::of(&root)?;
            service_mirrors::upgrade_diff(&root, &args, &|project| {
                layout
                    .locate(&std::path::Path::new("aosp").join(project))
                    .dir
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Command::RunApk(p) => harness::pass_through(&root, "run-apk", &p.args),
        Command::Remote(p) => harness::pass_through(&root, "remote", &p.args),
        Command::Test(p) => harness::pass_through(&root, "test", &p.args),
    };
    let result = if builds_trees {
        slot::after_build(&root, result)
    } else {
        result
    };
    // a run that ends in an error (`?`, bail!) still ends with its verdict line; main prints the error after it
    if let (Err(e), Some(name)) = (&result, name) {
        println!("{}", verdict::error(name, e));
    }
    result
}

/// The verdict-line name of a command whose failures end in a verdict line; None for the others.
fn verdict_name(command: &Command) -> Option<&'static str> {
    Some(match command {
        Command::Native(_) => "native",
        Command::Qemu(_) => "qemu",
        Command::Check(_) => "check",
        Command::Land(_) => "land",
        _ => return None,
    })
}

/// Subcommand names, the targets of `xtask:<name>` references in deps.toml.
pub fn subcommands() -> BTreeSet<String> {
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_owned())
        .collect()
}
