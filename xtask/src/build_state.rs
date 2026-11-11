// SPDX-License-Identifier: MIT
//! The source state a native out dir was built from, so a runtime gate refuses artifacts from another tree (a rebase without `xtask native` once cost a whole round).
//!
//! The state is the checkout's working tree as git sees it (tracked + untracked, ignored excluded), hashed through a scratch index: `git add -A` into a copy of the index,
//! `git write-tree`. The top-level entries listed in [`GATE_SIDE`] are left out of the hash. `native` deletes the record when it starts and writes it once ninja and the converter
//! packages succeeded (the state taken before the build, so an edit during the build reads as a mismatch); a gate compares it with the checkout's state now.
//! The record also lists the goals built from that state (line 1 state, then one goal per line, nb goals as `nb:<arch>:<module>`): a run from the same state adds its goals to the
//! previous record's, a run from another state starts over, since the older goals' outputs are stale. A gate refuses a slot lacking its goals (gate_goals.rs).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

/// The record, in the out dir.
const RECORD: &str = "built-from";
/// Top-level entries native's artifacts do not come from: `xtask/` and `xtask-harness/` are the gate side (gate code and the fixtures each gate builds itself), so editing a gate needs no native run.
/// `test-apks.toml` feeds only `run-apk`, `fetch --test-apks` and the `tables` check: a row or pin change needs no native run.
/// Trade-off: native's own code (xtask/src/native.rs, the converter driver) is in it too; a change there reaches the slot only through a native run anyway.
const GATE_SIDE: &[&str] = &["xtask", "xtask-harness", "test-apks.toml"];
/// The root manifest: it enters the state as [`native_manifest`], not as the file.
const MANIFEST: &str = "Cargo.toml";
/// The cargo profile native builds crates with (`cargo cinstall --release`, native.rs build_crates); no other profile reaches its outputs. Change both together.
const NATIVE_PROFILE: &str = "release";

/// The part of the root manifest a native build reads: the parsed TOML (comments and layout drop out) with every profile but [`NATIVE_PROFILE`] removed and,
/// in that one, the per-package overrides of gate-side packages (a [`GATE_SIDE`] directory holds the package of its name: `xtask`, `xtask-harness`).
/// So `[profile.dev.package.xtask]` or a comment edit keeps the slot current; members, `[workspace.*]`, `[patch]`, lints and the release profile still count.
fn native_manifest(text: &str) -> Result<String> {
    let mut doc: toml::Table = toml::from_str(text).context("parsing the root Cargo.toml")?;
    if let Some(toml::Value::Table(profiles)) = doc.get_mut("profile") {
        profiles.retain(|name, _| name == NATIVE_PROFILE);
        if let Some(toml::Value::Table(profile)) = profiles.get_mut(NATIVE_PROFILE)
            && let Some(toml::Value::Table(pkgs)) = profile.get_mut("package")
        {
            pkgs.retain(|name, _| !GATE_SIDE.contains(&name));
            // an emptied `package` table reads as no overrides, as if never written
            if pkgs.is_empty() {
                profile.remove("package");
            }
        }
    }
    toml::to_string(&doc).context("writing the root Cargo.toml's native part")
}

/// The checkout's source state: the hash of its top-level tree entries, [`GATE_SIDE`] left out, the root manifest as [`native_manifest`].
pub fn of_checkout(root: &Path) -> Result<String> {
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"], None)?);
    // one scratch index per call: concurrent calls in one process (parallel tests) would share git's `<index>.lock`
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch =
        std::env::temp_dir().join(format!("xtask-build-state-{}-{call}", std::process::id()));
    let index = git_dir.join("index");
    if index.exists() {
        std::fs::copy(&index, &scratch).context("copying the git index")?;
    }
    let result = (|| {
        git(root, &["add", "-A"], Some(&scratch))?;
        let tree = git(root, &["write-tree"], Some(&scratch))?;
        let listing = git(root, &["ls-tree", &tree], None)?;
        let mut kept = Vec::new();
        for line in listing.lines() {
            match line.split_once('\t') {
                Some((_, name)) if GATE_SIDE.contains(&name) => {}
                Some((mode, MANIFEST)) if mode.contains(" blob ") => {
                    let text = std::fs::read_to_string(root.join(MANIFEST))
                        .context("reading the root Cargo.toml")?;
                    let native = native_manifest(&text)?;
                    // only hashed, not stored: mktree --missing takes the id as is
                    let id = git_stdin(root, &["hash-object", "--stdin"], &native)?;
                    kept.push(format!("100644 blob {id}\t{MANIFEST}"));
                }
                _ => kept.push(line.to_owned()),
            }
        }
        git_stdin(
            root,
            &["mktree", "--missing"],
            &format!("{}\n", kept.join("\n")),
        )
    })();
    let _ = std::fs::remove_file(&scratch);
    result
}

/// A parsed record: the state, and the goals built from it.
struct Record {
    state: String,
    goals: BTreeSet<String>,
}

fn read(out: &Path) -> Option<Record> {
    let text = std::fs::read_to_string(out.join(RECORD)).ok()?;
    let mut lines = text.lines();
    Some(Record {
        state: lines.next()?.trim().to_owned(),
        goals: lines
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

/// `native` starts from `state`: the out dir no longer holds a known build. Returns the goals a previous run built from this same state, which stay current.
pub fn forget(out: &Path, state: &str) -> Result<BTreeSet<String>> {
    let kept = read(out)
        .filter(|r| r.state == state)
        .map(|r| r.goals)
        .unwrap_or_default();
    match std::fs::remove_file(out.join(RECORD)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).context("removing the build record")
        }
        _ => Ok(kept),
    }
}

/// `native` built `goals` from `state`.
pub fn record(out: &Path, state: &str, goals: &BTreeSet<String>) -> Result<()> {
    let mut text = format!("{state}\n");
    for g in goals {
        text.push_str(&format!("{g}\n"));
    }
    std::fs::write(out.join(RECORD), text).context("writing the build record")
}

/// Gate `gate`'s native out dir (`--out`, else the slot): refused unless built from the checkout at `root` as it is now, with every goal the gate declares (gate_goals.rs).
pub fn gate_out(root: &Path, explicit: Option<&Path>, host: &str, gate: &str) -> Result<PathBuf> {
    let out = crate::slot::out_dir(root, explicit, "--out", |workspace| {
        workspace.join("out/native").join(host)
    })?;
    ensure_current(root, &out).with_context(|| {
        format!(
            "gate `{gate}`: the slot is stale: run `cargo xtask native --host {host} --for {gate}`"
        )
    })?;
    let built = read(&out).map(|r| r.goals).unwrap_or_default();
    crate::gate_goals::ensure_built(gate, host, &built)?;
    Ok(out)
}

/// What a native build reads: the checkout's state, then each AOSP task copy bound over `src/aosp` (tree::overlays) as ` <project>=<its state>`, so a plain and a task build of one checkout differ.
pub fn of_build(root: &Path) -> Result<String> {
    with_overlays(root, &crate::tree::overlays_of(root)?)
}

fn with_overlays(root: &Path, overlays: &[crate::tree::Overlay]) -> Result<String> {
    let mut s = of_checkout(root)?;
    for o in overlays {
        s.push_str(&format!(" {}={}", o.project, of_checkout(&o.copy)?));
    }
    Ok(s)
}

/// A gate's guard: `out` was built from the checkout at `root` as it is now.
pub fn ensure_current(root: &Path, out: &Path) -> Result<()> {
    ensure_matches(&of_build(root)?, out)
}

pub(crate) fn ensure_matches(now: &str, out: &Path) -> Result<()> {
    let recorded = match read(out) {
        Some(r) => r.state,
        None => bail!(
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
        std::fs::write(d.join("test-apks.toml"), "[fossify-notes]\n").unwrap();
        git(&d, &["init", "-q"], None).unwrap();
        d
    }

    /// The guard fires with no record and after a source edit (untracked, uncommitted: the rebase case is the same tree change), stays quiet on the recorded state,
    /// and ignores the gate side (`xtask/`, `test-apks.toml`); the scratch index leaves the real one untouched.
    #[test]
    fn guard_fires_on_another_tree() {
        let d = repo("guard");
        let out = d.with_extension("out");
        std::fs::create_dir_all(&out).unwrap();
        // the fake repo has no sources.toml: the plain build's state, no overlays
        let ensure_current = |d: &Path, out: &Path| ensure_matches(&with_overlays(d, &[])?, out);
        let e = ensure_current(&d, &out).expect_err("no record");
        assert!(format!("{e}").contains("run `cargo xtask native`"), "{e}");
        let state = of_checkout(&d).unwrap();
        assert!(forget(&out, &state).unwrap().is_empty());
        let goals: BTreeSet<String> = ["a".to_owned()].into();
        record(&out, &state, &goals).unwrap();
        ensure_current(&d, &out).expect("built from this state");
        // a run from the same state keeps the earlier goals; from another, starts over
        assert_eq!(forget(&out, &state).unwrap(), goals);
        record(&out, &state, &goals).unwrap();
        assert!(forget(&out, "other").unwrap().is_empty());
        record(&out, &state, &goals).unwrap();
        // the same checkout with a task copy bound: another build
        let copy = repo("guard-copy");
        let bound = [crate::tree::Overlay {
            project: "p".into(),
            copy: copy.clone(),
            target: d.join("p"),
        }];
        let e = ensure_matches(&with_overlays(&d, &bound).unwrap(), &out).expect_err("task build");
        assert!(format!("{e}").contains(" p="), "{e}");
        let _ = std::fs::remove_dir_all(&copy);
        std::fs::write(d.join("xtask/g.rs"), "gate edit").unwrap();
        ensure_current(&d, &out).expect("gate side is not native input");
        std::fs::write(d.join("test-apks.toml"), "[fossify-notes]\nsize = 1\n").unwrap();
        ensure_current(&d, &out).expect("test APK table is not native input");
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
        forget(&out, "").unwrap();
        assert!(
            ensure_current(&d, &out).is_err(),
            "a started native run forgets"
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The root manifest counts only where a native build reads it: the release profile (minus xtask's override), members, workspace deps.
    /// Each case writes a variant of BASE and compares its state with BASE's; the expected verdict is the rule's, per line.
    #[test]
    fn manifest_counts_only_native_parts() {
        const BASE: &str = "[workspace]\nmembers = [\"xtask\", \"crates/*\"]\n\n[workspace.dependencies]\nlibc = \"0.2\"\n\n\
            [profile.release]\nopt-level = 3\n\n[profile.dev]\ndebug = \"line-tables-only\"\n\n[profile.dev.package.xtask]\nopt-level = 2\n";
        let d = repo("manifest");
        let state_of = |text: &str| {
            std::fs::write(d.join(MANIFEST), text).unwrap();
            of_checkout(&d).unwrap()
        };
        let base = state_of(BASE);
        let same = [
            (
                "xtask's dev opt-level",
                BASE.replace("opt-level = 2", "opt-level = 0"),
            ),
            ("dev profile", BASE.replace("\"line-tables-only\"", "true")),
            ("a comment", format!("# note\n{BASE}")),
            (
                "xtask's release override",
                format!("{BASE}\n[profile.release.package.xtask]\nopt-level = 1\n"),
            ),
        ];
        for (what, text) in same {
            assert_eq!(state_of(&text), base, "{what} is not native input");
        }
        let changed = [
            (
                "release opt-level",
                BASE.replace("opt-level = 3", "opt-level = 2"),
            ),
            ("a workspace dep", BASE.replace("\"0.2\"", "\"0.3\"")),
            ("members", BASE.replace("\"crates/*\"", "\"crates/a\"")),
            (
                "a product crate's release override",
                format!("{BASE}\n[profile.release.package.atl-x]\nopt-level = 1\n"),
            ),
        ];
        for (what, text) in changed {
            assert_ne!(state_of(&text), base, "{what} is native input");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
