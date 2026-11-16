// SPDX-License-Identifier: MIT
//! Checkout stamps: the exact mtimes of the land checkout a base was built against, given back to a new build dir's checkout where the content is the same.
//!
//! Cargo and ninja key freshness on mtimes: cargo compares a package's max source mtime by equality (`PrecalculatedComponents`) and each source against its dep-info (`ChangedFile`), ninja's glob rechecks compare dir mtimes.
//! A fresh checkout's files are all `now`, newer than every output a base carries, so a child at the base's commit rebuilt ~61 % of the workspace units (R7 probe).
//! [`record`] (publish) writes per tracked path mode + blob + mtime, per dir a hash of its entry names + mtime; [`restore`] (a new build dir) gives the recorded mtime back only to a path whose HEAD mode + blob equal the record and whose worktree equals HEAD, and to a dir whose entry names hash equal.
//! Invariant: a path whose content differs from what the base was built from never gets an old mtime -- that would make a stale output fresh (false green). Every other path, dir, dirty and untracked file gets now, newer than every copied output: its own mtime may predate the base (an edit committed before main's base was published, untouched by the rebase).
//! Restore is safe only into a dir that holds nothing but the base's outputs: outputs carried in from elsewhere (refresh's kept cargo roots, recopy's gate state) may have been built after the recorded time from other content.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};
use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, UTIME_NOW, UTIME_OMIT, utimensat};

use crate::git::git_raw;
use crate::test_apks::sha256_hex;

/// In a base dir: the stamps of the checkout it was built from.
pub const FILE: &str = "checkout.stamps";
/// The checkout root's path in the record.
const ROOT: &str = ".";

/// A recorded mtime: seconds + nanoseconds, exact.
type Mtime = (i64, i64);

/// The HEAD entries of `checkout` whose worktree content equals HEAD: path -> (mode, blob).
/// Dirty: `git diff HEAD` (index and worktree, content-checked where the stat differs) plus every path git does not stat (assume-unchanged, skip-worktree): an edit there is invisible to diff.
fn clean_entries(checkout: &Path) -> Result<BTreeMap<String, (String, String)>> {
    let mut dirty: BTreeSet<String> = git_raw(
        checkout,
        &["diff", "HEAD", "--name-only", "-z", "--no-renames"],
    )?
    .split('\0')
    .filter(|p| !p.is_empty())
    .map(str::to_owned)
    .collect();
    for line in git_raw(checkout, &["ls-files", "-v", "-z"])?.split('\0') {
        if let Some((tag, path)) = line.split_once(' ')
            && tag != "H"
        {
            dirty.insert(path.to_owned());
        }
    }
    let mut out = BTreeMap::new();
    for line in git_raw(checkout, &["ls-tree", "-r", "-z", "--full-tree", "HEAD"])?.split('\0') {
        // `<mode> <type> <object>\t<path>`
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let mut f = meta.split(' ');
        let (Some(mode), Some(kind), Some(blob)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        // submodule commits are dirs; a path the line format cannot hold is never restored
        if kind != "blob" || dirty.contains(path) || path.contains(['\n', '\t']) {
            continue;
        }
        out.insert(path.to_owned(), (mode.to_owned(), blob.to_owned()));
    }
    Ok(out)
}

/// Every dir holding a tracked path, plus [`ROOT`].
fn dirs_of<'a>(paths: impl Iterator<Item = &'a String>) -> BTreeSet<String> {
    let mut dirs = BTreeSet::from([ROOT.to_owned()]);
    for p in paths {
        let mut cur = Path::new(p.as_str());
        while let Some(parent) = cur.parent().filter(|d| !d.as_os_str().is_empty()) {
            if !dirs.insert(parent.to_string_lossy().into_owned()) {
                break;
            }
            cur = parent;
        }
    }
    dirs
}

/// Hash of a dir's entry names (sorted, every entry incl. untracked): equal -> a glob over it lists the same names.
fn entry_hash(dir: &Path) -> Result<String> {
    let mut names: Vec<Vec<u8>> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|e| e.map(|e| e.file_name().into_encoded_bytes()))
        .collect::<std::io::Result<_>>()?;
    names.sort();
    Ok(sha256_hex(&names.join(&0u8))[..32].to_owned())
}

fn mtime_of(path: &Path) -> Result<Mtime> {
    let m =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    Ok((m.mtime(), m.mtime_nsec()))
}

fn set_mtime(path: &Path, (sec, nsec): Mtime) -> Result<()> {
    set_mtime_ts(
        path,
        Timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        },
    )
}

fn set_mtime_ts(path: &Path, last_modification: Timespec) -> Result<()> {
    let ts = Timestamps {
        last_access: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        last_modification,
    };
    utimensat(CWD, path, &ts, AtFlags::SYMLINK_NOFOLLOW)
        .with_context(|| format!("setting the mtime of {}", path.display()))
}

fn at(checkout: &Path, rel: &str) -> std::path::PathBuf {
    if rel == ROOT {
        checkout.to_owned()
    } else {
        checkout.join(rel)
    }
}

/// The stamps of `checkout` as it is now. Lines: `f <mode> <blob> <sec> <nsec> <path>`, `d <entry hash> <sec> <nsec> <path>`.
pub fn record(checkout: &Path) -> Result<String> {
    let entries = clean_entries(checkout)?;
    let mut text = String::new();
    for (path, (mode, blob)) in &entries {
        let (s, n) = mtime_of(&checkout.join(path))?;
        text.push_str(&format!("f {mode} {blob} {s} {n} {path}\n"));
    }
    for dir in dirs_of(entries.keys()) {
        let p = at(checkout, &dir);
        let (s, n) = mtime_of(&p)?;
        text.push_str(&format!("d {} {s} {n} {dir}\n", entry_hash(&p)?));
    }
    Ok(text)
}

/// `git <args> -z` output as paths.
fn paths(checkout: &Path, args: &[&str]) -> Result<BTreeSet<String>> {
    Ok(git_raw(checkout, args)?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Dirty against HEAD: index or worktree differs (content-checked).
fn dirty_now(checkout: &Path) -> Result<BTreeSet<String>> {
    paths(
        checkout,
        &["diff", "HEAD", "--name-only", "-z", "--no-renames"],
    )
}

/// Untracked, not ignored: a new source a glob or a build file may pick up.
fn untracked(checkout: &Path) -> Result<BTreeSet<String>> {
    paths(
        checkout,
        &["ls-files", "-o", "--exclude-standard", "-z", "--full-name"],
    )
}

/// Sets `path`'s mtime to now (no-follow); a path gone meanwhile is skipped. Returns whether it was set.
fn set_now(path: &Path) -> Result<bool> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(false);
    }
    set_mtime_ts(
        path,
        Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_NOW,
        },
    )?;
    Ok(true)
}

/// Gives `checkout` the recorded mtimes where the content is the recorded one, then `now` to every other tracked path and dir, every dirty path and every untracked file (module doc). `stamps` empty = no record: everything gets now. Returns (restored, recorded) path counts. Ends with `git update-index --refresh` so the index stat matches again.
/// Why now and not "keep its own": after a base copy every output carries the base's build time; a checkout path left at an older own mtime (a committed edit the rebase did not rewrite) would read as older than the outputs built from main's content -> false green.
pub fn restore(checkout: &Path, stamps: &str) -> Result<(usize, usize)> {
    let entries = clean_entries(checkout)?;
    let mut restored = BTreeSet::new();
    let mut total = 0;
    let mut dirs = Vec::new();
    for line in stamps.lines() {
        total += 1;
        let f: Vec<&str> = line.splitn(6, ' ').collect();
        match f.as_slice() {
            ["f", mode, blob, s, n, path] => {
                if entries
                    .get(*path)
                    .is_some_and(|(m, b)| m == mode && b == blob)
                {
                    set_mtime(&checkout.join(path), (s.parse()?, n.parse()?))?;
                    restored.insert((*path).to_owned());
                }
            }
            ["d", hash, s, n, path] => dirs.push((*hash, (s.parse()?, n.parse()?), *path)),
            _ => anyhow::bail!("{FILE}: malformed line {line:?}"),
        }
    }
    // dirs after files; utimensat changes no parent's mtime, so their order is free
    for (hash, mtime, path) in dirs {
        let p = at(checkout, path);
        if p.is_dir() && !p.is_symlink() && entry_hash(&p)? == hash {
            set_mtime(&p, mtime)?;
            restored.insert(path.to_owned());
        }
    }
    let done = restored.len();
    // everything not restored -> now: tracked (index), HEAD paths, dirty, untracked, and every dir holding one
    let mut rest = paths(checkout, &["ls-files", "-z", "--full-name"])?;
    rest.extend(entries.into_keys());
    rest.extend(dirty_now(checkout)?);
    rest.extend(untracked(checkout)?);
    let all_dirs = dirs_of(rest.iter());
    for p in rest.iter().chain(&all_dirs) {
        if !restored.contains(p) {
            set_now(&at(checkout, p))?;
        }
    }
    // a write during the loop above left new content under a restored mtime: git sees it by ctime/content
    for p in dirty_now(checkout)?.intersection(&restored) {
        set_now(&checkout.join(p))?;
    }
    git_raw(checkout, &["update-index", "-q", "--refresh"]).ok();
    Ok((done, total))
}

/// After a base copy that keeps no record (refresh): every path whose content differs from commit `base` (index or worktree), every untracked file, and their dirs get now, so they read newer than the base's outputs. Returns the count set.
pub fn touch_changed_since(checkout: &Path, base: &str) -> Result<usize> {
    let mut changed = paths(
        checkout,
        &["diff", base, "--name-only", "-z", "--no-renames"],
    )?;
    changed.extend(untracked(checkout)?);
    let dirs = dirs_of(changed.iter());
    let mut n = 0;
    for p in changed.iter().chain(&dirs) {
        n += usize::from(set_now(&at(checkout, p))?);
    }
    git_raw(checkout, &["update-index", "-q", "--refresh"]).ok();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn g(dir: &Path, args: &[&str]) {
        let st = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .expect("git");
        assert!(st.success(), "git {args:?}");
    }

    /// A committed repo `land` (a/x.rs, a/y.rs, b/z.rs, top.toml) and its clone `child`, whose files all get the clone's `now`.
    fn pair(name: &str) -> (PathBuf, PathBuf) {
        let d = std::env::temp_dir().join(format!("xtask-stamps-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let land = d.join("land");
        for (p, c) in [
            ("a/x.rs", "x"),
            ("a/y.rs", "y"),
            ("b/z.rs", "z"),
            ("top.toml", "t"),
        ] {
            let f = land.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, c).unwrap();
        }
        g(&land, &["init", "-q"]);
        g(&land, &["add", "-A"]);
        g(&land, &["commit", "-qm", "base"]);
        // land's tree "built" long ago: an mtime no clone can have
        for p in ["a/x.rs", "a/y.rs", "b/z.rs", "top.toml", "a", "b", "."] {
            set_mtime(&land.join(p), (1_000_000_000, 123_456_789)).unwrap();
        }
        g(&d, &["clone", "-q", "land", "child"]);
        (land, d.join("child"))
    }

    const OLD: Mtime = (1_000_000_000, 123_456_789);

    fn m(p: &Path) -> Mtime {
        mtime_of(p).unwrap()
    }

    /// Clean files and dirs get the exact recorded mtime; the git index is clean afterwards.
    #[test]
    fn a_clean_clone_gets_the_exact_recorded_mtimes() {
        let (land, child) = pair("clean");
        let stamps = record(&land).unwrap();
        let (done, total) = restore(&child, &stamps).unwrap();
        // 4 files + a, b, root
        assert_eq!((done, total), (7, 7));
        for p in ["a/x.rs", "b/z.rs", "top.toml", "a", "b"] {
            assert_eq!(m(&child.join(p)), OLD, "{p}");
        }
        assert_eq!(m(&child), OLD);
        let st = std::process::Command::new("git")
            .current_dir(&child)
            .args(["diff", "--quiet"])
            .status()
            .unwrap();
        assert!(st.success());
    }

    /// The false-green trap: an edited file (committed or not), and a staged one, keep their own mtime; so does a dir that gained an entry.
    #[test]
    fn a_path_with_other_content_stays_dirty() {
        let (land, child) = pair("dirty");
        let stamps = record(&land).unwrap();
        // committed edit: HEAD blob differs from the record
        std::fs::write(child.join("a/x.rs"), "x2").unwrap();
        g(&child, &["commit", "-qam", "edit"]);
        // worktree edit, same size: only a content check sees it
        std::fs::write(child.join("a/y.rs"), "Y").unwrap();
        // staged edit
        std::fs::write(child.join("top.toml"), "t2").unwrap();
        g(&child, &["add", "top.toml"]);
        // a new entry in b/
        std::fs::write(child.join("b/new.rs"), "n").unwrap();
        let before: Vec<Mtime> = ["a/x.rs", "a/y.rs", "top.toml", "b"]
            .iter()
            .map(|p| m(&child.join(p)))
            .collect();
        let (done, _) = restore(&child, &stamps).unwrap();
        // b/z.rs, a, root
        assert_eq!(done, 3);
        assert_eq!(m(&child.join("b/z.rs")), OLD);
        for (p, was) in ["a/x.rs", "a/y.rs", "top.toml", "b", "b/new.rs"]
            .iter()
            .zip(before.into_iter().chain([(0, 0)]))
        {
            let now = m(&child.join(p));
            assert_ne!(now, OLD, "{p} got the old mtime");
            assert!(now >= was, "{p} went back in time");
        }
    }

    /// A committed edit older than the base (F at 10:00, base built 11:05, rebase left F alone): every path not restored gets now, past any output.
    #[test]
    fn an_old_committed_edit_gets_now() {
        let (land, child) = pair("oldedit");
        let stamps = record(&land).unwrap();
        std::fs::write(child.join("a/x.rs"), "x2").unwrap();
        g(&child, &["commit", "-qam", "edit"]);
        std::fs::write(child.join("b/new.rs"), "n").unwrap();
        let ten = (OLD.0 + 3600, 0);
        for p in ["a/x.rs", "b/new.rs", "b"] {
            set_mtime(&child.join(p), ten).unwrap();
        }
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        restore(&child, &stamps).unwrap();
        for p in ["a/x.rs", "b/new.rs", "b"] {
            assert!(m(&child.join(p)).0 >= before, "{p} kept an old mtime");
        }
        assert_eq!(m(&child.join("a/y.rs")), OLD);
    }

    /// Refresh's rule: paths differing from the base commit, untracked files and their dirs get now; equal ones keep theirs.
    #[test]
    fn touch_changed_since_marks_only_the_delta() {
        let (_, child) = pair("since");
        let base = git_raw(&child, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(child.join("a/x.rs"), "x2").unwrap();
        g(&child, &["commit", "-qam", "edit"]);
        std::fs::write(child.join("b/new.rs"), "n").unwrap();
        for p in ["a/x.rs", "a/y.rs", "b/new.rs", "b", "a"] {
            set_mtime(&child.join(p), OLD).unwrap();
        }
        // a/x.rs, b/new.rs, a, b, root
        assert_eq!(touch_changed_since(&child, base.trim()).unwrap(), 5);
        for p in ["a/x.rs", "b/new.rs", "a", "b"] {
            assert_ne!(m(&child.join(p)), OLD, "{p}");
        }
        assert_eq!(m(&child.join("a/y.rs")), OLD);
    }

    /// A path git does not stat (assume-unchanged) is never trusted clean, even when diff calls it so.
    #[test]
    fn an_assume_unchanged_edit_stays_dirty() {
        let (land, child) = pair("assume");
        let stamps = record(&land).unwrap();
        g(&child, &["update-index", "--assume-unchanged", "a/x.rs"]);
        std::fs::write(child.join("a/x.rs"), "X").unwrap();
        restore(&child, &stamps).unwrap();
        assert_ne!(m(&child.join("a/x.rs")), OLD);
    }

    /// Record skips land's own dirty paths, so nothing of them is restored.
    #[test]
    fn land_dirty_paths_are_not_recorded() {
        let (land, _) = pair("landdirty");
        std::fs::write(land.join("a/x.rs"), "dirty").unwrap();
        let stamps = record(&land).unwrap();
        assert!(
            !stamps
                .lines()
                .any(|l| l.starts_with('f') && l.ends_with(" a/x.rs"))
        );
        assert!(stamps.lines().any(|l| l.ends_with(" a/y.rs")));
    }

    /// End to end with cargo (review F1): the child commits an edit at "10:00", main's base is built later at the same canonical path, the child gets a fresh build dir copied from the base + restore -> cargo must recompile the edit.
    #[test]
    fn cargo_rebuilds_an_edit_older_than_the_base() {
        let d = std::env::temp_dir().join(format!("xtask-stamps-cargo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let src = d.join("src");
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::write(
            src.join("Cargo.toml"),
            "[package]\nname = \"fg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            src.join("src/lib.rs"),
            "mod other;\npub fn v() -> u32 { 1 }\n",
        )
        .unwrap();
        std::fs::write(src.join("src/other.rs"), "").unwrap();
        g(&src, &["init", "-q"]);
        g(&src, &["add", "-A"]);
        g(&src, &["commit", "-qm", "main"]);
        // 10:00: the child commits its edit; the clone is not at the canonical path yet
        g(&d, &["clone", "-q", "src", "child"]);
        let child = d.join("child");
        std::fs::write(
            child.join("src/lib.rs"),
            "mod other;\npub fn v() -> u32 { 2 }\n",
        )
        .unwrap();
        g(&child, &["commit", "-qam", "edit"]);
        let ten = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 7200;
        set_mtime(&child.join("src/lib.rs"), (ten, 0)).unwrap();
        // 11:05: land builds main at the canonical path `work` and publishes stamps + outputs
        let (work, target) = (d.join("work"), d.join("target"));
        g(&d, &["clone", "-q", "src", "work"]);
        let cargo = |dir: &Path| -> String {
            let o = std::process::Command::new(std::env::var("CARGO").unwrap_or("cargo".into()))
                .current_dir(dir)
                .args(["build", "-v", "--offline"])
                .env("CARGO_TARGET_DIR", &target)
                .env_remove("RUSTC_WRAPPER")
                .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
                .output()
                .expect("cargo");
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            String::from_utf8_lossy(&o.stderr).into_owned()
        };
        assert!(cargo(&work).contains("Compiling fg"));
        let stamps = record(&work).unwrap();
        // a fresh build dir for the child: same canonical path, the base's outputs, then restore
        std::fs::rename(&work, d.join("land")).unwrap();
        std::fs::rename(&child, &work).unwrap();
        let (done, _) = restore(&work, &stamps).unwrap();
        // the S1 gain stays: Cargo.toml, src/other.rs restored
        assert!(done >= 2, "restored {done}");
        let log = cargo(&work);
        assert!(
            log.contains("Compiling fg"),
            "false green, cargo said:\n{log}"
        );
    }

    #[test]
    fn a_malformed_record_is_an_error() {
        let (_, child) = pair("bad");
        assert!(restore(&child, "x y\n").is_err());
    }
}
