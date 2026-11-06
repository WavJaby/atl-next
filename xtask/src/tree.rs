// SPDX-License-Identifier: MIT
//! The AOSP tree a repo checkout builds from: always `src/aosp`. A task with AOSP task copies builds inside a mount namespace where each task copy is bind-mounted over its `src/aosp/<project>`
//! ([`enter`]: xtask re-executes itself under `bwrap --dev-bind / /`), so every path, canonical or not, is the one a plain build sees.
//! Why not a link farm (removed): javac checks `--patch-module` membership by canonical path (a farm's libcore resolved into `src-wt/` -> "package exists in another module: java.base"), and `#include "../.."` leaving an edited project resolved physically.
//! Only AOSP task copies are bound: the other upstreams (llvm-project, qemu, boringssl …) are built by their own build systems from `Layout::locate`'s real directory, so no path is resolved through another tree.
//! Remaining risk: `ninja` run by hand outside the namespace reads the unpatched `src/aosp`; `cargo xtask native --shell` opens a shell inside it.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_broker::bwrap;
use atl_layout::Layout;

use crate::sources::{Checkout, Pin};

/// Set inside the namespace to the overlay set it was entered with ([`describe`]), so the re-executed xtask does not enter again. `fetch --migrate-git-dirs` looks for it in other processes.
pub const ENV: &str = "ATL_XTASK_OVERLAYS";

/// The shared AOSP tree, `src/aosp`: its root holds no checkout of its own, so only the tree-wide readers (the converter, the prune markers, manifest links) take it whole.
pub fn shared_tree(layout: &Layout) -> PathBuf {
    layout.src().join("aosp")
}

/// AOSP's keyboard files (`frameworks/base/data/keyboards`), the ones [`KEYBOARD_FILES`] lists: the input service loads the `.kcm` maps through atl-libinput at start, the app's input thread the `.kl` layout; the parcel gate hands Virtual.kcm's Parcel form to its driver.
pub fn keyboards(layout: &Layout) -> PathBuf {
    shared_tree(layout).join("frameworks/base/data/keyboards")
}

/// The keyboard files the runtime uses and where each goes below /system (AOSP's own places: InputDevice.cpp getInputDeviceConfigurationFilePathByName, `usr/keylayout` / `usr/keychars`).
pub const KEYBOARD_FILES: [(&str, &str); 3] = [
    ("Virtual.kcm", "usr/keychars"),
    ("Generic.kcm", "usr/keychars"),
    ("Generic.kl", "usr/keylayout"),
];

/// One bind mount: the task copy `copy` shown at `target` (`src/aosp/<project>`). Git needs no binds of its own: both `.git` gitfiles point into `src/.git-dirs/` (atl_layout::Layout::git_dir), which no bind covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Overlay {
    /// Project path in the manifest (`frameworks/native`).
    pub project: String,
    pub copy: PathBuf,
    pub target: PathBuf,
}

/// This repo checkout's AOSP task copies, in project order. A pinned project inside an edited one is refused: the mount would hide its checkout.
pub fn overlays(layout: &Layout, checkouts: &[Checkout]) -> Result<Vec<Overlay>> {
    let shared = shared_tree(layout);
    if layout.task().is_none() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Overlay> = checkouts
        .iter()
        .filter_map(|c| match &c.pin {
            Pin::Aosp(p) => {
                let l = layout.locate(&c.dir);
                let target = shared.join(p);
                l.task.is_some().then(|| Overlay {
                    project: p.clone(),
                    copy: l.dir,
                    target,
                })
            }
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.project.cmp(&b.project));
    for o in &out {
        if let Some(inner) = checkouts.iter().find_map(|c| match &c.pin {
            Pin::Aosp(q) if Path::new(q).starts_with(&o.project) && *q != o.project => Some(q),
            _ => None,
        }) {
            bail!(
                "{inner} is a project inside the edited {}: binding the task copy would hide it",
                o.project
            );
        }
    }
    Ok(out)
}

/// The overlay set as one line: `<project>=<task copy>` joined by spaces; empty for none.
pub fn describe(overlays: &[Overlay]) -> String {
    overlays
        .iter()
        .map(|o| format!("{}={}", o.project, o.copy.display()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// bubblewrap options for the build namespace: the host's `/` as is (devices too), each task copy bound over its target. Not a sandbox: it isolates nothing, it only changes which tree a path shows; it shares the broker's flag spelling, not its profiles.
pub fn namespace_args(overlays: &[Overlay]) -> bwrap::Args {
    let mut a = bwrap::Args::new();
    a.dev_bind("/", "/").die_with_parent();
    for o in overlays {
        a.bind(&o.copy, &o.target);
    }
    a
}

/// Every overlay is in effect: its target is the task copy's directory (same device + inode). Fails outside the namespace, or with the wrong copy bound.
pub fn verify(overlays: &[Overlay]) -> Result<()> {
    for o in overlays {
        let id = |p: &Path| -> Result<(u64, u64)> {
            let m = std::fs::metadata(p).with_context(|| format!("stat {}", p.display()))?;
            Ok((m.dev(), m.ino()))
        };
        ensure!(
            id(&o.target)? == id(&o.copy)?,
            "{} is not the task copy {} (xtask is outside the task-copy namespace, or another copy is bound there)",
            o.target.display(),
            o.copy.display()
        );
    }
    Ok(())
}

/// The repo checkout at `root`'s overlay set.
pub fn overlays_of(root: &Path) -> Result<Vec<Overlay>> {
    let layout = Layout::of(root)?;
    overlays(&layout, &crate::sources::load(root)?.checkouts(None))
}

/// The AOSP file `rel` (tree-relative) as the build reads it, for a reader outside the namespace: in its task copy when one covers it, else in the shared tree.
pub fn build_file(root: &Path, rel: &str) -> Result<PathBuf> {
    let layout = Layout::of(root)?;
    Ok(overlays_of(root)?
        .into_iter()
        .find_map(|o| {
            Path::new(rel)
                .strip_prefix(&o.project)
                .ok()
                .map(|r| o.copy.join(r))
        })
        .unwrap_or_else(|| shared_tree(&layout).join(rel)))
}

/// Run the rest of this xtask invocation where the AOSP task copies are bound over `src/aosp`: Some(exit code) = re-executed inside the namespace, the caller returns it; None = continue here
/// (no task copies, or already inside, the overlays verified). `shell` = the namespace's command is `$SHELL` instead (for `ninja` by hand), with `shell_env` in its environment (`native::shell_env`).
pub fn enter(
    overlays: &[Overlay],
    shell: bool,
    shell_env: &[(&'static str, std::ffi::OsString)],
) -> Result<Option<ExitCode>> {
    let want = describe(overlays);
    match std::env::var(ENV) {
        Ok(had) => {
            ensure!(
                had == want,
                "entered the task-copy namespace with [{had}], the checkout now has [{want}]"
            );
            verify(overlays)?;
            if !shell {
                return Ok(None);
            }
        }
        Err(_) if overlays.is_empty() && !shell => return Ok(None),
        Err(_) => {}
    }
    for o in overlays {
        ensure!(
            o.target.is_dir(),
            "{} is missing: the task copy needs its src/ checkout as mount point (cargo xtask fetch --execute)",
            o.target.display()
        );
    }
    let mut cmd = Command::new(bwrap::PROGRAM);
    cmd.args(namespace_args(overlays).end()).env(ENV, &want);
    if shell {
        cmd.envs(shell_env.iter().cloned())
            .arg(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()));
    } else {
        cmd.arg(std::env::current_exe().context("finding xtask's executable")?)
            .args(std::env::args_os().skip(1))
            .current_dir(std::env::current_dir()?);
    }
    if !want.is_empty() {
        println!("tree: task copies bound over src/aosp: {want}");
    }
    let status = cmd.status().context("running bwrap")?;
    Ok(Some(match status.code() {
        Some(c) => ExitCode::from(u8::try_from(c).unwrap_or(1)),
        None => ExitCode::FAILURE,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(path: &str) -> Checkout {
        Checkout {
            dir: Path::new("aosp").join(path),
            url: None,
            rev: "refs/tags/t".into(),
            commit: "c".into(),
            sparse: Vec::new(),
            pin: Pin::Aosp(path.into()),
            licence_key: "aosp".into(),
        }
    }

    /// Workspace with src/aosp/{a/b,a/c} and a task copy of a/b.
    fn workspace(tag: &str) -> PathBuf {
        let ws = std::env::temp_dir().join(format!("xtask-tree-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let layout = Layout::new(&ws, None);
        for d in ["aosp/a/b", "aosp/a/c"] {
            std::fs::create_dir_all(ws.join("src").join(d)).unwrap();
            let store = layout.git_dir(Path::new(d));
            std::fs::create_dir_all(&store).unwrap();
            std::fs::write(
                ws.join("src").join(d).join(".git"),
                format!("gitdir: {}\n", store.display()),
            )
            .unwrap();
        }
        let copy = ws.join("src-wt/t/aosp/a/b");
        std::fs::create_dir_all(&copy).unwrap();
        // a worktree of the shared checkout, as `xtask patch --edit` makes it
        let gitdir = layout.git_dir(Path::new("aosp/a/b")).join("worktrees/b");
        std::fs::write(copy.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(copy.join("patched"), "").unwrap();
        ws
    }

    /// Golden argv, written from the namespace's spec: the host's / as is with devices (`--dev-bind / /`: builds open /dev/null, sccache its socket), gone with xtask (`--die-with-parent`);
    /// per overlay one bind of the task copy over its target, nothing for git (the git dir lives in the store, outside the bound tree).
    #[test]
    fn namespace_argv() {
        assert_eq!(
            namespace_args(&[]).end(),
            ["--dev-bind", "/", "/", "--die-with-parent", "--"]
        );
        let o = Overlay {
            project: "a/b".into(),
            copy: "/c".into(),
            target: "/t".into(),
        };
        let want = "--dev-bind / / --die-with-parent --bind /c /t --";
        let got: Vec<String> = namespace_args(&[o])
            .end()
            .iter()
            .map(|w| w.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got.join(" "), want);
    }

    #[test]
    fn overlays_follow_task_copies() {
        let ws = workspace("set");
        let layout = Layout::new(&ws, Some("t"));
        let all = [checkout("a/c"), checkout("a/b")];
        let o = overlays(&layout, &all).unwrap();
        assert_eq!(
            o,
            [Overlay {
                project: "a/b".into(),
                copy: ws.join("src-wt/t/aosp/a/b"),
                target: ws.join("src/aosp/a/b"),
            }]
        );
        assert_eq!(
            describe(&o),
            format!("a/b={}", ws.join("src-wt/t/aosp/a/b").display())
        );
        // main: no task copies
        assert!(overlays(&Layout::new(&ws, None), &all).unwrap().is_empty());
        // a pinned project inside the edited one
        let e = format!(
            "{:#}",
            overlays(&layout, &[checkout("a/b"), checkout("a/b/n")]).unwrap_err()
        );
        assert!(
            e.contains("a/b/n is a project inside the edited a/b"),
            "{e}"
        );
        // outside the namespace the overlay is not in effect: verify must fire
        let e = format!("{:#}", verify(&o).unwrap_err());
        assert!(e.contains("outside the task-copy namespace"), "{e}");
        std::fs::remove_dir_all(&ws).unwrap();
    }
}
