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
    let (Some(isa), Some(clang), Some(sysroot)) = (&host.isa, &host.clang, &host.sysroot) else {
        bail!(
            "{}: [{arch}.host] has no isa, clang and sysroot: not a cross target",
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
    let lib = sysroot.join("usr").join(clang).join("lib");
    ensure!(lib.is_dir(), "sysroot has no {}", lib.display());
    // RUSTFLAGS is split on whitespace: a path with a space would reach the linker as two arguments.
    ensure!(
        !sysroot.to_string_lossy().contains(char::is_whitespace),
        "sysroot path {} has whitespace",
        sysroot.display()
    );
    let features: Vec<String> = arches::isa_extensions(isa)?
        .iter()
        .map(|e| format!("+{e}"))
        .collect();
    let rustflags = rustflags(&features, clang, &sysroot, &lib);
    let driver = deps::binary(root, "clang")?;
    // bindgen's target-scoped extra clang args: atl-bridgegen's probes read them (front.rs args), so build scripts parse the target's headers, not the build machine's
    let libcxx = libcxx_dir(&sysroot)?;
    let probe_args = format!(
        "--sysroot={} -nostdinc++ -isystem {}",
        sysroot.display(),
        libcxx.display()
    );
    Ok(CargoTarget {
        triple: host.rust.clone(),
        env: vec![
            (env_key(&host.rust, "LINKER"), driver),
            (env_key(&host.rust, "RUSTFLAGS"), rustflags),
            (bindgen_key(&host.rust), probe_args),
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

/// `BINDGEN_EXTRA_CLANG_ARGS_<triple>`: bindgen's target-scoped extra clang args (`-` as `_`, the key bindgen itself looks up).
fn bindgen_key(triple: &str) -> String {
    format!("BINDGEN_EXTRA_CLANG_ARGS_{}", triple.replace('-', "_"))
}

/// The sysroot's libc++ headers: the one `c++/v1` directory holding `__config`, with `__config_site` beside it.
/// Same rule as bp2ninja stl.go `libcxxSysrootDir` (the converter's copy, used by `native`): keep the two in step.
fn libcxx_dir(sysroot: &Path) -> Result<PathBuf> {
    let mut found = Vec::new();
    config_dirs(sysroot, &mut found)?;
    ensure!(
        found.len() == 1,
        "sysroot {} holds {} c++/v1 directories with __config ({found:?}), want exactly one",
        sysroot.display(),
        found.len()
    );
    let dir = found.swap_remove(0);
    ensure!(
        dir.join("__config_site").exists(),
        "libc++ headers {} have no __config_site beside __config (a split install is not supported)",
        dir.display()
    );
    Ok(dir)
}

/// Each `c++/v1` directory under `dir` that holds a `__config`. Symlinked directories are not followed, as filepath.WalkDir does not.
fn config_dirs(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            config_dirs(&path, found)?;
        } else if entry.file_name().to_str() == Some("__config") {
            found.extend(
                path.parent()
                    .filter(|p| p.ends_with("c++/v1"))
                    .map(Path::to_path_buf),
            );
        }
    }
    Ok(())
}

/// The RUSTFLAGS of a cross link: the ISA features for rustc, then the clang triple, lld and the sysroot for the linker.
fn rustflags(features: &[String], clang: &str, sysroot: &Path, lib: &Path) -> String {
    format!(
        "-C target-feature={} -C link-arg=--target={clang} -C link-arg=-fuse-ld=lld -C link-arg=--sysroot={} -C link-arg=-L{}",
        features.join(","),
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
    fn bindgen_key_is_the_triple_with_underscores() {
        assert_eq!(
            bindgen_key("riscv64gc-unknown-linux-gnu"),
            "BINDGEN_EXTRA_CLANG_ARGS_riscv64gc_unknown_linux_gnu"
        );
    }

    /// A fake sysroot under the temp dir holding empty `files` (paths relative to the root).
    fn fake_sysroot(name: &str, files: &[&str]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("xtask-cross-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        root
    }

    #[test]
    fn libcxx_dir_is_the_one_c_plus_plus_v1_holding_config() {
        let v1 = "usr/lib/llvm-19/include/c++/v1";
        let root = fake_sysroot(
            "libcxx-one",
            &[
                &format!("{v1}/__config"),
                &format!("{v1}/__config_site"),
                // experimental/__config is not the libc++ root: not counted
                &format!("{v1}/experimental/__config"),
            ],
        );
        assert_eq!(libcxx_dir(&root).unwrap(), root.join(v1));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn libcxx_dir_refuses_two_copies_and_a_split_install() {
        let two = fake_sysroot(
            "libcxx-two",
            &[
                "a/c++/v1/__config",
                "a/c++/v1/__config_site",
                "b/c++/v1/__config",
                "b/c++/v1/__config_site",
            ],
        );
        let err = format!("{:#}", libcxx_dir(&two).unwrap_err());
        assert!(err.contains("want exactly one"), "{err}");
        std::fs::remove_dir_all(&two).unwrap();
        let split = fake_sysroot("libcxx-split", &["c++/v1/__config"]);
        let err = format!("{:#}", libcxx_dir(&split).unwrap_err());
        assert!(err.contains("__config_site"), "{err}");
        std::fs::remove_dir_all(&split).unwrap();
    }

    #[test]
    fn rustflags_carry_isa_and_link_driver() {
        let flags = rustflags(
            &["+zba".to_owned(), "+zbb".to_owned()],
            "riscv64-linux-gnu",
            Path::new("/s"),
            Path::new("/s/usr/riscv64-linux-gnu/lib"),
        );
        assert_eq!(
            flags,
            "-C target-feature=+zba,+zbb -C link-arg=--target=riscv64-linux-gnu -C link-arg=-fuse-ld=lld -C link-arg=--sysroot=/s -C link-arg=-L/s/usr/riscv64-linux-gnu/lib"
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
