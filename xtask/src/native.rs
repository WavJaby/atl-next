// SPDX-License-Identifier: MIT
//! `xtask native`: AOSP native modules built for this Linux host by our Blueprint converter
//! (`tools/bp2ninja`, plan D7-2): builds the converter with the distro Go against the pinned
//! `build/blueprint` (through a generated go.work: that path differs per checkout), converts the
//! goals' closure into ninja, builds it with `ninja -k 0` and summarises the failures by module.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, ensure};

use crate::{arches, deps, providers, sources, workspace};

#[derive(clap::Args)]
pub struct Args {
    /// Android.bp module names to build (with their dependency closure).
    #[arg(required = true)]
    goals: Vec<String>,
    /// The host arch (an arches.toml row with `[host]`); also Soong's arch name.
    #[arg(long)]
    host: String,
    /// C compiler (deps.toml row `clang`).
    #[arg(long, default_value = "clang-19")]
    cc: String,
    /// C++ compiler and link driver (deps.toml row `clang`).
    #[arg(long, default_value = "clang++-19")]
    cxx: String,
    /// Archiver (deps.toml row `llvm`).
    #[arg(long, default_value = "llvm-ar-19")]
    ar: String,
}

/// First error lines echoed per failing module; the ninja log keeps all of them.
const ERRORS_PER_MODULE: usize = 2;

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let arches = arches::load(root)?;
    ensure!(
        arches.get(&args.host).is_some_and(|a| a.host.is_some()),
        "{}: not a host arch in {}",
        args.host,
        arches::FILE
    );
    let ws = workspace::root_dir(root)?;
    let tree = ws.join("src").join("aosp");
    let tool_dir = ws.join("out").join("bp2ninja");
    let out = ws.join("out").join("native").join(&args.host);
    std::fs::create_dir_all(&tool_dir)?;
    let go_work = tool_dir.join("go.work");
    std::fs::write(
        &go_work,
        format!(
            "go 1.24\n\nuse {}\n\nreplace github.com/google/blueprint => {}\n",
            root.join("tools/bp2ninja").display(),
            tree.join("build/blueprint").display()
        ),
    )?;
    let converter = tool_dir.join("bp2ninja");
    let status = Command::new("go")
        .args(["build", "-o"])
        .arg(&converter)
        .arg(".")
        .current_dir(root.join("tools/bp2ninja"))
        .env("GOWORK", &go_work)
        // the pinned blueprint is the only dependency: no toolchain or module downloads
        .env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .status()
        .context("running go build")?;
    ensure!(status.success(), "building bp2ninja failed");
    let sources = sources::load(root)?;
    let workspace = workspace::load(root)?;
    let deps = deps::load(root, &deps::Consumers::new(&workspace, &sources))?;
    let providers = providers::load(root, &workspace.crates, &deps)?;
    let omit = providers::omitted(&providers).join(",");
    let status = Command::new(&converter)
        .arg("-tree")
        .arg(&tree)
        .arg("-out")
        .arg(&out)
        .args([
            "-arch", &args.host, "-cc", &args.cc, "-cxx", &args.cxx, "-ar", &args.ar, "-omit",
            &omit,
        ])
        .args(&args.goals)
        .status()
        .context("running bp2ninja")?;
    ensure!(status.success(), "bp2ninja failed");
    let problems = std::fs::read_to_string(out.join("problems.txt")).unwrap_or_default();
    for p in problems.lines().filter(|l| !l.is_empty()) {
        println!("native: problem {p}");
    }
    let log = out.join("ninja.log");
    let status = Command::new("ninja")
        .args(["-k", "0", "-C"])
        .arg(&out)
        .stdout(std::fs::File::create(&log)?)
        .stderr(std::fs::File::create(out.join("ninja.err"))?)
        .status()
        .context("running ninja")?;
    if status.success() {
        println!("native: built -> {}", out.display());
        return Ok(ExitCode::SUCCESS);
    }
    let text = std::fs::read_to_string(&log)?;
    let failures = failures(&text, &format!("{}/", out.display()));
    let total: usize = failures.values().map(|f| f.0).sum();
    println!(
        "native: {total} failed steps in {} modules (log {})",
        failures.len(),
        log.display()
    );
    for (module, (count, errors)) in &failures {
        println!("native:   {module}: {count} failed");
        for e in errors {
            println!("native:     {e}");
        }
    }
    Ok(ExitCode::FAILURE)
}

/// Failed ninja steps grouped by `<side>/<module>` (from the output path), with their first errors.
fn failures(log: &str, out_prefix: &str) -> BTreeMap<String, (usize, Vec<String>)> {
    let mut by_module: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in log.lines() {
        if let Some(outputs) = line.strip_prefix("FAILED: ") {
            let first = outputs.split(' ').next().unwrap_or(outputs);
            let rel = first.strip_prefix(out_prefix).unwrap_or(first);
            // obj/<side>/<module>/…, gen/<side>/<module>/…, <side>/<module>/<artifact>
            let parts: Vec<&str> = rel.split('/').collect();
            let module = match parts.as_slice() {
                ["obj" | "gen", side, module, ..] | [side, module, ..] => {
                    format!("{side}/{module}")
                }
                _ => rel.to_owned(),
            };
            by_module.entry(module.clone()).or_default().0 += 1;
            current = Some(module);
        } else if let Some(m) = &current
            && line.contains("error:")
            && let Some(entry) = by_module.get_mut(m)
            && entry.1.len() < ERRORS_PER_MODULE
            && !entry.1.iter().any(|e| e == line.trim())
        {
            entry.1.push(line.trim().to_owned());
        }
    }
    by_module
}

#[cfg(test)]
mod tests {
    #[test]
    fn failures_grouped_by_module() {
        let log = "\
FAILED: /o/obj/device/liblog/shared/x.o
x.cpp:1:1: error: boom
FAILED: /o/gen/host/libaidl-common/src/y.cpp /o/gen/host/libaidl-common/src/y.h
bison: error
FAILED: /o/device/libbinder/libbinder.so
ld.lld: error: undefined symbol: f
[3/9] cc ok.cpp
";
        let f = super::failures(log, "/o/");
        assert_eq!(
            f["device/liblog"],
            (1, vec!["x.cpp:1:1: error: boom".to_owned()])
        );
        assert_eq!(f["host/libaidl-common"].0, 1);
        assert_eq!(
            f["device/libbinder"].1,
            ["ld.lld: error: undefined symbol: f"]
        );
    }
}
