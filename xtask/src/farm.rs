// SPDX-License-Identifier: MIT
//! The AOSP tree a repo checkout builds from. The converter takes the tree whole, so a task with AOSP task copies builds from a link farm over `src/aosp` and them; every checkout is placed by `atl_layout::Layout`, the one resolver.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use atl_layout::Layout;

use crate::aosp::{self, Manifest};
use crate::sources::{Checkout, Pin};

/// The AOSP link farm inside a task's directory.
const FARM: &str = "aosp-tree";

/// The shared AOSP tree, `src/aosp`: its root holds no checkout of its own, so only the tree-wide readers (the farm, `xtask soong`, manifest links) take it whole.
pub fn shared_tree(layout: &Layout) -> PathBuf {
    layout.src().join("aosp")
}

/// The AOSP tree this repo checkout builds from: `src/aosp`, or, with task copies of AOSP projects, the link farm `src-wt/<task>/aosp-tree`, regenerated on every call (stale entries removed; deleted once no task copy is left).
/// The farm: real directories along each edited project's ancestor path; every other entry an absolute link into `src/aosp`, the edited project an absolute link to its task copy; and the manifest's link/copy files landing in a real directory as relative links through the farm (a copyfile too), so they reach a task copy as well. Absolute = leaves the tree: the converter follows only those (bp2ninja walk.go).
/// Limitation: a source's `#include "../.."` that leaves an edited project resolves physically, inside `src-wt/`, not through the farm.
pub fn aosp_tree(
    layout: &Layout,
    checkouts: &[Checkout],
    manifest: Option<&Manifest>,
) -> Result<PathBuf> {
    let shared = shared_tree(layout);
    let Some((_, task_dir)) = layout.task() else {
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
                let l = layout.locate(&c.dir);
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

    /// Shared tree: a/b (edited, a checkout), a/c, x/; root file r and manifest link L -> a/b/l; task copy of a/b.
    #[test]
    fn farm_follows_task_copies() {
        let ws = std::env::temp_dir().join(format!("xtask-layout-farm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let tree = ws.join("src/aosp");
        for d in ["a/b/.git", "a/c", "x"] {
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
        let farm = aosp_tree(&layout, &all, Some(&m)).unwrap();
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
        aosp_tree(&layout, &all, Some(&m)).unwrap();
        assert!(farm.join("x").symlink_metadata().is_err());
        assert!(farm.join("a/junk").symlink_metadata().is_err());
        assert_eq!(link(&farm.join("a/c")), tree.join("a/c"));
        // a pinned project inside the edited one cannot be shown
        let nested = [checkout("a/b"), checkout("a/b/n")];
        let e = format!("{:#}", aosp_tree(&layout, &nested, None).unwrap_err());
        assert!(
            e.contains("a/b/n is a project inside the edited a/b"),
            "{e}"
        );
        // the task copy gone: back to src/aosp, the farm deleted
        std::fs::remove_dir_all(ws.join("src-wt/t/aosp")).unwrap();
        assert_eq!(aosp_tree(&layout, &all, Some(&m)).unwrap(), tree);
        assert!(farm.symlink_metadata().is_err());
        assert!(!ws.join("src-wt/t").exists());
        std::fs::remove_dir_all(&ws).unwrap();
    }
}
