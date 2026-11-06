// SPDX-License-Identifier: MIT
//! The source state a native out dir was built from, so a runtime gate refuses artifacts from another tree (a rebase without `xtask native` once cost a whole round).
//!
//! The state is the checkout's working tree as git sees it (tracked + untracked, ignored excluded), hashed through a scratch index: `git add -A` into a copy of the index,
//! `git write-tree`. The top-level entries listed in [`GATE_SIDE`] are left out of the hash. `native` deletes the record when it starts and writes it once ninja and the converter
//! packages succeeded (the state taken before the build, so an edit during the build reads as a mismatch); a gate compares it with the checkout's state now.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

/// The record, in the out dir.
const RECORD: &str = "built-from";
/// Top-level entries native's artifacts do not come from: `xtask/` is the gate side (gate code and the fixtures each gate builds itself), so editing a gate needs no native run.
/// Trade-off: native's own code (xtask/src/native.rs, the converter driver) is in it too; a change there reaches the slot only through a native run anyway.
const GATE_SIDE: &[&str] = &["xtask"];

/// The checkout's source state: the hash of its top-level tree entries, [`GATE_SIDE`] left out.
pub fn of_checkout(root: &Path) -> Result<String> {
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"], None)?);
    let scratch = std::env::temp_dir().join(format!("xtask-build-state-{}", std::process::id()));
    let index = git_dir.join("index");
    if index.exists() {
        std::fs::copy(&index, &scratch).context("copying the git index")?;
    }
    let result = (|| {
        git(root, &["add", "-A"], Some(&scratch))?;
        let tree = git(root, &["write-tree"], Some(&scratch))?;
        let listing = git(root, &["ls-tree", &tree], None)?;
        let kept: Vec<&str> = listing
            .lines()
            .filter(|l| {
                !l.split('\t')
                    .nth(1)
                    .is_some_and(|name| GATE_SIDE.contains(&name))
            })
            .collect();
        git_stdin(root, &["mktree"], &format!("{}\n", kept.join("\n")))
    })();
    let _ = std::fs::remove_file(&scratch);
    result
}

/// `native` starts: the out dir no longer holds a known build.
pub fn forget(out: &Path) -> Result<()> {
    match std::fs::remove_file(out.join(RECORD)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).context("removing the build record")
        }
        _ => Ok(()),
    }
}

/// `native` built the crates from `state`.
pub fn record(out: &Path, state: &str) -> Result<()> {
    std::fs::write(out.join(RECORD), format!("{state}\n")).context("writing the build record")
}

/// A runtime gate's native out dir (`--out`, else the slot): refused unless built from the checkout at `root` as it is now.
pub fn gate_out(root: &Path, explicit: Option<&Path>, host: &str) -> Result<PathBuf> {
    let out = crate::slot::out_dir(root, explicit, "--out", |workspace| {
        workspace.join("out/native").join(host)
    })?;
    ensure_current(root, &out)?;
    Ok(out)
}

/// A gate's guard: `out` was built from the checkout at `root` as it is now.
pub fn ensure_current(root: &Path, out: &Path) -> Result<()> {
    ensure_matches(&of_checkout(root)?, out)
}

fn ensure_matches(now: &str, out: &Path) -> Result<()> {
    let recorded = match std::fs::read_to_string(out.join(RECORD)) {
        Ok(s) => s.trim().to_owned(),
        Err(_) => bail!(
            "{} holds no record of the tree it was built from (a native run never finished there): run `cargo xtask native`",
            out.display()
        ),
    };
    ensure!(
        recorded == now,
        "{} was built from another source state ({recorded}, the checkout is {now}): run `cargo xtask native`",
        out.display()
    );
    Ok(())
}

fn git(dir: &Path, args: &[&str], index: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let out = cmd
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    ensure!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git_stdin(dir: &Path, args: &[&str], input: &str) -> Result<String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    child
        .stdin
        .take()
        .context("git stdin")?
        .write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("xtask-build-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("crates")).unwrap();
        std::fs::create_dir_all(d.join("xtask")).unwrap();
        std::fs::write(d.join("crates/a.rs"), "a").unwrap();
        std::fs::write(d.join("xtask/g.rs"), "g").unwrap();
        git(&d, &["init", "-q"], None).unwrap();
        d
    }

    /// The guard fires with no record and after a source edit (untracked, uncommitted: the rebase case is the same tree change), stays quiet on the recorded state,
    /// and ignores the gate side; the scratch index leaves the real one untouched.
    #[test]
    fn guard_fires_on_another_tree() {
        let d = repo("guard");
        let out = d.with_extension("out");
        std::fs::create_dir_all(&out).unwrap();
        let e = ensure_current(&d, &out).expect_err("no record");
        assert!(format!("{e}").contains("run `cargo xtask native`"), "{e}");
        forget(&out).unwrap();
        record(&out, &of_checkout(&d).unwrap()).unwrap();
        ensure_current(&d, &out).expect("built from this state");
        std::fs::write(d.join("xtask/g.rs"), "gate edit").unwrap();
        ensure_current(&d, &out).expect("gate side is not native input");
        std::fs::write(d.join("crates/a.rs"), "edited").unwrap();
        let e = ensure_current(&d, &out).expect_err("source edited");
        assert!(
            format!("{e}").contains("built from another source state"),
            "{e}"
        );
        let status = git(&d, &["status", "--porcelain"], None).unwrap();
        assert!(
            status.lines().all(|l| l.starts_with("??")),
            "the real index stays empty: {status}"
        );
        forget(&out).unwrap();
        assert!(
            ensure_current(&d, &out).is_err(),
            "a started native run forgets"
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&out);
    }
}
