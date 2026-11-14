// SPDX-License-Identifier: MIT
//! `cargo xtask cross --arch <name> [--execute] -- <cargo args>`: runs cargo for another arch of arches.toml.
//! The Rust target, the ISA flags (`rustc -C target-feature`), the link driver (the `clang` deps row's binary)
//! and the sysroot (a `sysroot_debs` row, fetched by `xtask deps --fetch`) all come from that arch's
//! `[<arch>.host]` row, so no arch name or flag is written here. Dry run by default: prints the environment and the command.
//! `--target` goes right after the cargo subcommand, so harness arguments after a `--` in the cargo arguments stay theirs.
//! Test binaries land in `target/<triple>/debug/deps/` of the checkout.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};

use crate::unit_use::{self, Echo, Units};
use crate::{arches, deps};

#[derive(clap::Args)]
pub struct Args {
    /// The arch of arches.toml to build for: its `[<arch>.host]` row names the target.
    #[arg(long)]
    arch: String,
    /// Runs cargo. Without it: prints the environment and the command.
    #[arg(long)]
    execute: bool,
    /// Cargo arguments, subcommand first: `test --no-run -p atl-layout`.
    #[arg(last = true, required = true)]
    cargo: Vec<String>,
}

/// The environment cargo runs with, and its arguments.
struct Plan {
    env: Vec<(String, String)>,
    args: Vec<String>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let plan = plan(root, &args.arch, &args.cargo)?;
    for (key, value) in &plan.env {
        println!("{key}={value}");
    }
    println!("cargo {}", plan.args.join(" "));
    if !args.execute {
        println!("dry run: --execute runs it");
        return Ok(ExitCode::SUCCESS);
    }
    let mut cargo = crate::cargo_command();
    cargo.current_dir(root).args(&plan.args);
    for (key, value) in &plan.env {
        cargo.env(key, value);
    }
    // only the build-like subcommands take --message-format; the others run unrecorded
    let units = match plan.args.first().map(String::as_str) {
        Some("build" | "check" | "clippy" | "run" | "test" | "bench") => Units::Recorded,
        _ => Units::Plain,
    };
    let status = unit_use::run(&mut cargo, units, Echo::Both)
        .context("running cargo")?
        .status;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// A cross host's cargo target: its rust triple, and the environment cargo runs for it (linker, RUSTFLAGS).
pub struct CargoTarget {
    pub triple: String,
    pub env: Vec<(String, String)>,
}

/// The cargo target of cross host `arch`, once its rows are satisfied. `cross` runs cargo with it; `native` builds the host's crates with it.
pub fn cargo_target(root: &Path, arch: &str) -> Result<CargoTarget> {
    let arches = arches::load(root)?;
    let host = arches
        .get(arch)
        .and_then(|a| a.host.as_ref())
        .with_context(|| format!("{}: no [{arch}.host] row", arches::FILE))?;
    let (Some(isa), Some(clang), Some(sysroot), Some(cc)) =
        (&host.isa, &host.clang, &host.sysroot, &host.cc)
    else {
        bail!(
            "{}: [{arch}.host] has no isa, clang, sysroot and cc: not a cross target",
            arches::FILE
        );
    };
    // the cross rows (sysroot, rust std) first: a missing one names its fetch or `rustup target add` command
    let problems = deps::host_rows_problems(root, arch)?;
    ensure!(
        problems.is_empty(),
        "cross host {arch}: {}",
        problems.join("; ")
    );
    let sysroot = deps::sysroot_root(root, sysroot)?;
    let [lib, _] = sysroot_lib_dirs(&sysroot, clang);
    ensure!(lib.is_dir(), "sysroot has no {}", lib.display());
    // RUSTFLAGS is split on whitespace: a path with a space would reach the linker as two arguments.
    ensure!(
        !sysroot.to_string_lossy().contains(char::is_whitespace),
        "sysroot path {} has whitespace",
        sysroot.display()
    );
    let rustflags = rustflags(
        &arches::rustc_target_features(isa)?,
        clang,
        &host.cflags,
        &sysroot,
        &lib,
    );
    // the host's own compiler links its crates, as it links the converter's modules
    let driver = deps::binary(root, cc)?;
    Ok(CargoTarget {
        triple: host.rust.clone(),
        env: vec![
            (env_key(&host.rust, "LINKER"), driver),
            (env_key(&host.rust, "RUSTFLAGS"), rustflags),
        ],
    })
}

fn plan(root: &Path, arch: &str, cargo: &[String]) -> Result<Plan> {
    let target = cargo_target(root, arch)?;
    Ok(Plan {
        args: with_target(cargo, &target.triple)?,
        env: target.env,
    })
}

/// `CARGO_TARGET_<TRIPLE>_<SUFFIX>`: cargo's per-target environment key.
fn env_key(triple: &str, suffix: &str) -> String {
    format!(
        "CARGO_TARGET_{}_{suffix}",
        triple.to_ascii_uppercase().replace(['-', '.'], "_")
    )
}

/// A sysroot's library dirs for the clang triple `clang`: `usr/<clang>/lib` (glibc's runtime; cargo_target's -L) and `usr/lib/<clang>` (the multiarch dir). isa_gate resolves DT_NEEDED names in both.
pub fn sysroot_lib_dirs(sysroot: &Path, clang: &str) -> [PathBuf; 2] {
    [
        sysroot.join("usr").join(clang).join("lib"),
        sysroot.join("usr").join("lib").join(clang),
    ]
}

/// The RUSTFLAGS of a cross link: the ISA features for rustc (arches::rustc_target_features), then the clang triple, the row's cflags (`-mno-relax` reaches lld as --no-relax), lld and the sysroot for the linker.
fn rustflags(features: &str, clang: &str, cflags: &[String], sysroot: &Path, lib: &Path) -> String {
    let extra: String = cflags.iter().map(|f| format!(" -C link-arg={f}")).collect();
    format!(
        "-C target-feature={features} -C link-arg=--target={clang}{extra} -C link-arg=-fuse-ld=lld -C link-arg=--sysroot={} -C link-arg=-L{}",
        sysroot.display(),
        lib.display()
    )
}

/// `cargo <subcommand> ...` with `--target <triple>` inserted after the subcommand.
fn with_target(cargo: &[String], triple: &str) -> Result<Vec<String>> {
    ensure!(
        !cargo
            .iter()
            .any(|a| a == "--target" || a.starts_with("--target=")),
        "cross sets --target from arches.toml; drop it from the cargo arguments"
    );
    let mut args = cargo.to_vec();
    args.insert(1, triple.to_owned());
    args.insert(1, "--target".to_owned());
    Ok(args)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn env_keys_follow_cargo_naming() {
        assert_eq!(
            env_key("riscv64gc-unknown-linux-gnu", "LINKER"),
            "CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER"
        );
    }

    #[test]
    fn rustflags_carry_isa_and_link_driver() {
        let flags = rustflags(
            "+zba,+zbb",
            "riscv64-linux-gnu",
            &["-mno-relax".to_owned()],
            Path::new("/s"),
            Path::new("/s/usr/riscv64-linux-gnu/lib"),
        );
        assert_eq!(
            flags,
            "-C target-feature=+zba,+zbb -C link-arg=--target=riscv64-linux-gnu -C link-arg=-mno-relax -C link-arg=-fuse-ld=lld -C link-arg=--sysroot=/s -C link-arg=-L/s/usr/riscv64-linux-gnu/lib"
        );
    }

    #[test]
    fn target_goes_after_the_subcommand() {
        let cargo: Vec<String> = ["test", "--no-run", "-p", "x", "--", "--ignored"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            with_target(&cargo, "T").unwrap(),
            [
                "test",
                "--target",
                "T",
                "--no-run",
                "-p",
                "x",
                "--",
                "--ignored"
            ]
        );
        assert!(with_target(&["build".to_owned(), "--target=T".to_owned()], "T").is_err());
    }
}
