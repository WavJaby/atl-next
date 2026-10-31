// SPDX-License-Identifier: MIT
//! `xtask check`: the static + unit test tiers.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::{arches, deps, licenses, modules, patch, providers, prune, sources, spdx, workspace};
use atl_layout::Layout;

#[derive(clap::Args)]
pub struct Args {
    /// Run only these steps (repeatable).
    #[arg(long, value_enum)]
    only: Vec<Step>,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Step {
    /// rustfmt, check mode
    Fmt,
    /// SPDX header on every tracked or new, non-ignored file
    Spdx,
    /// repo tables (arches, sources, deps, providers, licenses) parse with their schema and invariants
    Tables,
    /// modules.toml equals the real cross-module crate graph
    Modules,
    /// fetched upstream checkouts match their pins and patches/ (no unexported or uncommitted work)
    Drift,
    /// clippy on all targets, warnings denied
    Clippy,
    /// cargo-deny: advisories, bans, licences, sources
    Deny,
    /// cargo test, whole workspace
    Test,
}

const ALL: [Step; 8] = [
    Step::Fmt,
    Step::Spdx,
    Step::Tables,
    Step::Modules,
    Step::Drift,
    Step::Clippy,
    Step::Deny,
    Step::Test,
];

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Fmt => "fmt",
            Step::Spdx => "spdx",
            Step::Tables => "tables",
            Step::Modules => "modules",
            Step::Drift => "drift",
            Step::Clippy => "clippy",
            Step::Deny => "deny",
            Step::Test => "test",
        }
    }

    fn run(self, root: &Path) -> Result<()> {
        match self {
            Step::Fmt => cargo(root, &["fmt", "--all", "--check"]),
            Step::Spdx => spdx::check(root),
            Step::Tables => {
                let arches = arches::load(root)?;
                arches::check_kernel_arch(&Layout::of(root)?, &arches)?;
                eprintln!("tables: {} ok ({} arches)", arches::FILE, arches.len());
                let sources = sources::load(root)?;
                eprintln!(
                    "tables: {} ok ({} aosp projects, {} other upstreams)",
                    sources::FILE,
                    sources.aosp.projects.len(),
                    sources.git.len()
                );
                let workspace = workspace::load(root)?;
                let consumers = deps::Consumers::new(&workspace, &sources);
                let deps = deps::load(root, &consumers)?;
                eprintln!("tables: {} ok ({} rows)", deps::FILE, deps.len());
                let providers =
                    providers::load(root, &providers::Targets::new(&workspace.crates, &sources))?;
                eprintln!("tables: {} ok ({} rows)", providers::FILE, providers.len());
                let licenses = licenses::load(root, &consumers.upstreams)?;
                eprintln!(
                    "tables: {} ok ({} artifacts, deny.toml within runtime)",
                    licenses::FILE,
                    licenses.artifact.len()
                );
                let prune = prune::load(root, &sources)?;
                eprintln!("tables: {} ok ({} rows)", prune::FILE, prune.len());
                Ok(())
            }
            Step::Modules => {
                let workspace = workspace::load(root)?;
                let modules = modules::load(root, &workspace)?;
                eprintln!(
                    "modules: {} ok ({} modules, {} crate edges)",
                    modules::FILE,
                    modules.len(),
                    workspace.edges.len()
                );
                Ok(())
            }
            Step::Drift => patch::check(root),
            Step::Clippy => cargo(
                root,
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            Step::Deny => {
                // a missing tool fails the step; skipping would report a check that never ran
                cargo(root, &["deny", "--version"])
                    .context("cargo-deny not installed: cargo install --locked cargo-deny")?;
                cargo(root, &["deny", "--locked", "check"])
            }
            Step::Test => cargo(root, &["test", "--workspace", "--locked"]),
        }
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let steps: Vec<Step> = ALL
        .into_iter()
        .filter(|s| args.only.is_empty() || args.only.contains(s))
        .collect();
    let mut failed = Vec::new();
    for step in &steps {
        eprintln!("== check: {}", step.name());
        if let Err(e) = step.run(root) {
            eprintln!("== check: {} FAILED: {e:#}", step.name());
            failed.push(step.name());
        }
    }
    if failed.is_empty() {
        println!("check: ok ({} steps)", steps.len());
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "check: FAILED {} of {} steps: {}",
            failed.len(),
            steps.len(),
            failed.join(", ")
        );
        Ok(ExitCode::FAILURE)
    }
}

fn cargo(root: &Path, args: &[&str]) -> Result<()> {
    let status = crate::cargo_command()
        .args(args)
        .current_dir(root)
        .status()
        .with_context(|| format!("spawning cargo {}", args.join(" ")))?;
    if !status.success() {
        bail!("cargo {} exited with {status}", args.join(" "));
    }
    Ok(())
}
