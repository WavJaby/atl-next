// SPDX-License-Identifier: MIT
//! Where an upstream's checkout lives for one repo checkout. `src/<up>` is shared by every repo checkout and holds the pin + origin/main's patches; a task editing an upstream works in its own copy `src-wt/<task>/<up>` (`xtask patch --edit`; `<task>` = the repo checkout's branch), a git worktree of `src/<up>` on branch `atl-patches-<task>`.
//! `Layout::checkout` is the one resolver: fetch, patch, native and the upstream builds reach a checkout only through it. The converter takes the AOSP tree whole, so a task with AOSP task copies builds from a link farm (`Layout::aosp_tree`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::aosp::{self, Manifest};
use crate::git::git;
use crate::sources::{Checkout, Pin};
use crate::workspace;

/// The trunk: a repo checkout on it has no task copies, its patches are what `src/` holds.
pub const MAIN: &str = "main";
const SHARED: &str = "src";
const TASKS: &str = "src-wt";
/// The AOSP link farm inside a task's directory.
const FARM: &str = "aosp-tree";

pub struct Layout {
    src: PathBuf,
    /// Task name (the branch) and `src-wt/<task>`; None on main or a detached HEAD.
    task: Option<(String, PathBuf)>,
}

/// A checkout as the resolver places it.
pub struct Located {
    pub dir: PathBuf,
    /// The task owning it; None = the shared `src/` checkout.
    pub task: Option<String>,
}

impl Layout {
    /// The layout of the repo checkout at `root`.
    pub fn of(root: &Path) -> Result<Self> {
        let branch = git(Some(root), &["symbolic-ref", "-q", "--short", "HEAD"]).ok();
        Ok(Self::new(&workspace::root_dir(root)?, branch.as_deref()))
    }

    /// Workspace `ws` as a repo checkout on `branch` sees it (None = detached HEAD).
    pub fn new(ws: &Path, branch: Option<&str>) -> Self {
        Layout {
            src: ws.join(SHARED),
            task: branch
                .filter(|b| *b != MAIN)
                .map(|b| (b.to_owned(), ws.join(TASKS).join(b))),
        }
    }

    /// The shared checkouts' root: only fetch's own files (the AOSP manifest) and `xtask soong`'s tree are reached through it.
    pub fn src(&self) -> &Path {
        &self.src
    }

    /// The shared AOSP tree, `src/aosp`.
    pub fn shared_tree(&self) -> PathBuf {
        self.src.join("aosp")
    }

    /// The checkout of `c` for this repo checkout: its task copy if one exists, else `src/<up>`.
    pub fn checkout(&self, c: &Checkout) -> Located {
        if let Some((name, dir)) = &self.task {
            let copy = dir.join(&c.dir);
            if copy.join(".git").exists() {
                return Located {
                    dir: copy,
                    task: Some(name.clone()),
                };
            }
        }
        Located {
            dir: self.src.join(&c.dir),
            task: None,
        }
    }

    /// For `xtask patch --edit / --done` only: `src/<up>`, the task copy's place, the task name.
    pub fn edit_paths(&self, c: &Checkout) -> Result<(PathBuf, PathBuf, &str)> {
        let (name, dir) = self.task.as_ref().with_context(|| {
            format!(
                "the repo checkout is on {MAIN} or detached: task copies belong to a task branch"
            )
        })?;
        ensure!(
            !name.contains('/'),
            "branch {name} holds `/`, and task copies live in {TASKS}/<branch>/: rename the branch"
        );
        Ok((self.src.join(&c.dir), dir.join(&c.dir), name))
    }

    /// Remove the empty directories between a removed task copy and `src-wt/` (the task's own included).
    pub fn remove_empty_parents(&self, copy: &Path) {
        let Some((_, task_dir)) = &self.task else {
            return;
        };
        let stop = task_dir.parent().unwrap_or(task_dir);
        for dir in copy.ancestors().skip(1).take_while(|d| *d != stop) {
            if std::fs::remove_dir(dir).is_err() {
                break;
            }
        }
    }

    /// The AOSP tree this repo checkout builds from: `src/aosp`, or, with task copies of AOSP projects, the link farm `src-wt/<task>/aosp-tree`, regenerated on every call (stale entries removed; deleted once no task copy is left).
    /// The farm: real directories along each edited project's ancestor path; every other entry an absolute link into `src/aosp`, the edited project an absolute link to its task copy; and the manifest's link/copy files landing in a real directory as relative links through the farm (a copyfile too), so they reach a task copy as well. Absolute = leaves the tree: the converter follows only those (bp2ninja walk.go).
    /// Limitation: a source's `#include "../.."` that leaves an edited project resolves physically, inside `src-wt/`, not through the farm.
    pub fn aosp_tree(
        &self,
        checkouts: &[Checkout],
        manifest: Option<&Manifest>,
    ) -> Result<PathBuf> {
        let shared = self.shared_tree();
        let Some((_, task_dir)) = &self.task else {
            return Ok(shared);
        };
        let farm = task_dir.join(FARM);
        let pinned: Vec<&str> = checkouts
            .iter()
            .filter_map(|c| match &c.pin {
                Pin::Aosp(p) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        let edited: BTreeMap<PathBuf, PathBuf> = checkouts
            .iter()
            .filter_map(|c| match &c.pin {
                Pin::Aosp(p) => {
                    let l = self.checkout(c);
                    l.task.is_some().then(|| (PathBuf::from(p), l.dir))
                }
                _ => None,
            })
            .collect();
        if edited.is_empty() {
            if farm.symlink_metadata().is_ok() {
                std::fs::remove_dir_all(&farm)
                    .with_context(|| format!("removing {}", farm.display()))?;
                // the task's directory, once the farm was all it held
                let _ = std::fs::remove_dir(task_dir);
            }
            return Ok(shared);
        }
        // manifest links of every present project: (dest, project path / src), tree-relative
        let links: Vec<(String, PathBuf)> = manifest
            .map(|m| {
                m.projects
                    .iter()
                    .filter(|(p, _)| {
                        edited.contains_key(Path::new(p.as_str())) || shared.join(p).is_dir()
                    })
                    .flat_map(|(p, pr)| {
                        pr.links
                            .iter()
                            .map(move |l| (l.dest.clone(), Path::new(p).join(&l.src)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let want = farm_entries(&shared, &edited, &pinned, &links)?;
        reconcile(&farm, &want)?;
        Ok(farm)
    }
}

/// What a farm directory holds under one name.
#[derive(Debug, PartialEq)]
enum Entry {
    Dir,
    Link(PathBuf),
}

/// The farm's real directories (tree-relative, "" = its root), each with the entries it must hold.
fn farm_entries(
    shared: &Path,
    edited: &BTreeMap<PathBuf, PathBuf>,
    pinned: &[&str],
    links: &[(String, PathBuf)],
) -> Result<BTreeMap<PathBuf, BTreeMap<OsString, Entry>>> {
    for p in edited.keys() {
        // the edited project is one link to its task copy: a project checked out inside it would vanish
        if let Some(inner) = pinned
            .iter()
            .find(|q| Path::new(q).starts_with(p) && Path::new(q) != p)
        {
            bail!(
                "{inner} is a project inside the edited {}: the link farm cannot hold both",
                p.display()
            );
        }
    }
    let real: BTreeSet<PathBuf> = edited
        .keys()
        .flat_map(|p| p.ancestors().skip(1).map(Path::to_path_buf))
        .collect();
    let mut out: BTreeMap<PathBuf, BTreeMap<OsString, Entry>> = BTreeMap::new();
    for d in &real {
        let mut entries = BTreeMap::new();
        if let Ok(dir) = std::fs::read_dir(shared.join(d)) {
            for e in dir {
                let name = e?.file_name();
                let target = shared.join(d).join(&name);
                entries.insert(name, Entry::Link(target));
            }
        }
        out.insert(d.clone(), entries);
    }
    let mut set = |path: &Path, entry: Entry| {
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
            && let Some(entries) = out.get_mut(parent)
        {
            entries.insert(name.to_owned(), entry);
        }
    };
    for d in real.iter().filter(|d| !d.as_os_str().is_empty()) {
        set(d, Entry::Dir);
    }
    for (p, copy) in edited {
        set(p, Entry::Link(copy.clone()));
    }
    for (dest, target) in links {
        let dest = Path::new(dest);
        if !real.contains(dest) && !edited.contains_key(dest) {
            set(dest, Entry::Link(aosp::relative(dest, target)));
        }
    }
    Ok(out)
}

/// Make `farm` hold exactly `want`: entries kept where already right, everything else replaced or removed. Returns the number of changes.
fn reconcile(farm: &Path, want: &BTreeMap<PathBuf, BTreeMap<OsString, Entry>>) -> Result<usize> {
    let mut changed = 0;
    // parents first (PathBuf orders by component)
    for (d, entries) in want {
        let dir = farm.join(d);
        if dir.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
            std::fs::remove_file(&dir)?;
            changed += 1;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let path = e.path();
            let kind = e.file_type()?;
            let keep = match entries.get(&e.file_name()) {
                Some(Entry::Dir) => kind.is_dir(),
                Some(Entry::Link(t)) => {
                    kind.is_symlink() && std::fs::read_link(&path).is_ok_and(|l| l == *t)
                }
                None => false,
            };
            if !keep {
                // never follows a link: a farm holds only links and the directories made here
                if kind.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                } else {
                    std::fs::remove_file(&path)?;
                }
                changed += 1;
            }
        }
        for (name, entry) in entries {
            let path = dir.join(name);
            if path.symlink_metadata().is_ok() {
                continue;
            }
            match entry {
                Entry::Dir => std::fs::create_dir(&path)?,
                Entry::Link(t) => std::os::unix::fs::symlink(t, &path)
                    .with_context(|| format!("linking {}", path.display()))?,
            }
            changed += 1;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(path: &str) -> Checkout {
        Checkout {
            dir: Path::new("aosp").join(path),
            url: None,
            tag: "t".into(),
            commit: "c".into(),
            pin: Pin::Aosp(path.into()),
            licence_key: "aosp".into(),
        }
    }

    fn link(p: &Path) -> PathBuf {
        std::fs::read_link(p).unwrap()
    }

    #[test]
    fn resolver_prefers_the_task_copy() {
        let ws = std::env::temp_dir().join(format!("xtask-layout-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let c = checkout("a/b");
        let task = Layout::new(&ws, Some("t"));
        assert_eq!(task.checkout(&c).dir, ws.join("src/aosp/a/b"));
        std::fs::create_dir_all(ws.join("src-wt/t/aosp/a/b")).unwrap();
        // a directory without .git is no checkout
        assert!(task.checkout(&c).task.is_none());
        std::fs::write(ws.join("src-wt/t/aosp/a/b/.git"), "gitdir: x\n").unwrap();
        let l = task.checkout(&c);
        assert_eq!(
            (l.dir, l.task.as_deref()),
            (ws.join("src-wt/t/aosp/a/b"), Some("t"))
        );
        // main and a detached HEAD never see task copies
        assert!(Layout::new(&ws, Some(MAIN)).checkout(&c).task.is_none());
        assert!(Layout::new(&ws, None).checkout(&c).task.is_none());
        assert!(Layout::new(&ws, None).edit_paths(&c).is_err());
        let e = format!(
            "{:#}",
            Layout::new(&ws, Some("feat/x")).edit_paths(&c).unwrap_err()
        );
        assert!(e.contains("rename the branch"), "{e}");
        std::fs::remove_dir_all(&ws).unwrap();
    }

    /// Shared tree: a/b (edited), a/c, x/; root file r and manifest link L -> a/b/l; task copy of a/b.
    #[test]
    fn farm_follows_task_copies() {
        let ws = std::env::temp_dir().join(format!("xtask-layout-farm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let tree = ws.join("src/aosp");
        for d in ["a/b", "a/c", "x"] {
            std::fs::create_dir_all(tree.join(d)).unwrap();
        }
        std::fs::write(tree.join("r"), "").unwrap();
        std::os::unix::fs::symlink("a/b/l", tree.join("L")).unwrap();
        let copy = ws.join("src-wt/t/aosp/a/b");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join(".git"), "gitdir: x\n").unwrap();
        let all = [checkout("a/b"), checkout("a/c"), checkout("x")];
        let m = crate::aosp::parse(r#"<manifest><default revision="r"/><project path="a/b" name="b"><linkfile src="l" dest="L"/></project></manifest>"#).unwrap();
        let layout = Layout::new(&ws, Some("t"));
        let farm = layout.aosp_tree(&all, Some(&m)).unwrap();
        assert_eq!(farm, ws.join("src-wt/t/aosp-tree"));
        // real directories on the edited project's ancestor path only
        assert!(!farm.join("a").symlink_metadata().unwrap().is_symlink());
        assert_eq!(link(&farm.join("a/b")), copy);
        assert_eq!(link(&farm.join("a/c")), tree.join("a/c"));
        assert_eq!(link(&farm.join("x")), tree.join("x"));
        assert_eq!(link(&farm.join("r")), tree.join("r"));
        // the manifest link goes through the farm, so it reaches the task copy
        assert_eq!(link(&farm.join("L")), PathBuf::from("a/b/l"));
        // idempotent
        let want = farm_entries(
            &tree,
            &BTreeMap::from([(PathBuf::from("a/b"), copy.clone())]),
            &["a/b", "a/c", "x"],
            &[("L".into(), PathBuf::from("a/b/l"))],
        )
        .unwrap();
        assert_eq!(reconcile(&farm, &want).unwrap(), 0);
        // stale: an entry gone from src/aosp, a foreign file, a wrong link
        std::fs::remove_dir(tree.join("x")).unwrap();
        std::fs::write(farm.join("a/junk"), "").unwrap();
        std::fs::remove_file(farm.join("a/c")).unwrap();
        std::os::unix::fs::symlink("/nowhere", farm.join("a/c")).unwrap();
        layout.aosp_tree(&all, Some(&m)).unwrap();
        assert!(farm.join("x").symlink_metadata().is_err());
        assert!(farm.join("a/junk").symlink_metadata().is_err());
        assert_eq!(link(&farm.join("a/c")), tree.join("a/c"));
        // a pinned project inside the edited one cannot be shown
        let nested = [checkout("a/b"), checkout("a/b/n")];
        let e = format!("{:#}", layout.aosp_tree(&nested, None).unwrap_err());
        assert!(
            e.contains("a/b/n is a project inside the edited a/b"),
            "{e}"
        );
        // the task copy gone: back to src/aosp, the farm deleted
        std::fs::remove_dir_all(ws.join("src-wt/t/aosp")).unwrap();
        assert_eq!(layout.aosp_tree(&all, Some(&m)).unwrap(), tree);
        assert!(farm.symlink_metadata().is_err());
        assert!(!ws.join("src-wt/t").exists());
        std::fs::remove_dir_all(&ws).unwrap();
    }
}
