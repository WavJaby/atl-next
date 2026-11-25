// SPDX-License-Identifier: MIT
//! Check step `bootstrap`: a fresh checkout builds `xtask` without product crates or fetched trees.
//!
//! Two halves. Structural: every member in xtask's build closure (cargo metadata, non-dev edges) is a definition crate -- no build script, and only definition crates below it; derived from metadata, no hand list.
//! Sandbox: `cargo build -p xtask` offline under bwrap with tmpfs over `src`, `src-wt` and `out` and no network, into the checkout's bootstrap target bound back in place over `out` (see [`sandbox`]); a control build of [`CONTROL`] in the same sandbox must fail on a hidden path, else the sandbox hides nothing and the green build proves nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use atl_bwrap as bwrap;
use atl_layout::Layout;

use crate::modules;
use crate::out;
use crate::unit_use::{self, Echo, Units};
use crate::workspace::{self, Workspace};

/// The bootstrap tool's crate.
const TOOL: &str = "xtask";
/// A member whose build script reads the fetched tree (bionic's binder.h): its build must fail in the sandbox.
const CONTROL: &str = "atl-binder-wire";

pub fn check(root: &Path) -> Result<()> {
    let ws = workspace::load(root)?;
    let mods = modules::load(root, &ws)?;
    let closure = ws.build_closure(TOOL);
    structural(&ws, &closure, &|c| {
        mods.iter()
            .find(|(_, m)| m.crates.iter().any(|k| k == c))
            .map_or("-", |(name, _)| name.as_str())
            .to_owned()
    })?;
    let layout = Layout::of(root)?;
    let hidden = hidden_dirs(&layout);
    // one target per checkout (its build dir): a shared one fed a worktree at another commit stale crate outputs (E0061 in xtask).
    // Under out/, which the sandbox hides: sandbox() binds it back. ensure_held: refresh, leave and prune leave the dir alone while the gate runs.
    let (_, dir) = out::ensure_held(root)?;
    let target = state_dir(&dir);
    let tool = sandboxed_build(root, &hidden, &target, TOOL)?;
    if !tool.0 {
        bail!(
            "{TOOL} does not build without the fetched trees and out/:\n{}",
            tool.1
        );
    }
    let control = sandboxed_build(root, &hidden, &target, CONTROL)?;
    ensure!(
        !control.0,
        "control: {CONTROL} built in the sandbox: tmpfs over {} hides nothing it reads, so the {TOOL} build proves nothing",
        describe(&hidden)
    );
    ensure!(
        hidden
            .iter()
            .any(|h| control.1.contains(&*h.to_string_lossy())),
        "control: {CONTROL} failed in the sandbox, but not on a hidden path ({}): \n{}",
        describe(&hidden),
        control.1
    );
    eprintln!(
        "bootstrap: ok ({TOOL} closure {} members, all definition crates; builds offline with {} hidden; control {CONTROL} fails there)",
        closure.len(),
        describe(&hidden)
    );
    Ok(())
}

/// Members that are definition crates: no build script, and every build dependency among members is one.
pub fn definition_crates(ws: &Workspace) -> BTreeSet<String> {
    let mut defs: BTreeSet<String> = BTreeSet::new();
    // fixpoint: a crate joins once all its member deps have
    loop {
        let before = defs.len();
        for c in &ws.crates {
            if defs.contains(c) || ws.build_scripts.contains(c) {
                continue;
            }
            if ws
                .build_edges
                .iter()
                .filter(|(f, _)| f == c)
                .all(|(_, t)| defs.contains(t))
            {
                defs.insert(c.clone());
            }
        }
        if defs.len() == before {
            return defs;
        }
    }
}

fn structural(
    ws: &Workspace,
    closure: &BTreeSet<String>,
    module: &dyn Fn(&str) -> String,
) -> Result<()> {
    let defs = definition_crates(ws);
    let bad: Vec<String> = closure
        .iter()
        .filter(|c| *c != TOOL && !defs.contains(*c))
        .map(|c| {
            let why = if ws.build_scripts.contains(c) {
                "has a build script".to_owned()
            } else {
                let deps: Vec<&str> = ws
                    .build_edges
                    .iter()
                    .filter(|(f, t)| f == c && !defs.contains(t))
                    .map(|(_, t)| t.as_str())
                    .collect();
                format!("depends on {}", deps.join(", "))
            };
            format!("{c} (module {}): {why}", module(c))
        })
        .collect();
    if !bad.is_empty() {
        bail!(
            "{TOOL}'s build closure reaches {} non-definition crate(s): {}",
            bad.len(),
            bad.join("; ")
        );
    }
    Ok(())
}

/// The fetched trees and the build outputs: what a fresh checkout lacks.
fn hidden_dirs(layout: &Layout) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = layout.fetched_roots().into();
    v.push(layout.workspace().join("out"));
    v
}

fn describe(dirs: &[PathBuf]) -> String {
    dirs.iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The gate's target in build dir `dir` (`out::dir_of`): `out prune` removes it with the dir.
fn state_dir(dir: &Path) -> PathBuf {
    dir.join(out::BOOTSTRAP_DIR)
}

/// The sandbox options: tmpfs over each hidden dir, then `target` bound back in place. bwrap applies them in order, so the bind lands inside the tmpfs when `target` sits under a hidden dir.
fn sandbox(root: &Path, hidden: &[PathBuf], target: &Path) -> bwrap::Args {
    let mut a = bwrap::Args::new();
    a.dev_bind("/", "/").unshare_net().die_with_parent();
    for h in hidden {
        a.tmpfs(h);
    }
    a.bind(target, target);
    a.chdir(root);
    a
}

/// (success, combined output) of `cargo build -p <krate>` in the sandbox.
fn sandboxed_build(
    root: &Path,
    hidden: &[PathBuf],
    target: &Path,
    krate: &str,
) -> Result<(bool, String)> {
    std::fs::create_dir_all(target).with_context(|| target.display().to_string())?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(bwrap::PROGRAM);
    // the step's grant reaches the sandboxed cargo through the environment
    crate::grant_jobs(&mut cmd);
    cmd.args(sandbox(root, hidden, target).end())
        .arg(cargo)
        .args([
            "build",
            "--offline",
            "--locked",
            "--quiet",
            "-p",
            krate,
            "--target-dir",
        ])
        .arg(target)
        .env_remove("CARGO_TARGET_DIR")
        .current_dir(root);
    let out = unit_use::run(&mut cmd, Units::Plain, Echo::Nothing)
        .with_context(|| format!("spawning {} for cargo build -p {krate}", bwrap::PROGRAM))?;
    Ok((
        out.status.success(),
        format!("{}{}", out.stdout, out.stderr),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(crates: &[&str], edges: &[(&str, &str)], scripts: &[&str]) -> Workspace {
        let own = |s: &&str| (*s).to_owned();
        let e: BTreeSet<(String, String)> = edges
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect();
        Workspace {
            crates: crates.iter().map(own).collect(),
            edges: e.clone(),
            build_edges: e,
            build_scripts: scripts.iter().map(own).collect(),
            ..Workspace::default()
        }
    }

    #[test]
    fn definition_needs_no_script_and_definition_deps_only() {
        // d1 <- d2 pure; s has a script; t depends on s; xtask itself depends on s
        let w = ws(
            &["d1", "d2", "s", "t", "xtask"],
            &[("d2", "d1"), ("t", "s"), ("xtask", "d2")],
            &["s"],
        );
        let defs = definition_crates(&w);
        assert_eq!(
            defs,
            BTreeSet::from(["d1".into(), "d2".into(), "xtask".into()])
        );
    }

    #[test]
    fn structural_names_the_runtime_crate_reached() {
        let w = ws(
            &["d", "s", "t", "xtask"],
            &[("xtask", "d"), ("xtask", "t"), ("t", "s")],
            &["s"],
        );
        let e = format!(
            "{:#}",
            structural(&w, &w.build_closure(TOOL), &|_| "m".into()).unwrap_err()
        );
        assert!(e.contains("s (module m): has a build script"), "{e}");
        assert!(e.contains("t (module m): depends on s"), "{e}");
        let ok = ws(&["d", "xtask"], &[("xtask", "d")], &[]);
        structural(&ok, &ok.build_closure(TOOL), &|_| "m".into()).unwrap();
    }

    #[test]
    fn dev_edges_stay_out_of_the_closure() {
        let mut w = ws(&["d", "s", "xtask"], &[("xtask", "d")], &["s"]);
        w.edges.insert(("xtask".into(), "s".into()));
        assert_eq!(
            w.build_closure(TOOL),
            BTreeSet::from(["d".into(), "xtask".into()])
        );
    }

    /// Literals are the spec's pool layout (`out/pool/ws/<id>`), not `state_dir`'s own join.
    #[test]
    fn the_gate_state_lives_in_the_build_dir() {
        let ws = Path::new("/home/u/atl-next");
        assert_eq!(
            state_dir(&out::dir_of(ws, "repo")),
            Path::new("/home/u/atl-next/out/pool/ws/repo/bootstrap")
        );
        assert_eq!(
            state_dir(&out::dir_of(ws, "bootstrap-gate-ws")),
            Path::new("/home/u/atl-next/out/pool/ws/bootstrap-gate-ws/bootstrap")
        );
    }

    /// Golden argv: bwrap applies options in order, so the bind back of the target follows the tmpfs over `out`; reversed, the target is an empty dir in the sandbox.
    #[test]
    fn sandbox_binds_the_target_after_the_hidden_dirs() {
        let ws = Path::new("/home/u/atl-next");
        let hidden = [ws.join("out")];
        let target = state_dir(&out::dir_of(ws, "repo"));
        let argv: Vec<String> = sandbox(ws, &hidden, &target)
            .options()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let out_dir = "/home/u/atl-next/out";
        let bootstrap = "/home/u/atl-next/out/pool/ws/repo/bootstrap";
        let tmpfs = argv
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == out_dir)
            .expect("tmpfs over out");
        let bind = argv
            .windows(3)
            .position(|w| w[0] == "--bind" && w[1] == bootstrap && w[2] == bootstrap)
            .expect("target bound back");
        assert!(tmpfs < bind, "tmpfs at {tmpfs}, bind at {bind}: {argv:?}");
    }
}
