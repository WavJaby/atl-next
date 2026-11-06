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
use atl_layout::Layout;

use crate::sources::{Checkout, Pin};

/// Set inside the namespace to the overlay set it was entered with ([`describe`]), so the re-executed xtask does not enter again.
const ENV: &str = "ATL_XTASK_OVERLAYS";

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

/// One bind mount: the task copy `copy` shown at `target` (`src/aosp/<project>`).
#[derive(Debug, Clone, PartialEq)]
pub struct Overlay {
    /// Project path in the manifest (`frameworks/native`).
    pub project: String,
    pub copy: PathBuf,
    pub target: PathBuf,
    pub git: Option<GitShadow>,
}

/// A task copy is a git worktree of the shared checkout: its `.git` file points into `<target>/.git/worktrees/<n>`, which the bind hides. The namespace shows the shared checkout at
/// `shadow` (`src-wt/<task>/.shadow/<project>`) and binds `gitfile` (the pointer rewritten into it) over the copy's `.git` at both paths, so git works inside.
#[derive(Debug, Clone, PartialEq)]
pub struct GitShadow {
    pub shadow: PathBuf,
    pub gitfile: PathBuf,
    /// The rewritten pointer, `gitdir: <shadow>/.git/worktrees/<n>`.
    pub content: String,
}

/// The copy's `.git` pointer, when it leads under `target` (a worktree of the checkout the bind hides).
fn git_shadow(copy: &Path, target: &Path, shadow_root: &Path, project: &str) -> Option<GitShadow> {
    let text = std::fs::read_to_string(copy.join(".git")).ok()?;
    let gitdir = Path::new(text.strip_prefix("gitdir:")?.trim());
    let rest = gitdir.strip_prefix(target).ok()?;
    let (shadow, gitfile) = shadow_paths(shadow_root, project);
    Some(GitShadow {
        content: format!("gitdir: {}\n", shadow.join(rest).display()),
        gitfile,
        shadow,
    })
}

/// A project's shadow mount point and rewritten pointer under `shadow_root` (`src-wt/<task>/.shadow`).
fn shadow_paths(shadow_root: &Path, project: &str) -> (PathBuf, PathBuf) {
    (
        shadow_root.join(project),
        shadow_root.join(format!("{}.gitfile", project.replace('/', "_"))),
    )
}

/// `xtask patch --done` removed the task copy of `project`: its shadow goes too (the mount point, empty outside the namespace, and the pointer), then the emptied directories up to `.shadow`.
pub fn remove_shadow(layout: &Layout, project: &str) -> Result<()> {
    let Some((_, task_dir)) = layout.task() else {
        return Ok(());
    };
    let root = task_dir.join(".shadow");
    let (shadow, gitfile) = shadow_paths(&root, project);
    match std::fs::remove_file(&gitfile) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", gitfile.display()));
        }
        _ => {}
    }
    // remove_dir only: an empty mount point never takes anything with it
    for d in shadow.ancestors().take_while(|d| d.starts_with(&root)) {
        if std::fs::remove_dir(d).is_err() {
            break;
        }
    }
    Ok(())
}

/// This repo checkout's AOSP task copies, in project order. A pinned project inside an edited one is refused: the mount would hide its checkout.
pub fn overlays(layout: &Layout, checkouts: &[Checkout]) -> Result<Vec<Overlay>> {
    let shared = shared_tree(layout);
    let Some((_, task_dir)) = layout.task() else {
        return Ok(Vec::new());
    };
    let shadow_root = task_dir.join(".shadow");
    let mut out: Vec<Overlay> = checkouts
        .iter()
        .filter_map(|c| match &c.pin {
            Pin::Aosp(p) => {
                let l = layout.locate(&c.dir);
                let target = shared.join(p);
                l.task.is_some().then(|| Overlay {
                    project: p.clone(),
                    git: git_shadow(&l.dir, &target, &shadow_root, p),
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

/// bubblewrap arguments for the namespace: the host's `/` as is, each task copy bound over its target.
pub fn bwrap_args(overlays: &[Overlay]) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--dev-bind".into(),
        "/".into(),
        "/".into(),
        "--die-with-parent".into(),
    ];
    let mut bind = |kind: &str, from: &Path, to: &Path| {
        a.extend([
            kind.into(),
            from.display().to_string(),
            to.display().to_string(),
        ]);
    };
    for o in overlays {
        // the shared checkout first, while its path still shows it
        if let Some(g) = &o.git {
            bind("--bind", &o.target, &g.shadow);
        }
        bind("--bind", &o.copy, &o.target);
        if let Some(g) = &o.git {
            bind("--ro-bind", &g.gitfile, &o.target.join(".git"));
            bind("--ro-bind", &g.gitfile, &o.copy.join(".git"));
        }
    }
    a
}

/// One overlay of `copy` over `target` outside a workspace layout (the task-bind gate's scratch trees), its git shadow under `shadow_root`.
pub fn overlays_for_test(copy: &Path, target: &Path, shadow_root: &Path) -> Overlay {
    Overlay {
        project: "probe".into(),
        git: git_shadow(copy, target, shadow_root, "probe"),
        copy: copy.to_path_buf(),
        target: target.to_path_buf(),
    }
}

/// Write each overlay's shadow mount point and rewritten git pointer (read by [`bwrap_args`]'s binds).
pub fn prepare(overlays: &[Overlay]) -> Result<()> {
    for g in overlays.iter().filter_map(|o| o.git.as_ref()) {
        std::fs::create_dir_all(&g.shadow)
            .with_context(|| format!("creating {}", g.shadow.display()))?;
        std::fs::write(&g.gitfile, &g.content)
            .with_context(|| format!("writing {}", g.gitfile.display()))?;
    }
    Ok(())
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

/// Run the rest of this xtask invocation where the AOSP task copies are bound over `src/aosp`: Some(exit code) = re-executed inside the namespace, the caller returns it; None = continue here
/// (no task copies, or already inside, the overlays verified). `shell` = the namespace's command is `$SHELL` instead (for `ninja` by hand).
pub fn enter(overlays: &[Overlay], shell: bool) -> Result<Option<ExitCode>> {
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
    prepare(overlays)?;
    let mut cmd = Command::new("bwrap");
    cmd.args(bwrap_args(overlays)).arg("--").env(ENV, &want);
    if shell {
        cmd.arg(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into()));
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
        for d in ["src/aosp/a/b/.git", "src/aosp/a/c"] {
            std::fs::create_dir_all(ws.join(d)).unwrap();
        }
        let copy = ws.join("src-wt/t/aosp/a/b");
        std::fs::create_dir_all(&copy).unwrap();
        // a worktree of the shared checkout, as `xtask patch --edit` makes it
        let gitdir = ws.join("src/aosp/a/b/.git/worktrees/b");
        std::fs::write(copy.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(copy.join("patched"), "").unwrap();
        ws
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
                git: Some(GitShadow {
                    shadow: ws.join("src-wt/t/.shadow/a/b"),
                    gitfile: ws.join("src-wt/t/.shadow/a_b.gitfile"),
                    content: format!(
                        "gitdir: {}\n",
                        ws.join("src-wt/t/.shadow/a/b/.git/worktrees/b").display()
                    ),
                }),
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
        // the shadow is written for the namespace and goes with the task copy
        prepare(&o).unwrap();
        assert!(ws.join("src-wt/t/.shadow/a_b.gitfile").is_file());
        remove_shadow(&layout, "a/b").unwrap();
        assert!(!ws.join("src-wt/t/.shadow").exists());
        std::fs::remove_dir_all(&ws).unwrap();
    }
}
