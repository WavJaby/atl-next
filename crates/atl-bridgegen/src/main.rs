// SPDX-License-Identifier: MIT
//! Inspect what atl-bridgegen makes of one header: the per-arch IR, host Rust types, ABI-identity report.
//!
//! Crates get the same output from their `build.rs`; this binary only writes it where you can read it.
//! Writes `<name>.ir.json`, `<name>.<host>.rs` per `--host`, and `<name>.abi.txt` (every other target
//! against every host) into `--out`.
//!
//! Reads: the header and whatever its targets' arguments include
//! Default: mutating (writes into --out)
#![forbid(unsafe_code)]
// Terminal output is this tool's product, not logging.
#![allow(clippy::print_stdout)]

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};
use atl_bridgegen::{Lang, Source, Target, report, rust};
use clap::Parser;

#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// Header to describe.
    #[arg(long)]
    header: PathBuf,
    /// Interface name: the output file stem.
    #[arg(long)]
    name: String,
    /// How generated text names the header (default: --header). Use a source-tree-relative path so
    /// output does not depend on where the workspace lives.
    #[arg(long)]
    label: Option<String>,
    /// `<arch>=<triple>[ <clang argument>…]`, repeatable: one IR view per target.
    #[arg(long = "target", required = true)]
    targets: Vec<String>,
    /// Arch to emit Rust types for (one of the targets), repeatable.
    #[arg(long = "host")]
    hosts: Vec<String>,
    /// Object-like macro to evaluate (integer or string), or a `*` glob over the macros the header
    /// defines; repeatable (enumerators come without it).
    #[arg(long = "macro")]
    macros: Vec<String>,
    /// Parse --header as C++ (a C++ source or header) instead of C.
    #[arg(long)]
    cxx: bool,
    /// Further file whose declarations belong to the interface (a header the C++ source keeps its types
    /// in), repeatable.
    #[arg(long)]
    also: Vec<PathBuf>,
    /// Output directory.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let targets: Vec<Target> = args
        .targets
        .iter()
        .map(|t| target(t))
        .collect::<Result<_>>()?;
    let label = args
        .label
        .clone()
        .unwrap_or_else(|| args.header.display().to_string());
    let macros: Vec<&str> = args.macros.iter().map(String::as_str).collect();
    let src = Source {
        path: &args.header,
        lang: if args.cxx { Lang::Cxx } else { Lang::C },
        also: &args.also,
        macros: &macros,
    };
    let iface = atl_bridgegen::interface(&args.name, &label, &src, &targets)?;
    std::fs::create_dir_all(&args.out)?;
    let write = |file: String, text: &str| -> Result<()> {
        let path = args.out.join(file);
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        println!("wrote {}", path.display());
        Ok(())
    };
    write(
        format!("{}.ir.json", args.name),
        &atl_bridgegen::to_json(&iface)?,
    )?;
    let mut abi = String::new();
    for host in &args.hosts {
        write(
            format!("{}.{host}.rs", args.name),
            &rust::host_module(&iface, host)?,
        )?;
        for other in iface.arches.keys().filter(|a| *a != host) {
            for d in report::compare(&iface, other, host) {
                let verdict = if d.identical {
                    "identical".to_owned()
                } else {
                    d.differences.join("; ")
                };
                writeln!(abi, "{other} vs {host}  {}: {verdict}", d.record)?;
            }
        }
    }
    write(format!("{}.abi.txt", args.name), &abi)?;
    Ok(())
}

/// `arch=triple arg arg …`
fn target(spec: &str) -> Result<Target> {
    let (arch, rest) = spec
        .split_once('=')
        .with_context(|| format!("--target {spec}: expected <arch>=<triple>"))?;
    let mut words = rest.split_whitespace();
    let triple = words
        .next()
        .with_context(|| format!("--target {spec}: no triple"))?;
    Ok(Target {
        arch: arch.to_owned(),
        triple: triple.to_owned(),
        args: words.map(str::to_owned).collect(),
    })
}
