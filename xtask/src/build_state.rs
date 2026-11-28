// SPDX-License-Identifier: MIT
//! The source state a native out dir was built from, so a runtime gate refuses artifacts from another tree (a rebase without `xtask native` once cost a whole round).
//!
//! The state is the checkout's tracked files as they are on disk, hashed through a scratch index: `git add -u` into a copy of the index, `git write-tree`.
//! Tracked only: `add -A` stored every untracked, unignored file (stray build outputs included) as a blob in `.git`; a new source file counts once it is `git add`ed. The top-level entries listed in [`GATE_SIDE`] are left out of the hash; the gate code native judges with ([`GATE_CODE`]) is a word of its own, so a stricter gate makes the record stale without any artifact being.
//! `native` deletes the record when it starts and writes it after its gates (the state taken before the build, so an edit during the build reads as a mismatch); a gate compares it with the checkout's state now.
//! A record of goals means: this graph was built from this state and every native gate passed. A red run leaves [`RED`] and the failed gates' names instead, which no reader takes as built: `check` does not run native's gates and land skips a current build dir, so the record is their only sight of a red one.
//! Each graph (the host's, each foreign guest's `nb-graph-<arch>`) holds its own verdict; a dir is current when its record and those of the foreign graphs in it are ([`ensure_current`]).
//! The record also lists the goals built from that state (line 1 state, then one goal per line, nb goals as `nb:<arch>:<module>`): a run from the same state adds its goals to the
//! previous record's, a run from another state starts over, since the older goals' outputs are stale. A gate refuses a build dir lacking its goals (gate_goals.rs).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::verdict::Failure;

/// The record, in the out dir.
const RECORD: &str = "built-from";
/// First word of a red run's record: `red <state>`, then one failed gate per line. A reader that does not know it sees another state, so it refuses too.
const RED: &str = "red ";
/// Top-level entries native's artifacts do not come from: `xtask/` and `xtask-harness/` are the gate side (gate code and the fixtures each gate builds itself), so editing a runtime gate or a fixture needs no native run. Native's own gates do count, through [`GATE_CODE`].
/// `test-apks.toml` feeds only `run-apk`, `fetch --test-apks` and the `tables` check: a row or pin change needs no native run.
/// Trade-off: native's own code (xtask/src/native.rs, the converter driver) is in it too; a change there reaches the build dir only through a native run anyway.
const GATE_SIDE: &[&str] = &["xtask", "xtask-harness", "test-apks.toml"];
/// The code native's gates judge with: native_gates.rs (which calls them) and the modules the verdict comes from. A run's record names its hash, so a build gated by other gate code reads as stale: one native run, no compile.
/// Not all of `xtask/`: every edit there would cost a native run. Modules that only shape artifacts or parse a table already in the state are the test's `GATE_SHARED`; the test holds this list to what the files reference.
/// Trade-off: native.rs (the order of work) is not here: a change that drops its call to `native_gates::run` is not seen.
const GATE_CODE: &[&str] = &[
    "xtask/src/aosp.rs",
    "xtask/src/isa_gate.rs",
    "xtask/src/isa_tiers.rs",
    "xtask/src/licence_gate.rs",
    "xtask/src/licenses.rs",
    "xtask/src/native_gates.rs",
    "xtask/src/ndk_surface.rs",
    "xtask/src/ninja_deps.rs",
    "xtask/src/none_gate.rs",
    "xtask/src/notice.rs",
    "xtask/src/provided_gate.rs",
    "xtask/src/providers.rs",
    "xtask/src/runpath_gate.rs",
    "xtask/src/selfbind_gate.rs",
    "xtask/src/spdx.rs",
    "xtask/src/triage.rs",
];
/// Starts the state's last word: the hash of [`GATE_CODE`].
const GATES: &str = " gates=";
/// The root manifest: it enters the state as [`native_manifest`], not as the file.
const MANIFEST: &str = "Cargo.toml";
/// The cargo profile native builds crates with (`cargo cinstall --profile`, native.rs build_crates); no other profile reaches its outputs.
pub const NATIVE_PROFILE: &str = "release";

/// The part of the root manifest a native build reads: the parsed TOML (comments and layout drop out) with every profile but [`NATIVE_PROFILE`] removed and,
/// in that one, the per-package overrides of gate-side packages (a [`GATE_SIDE`] directory holds the package of its name: `xtask`, `xtask-harness`).
/// So `[profile.dev.package.xtask]` or a comment edit keeps the build dir current; members, `[workspace.*]`, `[patch]`, lints and the release profile still count.
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

/// The tree of the git checkout at `root` as its tracked files are on disk (uncommitted edits included, untracked files not): `git add -u` into a scratch copy of the index, `git write-tree`.
pub fn tracked_tree(root: &Path) -> Result<String> {
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"], None)?);
    // one scratch index per call: concurrent calls in one process (parallel tests, land's per-upstream scan) would share git's `<index>.lock`
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let scratch =
        std::env::temp_dir().join(format!("xtask-build-state-{}-{call}", std::process::id()));
    let index = git_dir.join("index");
    if index.exists() {
        std::fs::copy(&index, &scratch).context("copying the git index")?;
    }
    let result = git(root, &["add", "-u"], Some(&scratch))
        .and_then(|_| git(root, &["write-tree"], Some(&scratch)));
    let _ = std::fs::remove_file(&scratch);
    result
}

/// The checkout's source state: the hash of its top-level tree entries ([`tracked_tree`]), [`GATE_SIDE`] left out, the root manifest as [`native_manifest`].
pub fn of_checkout(root: &Path) -> Result<String> {
    of_tree(root, &tracked_tree(root)?)
}

fn of_tree(root: &Path, tree: &str) -> Result<String> {
    let listing = git(root, &["ls-tree", tree], None)?;
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
}

/// The hash of [`GATE_CODE`]'s entries in `tree`, the tracked tree of `root`.
fn gate_code(root: &Path, tree: &str) -> Result<String> {
    let mut args = vec!["ls-tree", tree, "--"];
    args.extend(GATE_CODE);
    let listing = git(root, &args, None)?;
    git_stdin(root, &["hash-object", "--stdin"], &listing)
}

/// A parsed record: the state, and what the run from it came to.
struct Record {
    state: String,
    run: Run,
}

enum Run {
    /// Every native gate passed: the goals built.
    Passed(BTreeSet<String>),
    /// The gates (ninja included) that failed.
    Red(Vec<String>),
}

impl Record {
    /// The goals known built and gated: none of a red run.
    fn goals(self) -> BTreeSet<String> {
        match self.run {
            Run::Passed(goals) => goals,
            Run::Red(_) => BTreeSet::new(),
        }
    }
}

fn read(out: &Path) -> Option<Record> {
    let text = std::fs::read_to_string(out.join(RECORD)).ok()?;
    let mut lines = text.lines();
    let first = lines.next()?.trim();
    let rest = lines
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned);
    let (state, run) = match first.strip_prefix(RED) {
        Some(state) => (state, Run::Red(rest.collect())),
        None => (first, Run::Passed(rest.collect())),
    };
    Some(Record {
        state: state.to_owned(),
        run,
    })
}

/// `native` starts from `state`: the out dir no longer holds a known build. Returns the goals a previous run built from this same state, which stay current.
pub fn forget(out: &Path, state: &str) -> Result<BTreeSet<String>> {
    let kept = read(out)
        .filter(|r| r.state == state)
        .map(Record::goals)
        .unwrap_or_default();
    match std::fs::remove_file(out.join(RECORD)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).context("removing the build record")
        }
        _ => Ok(kept),
    }
}

/// `native` ran from `state`, every gate done: none failed -> `goals` are built, else the run is red and records the failed gates, no goal.
pub fn record(out: &Path, state: &str, goals: &BTreeSet<String>, failed: &[Failure]) -> Result<()> {
    let (mut text, lines): (String, Vec<&str>) = if failed.is_empty() {
        (
            format!("{state}\n"),
            goals.iter().map(String::as_str).collect(),
        )
    } else {
        let gates = failed.iter().map(|f| f.step.as_deref().unwrap_or("native"));
        (format!("{RED}{state}\n"), gates.collect())
    };
    for l in lines {
        text.push_str(&format!("{l}\n"));
    }
    std::fs::write(out.join(RECORD), text).context("writing the build record")
}

/// Gate `gate`'s native out dir (`--out`, else the build dir): refused unless built from the checkout at `root` as it is now, with every goal the gate declares (gate_goals.rs; `all-gates` = every gate's, land's build stage).
pub fn gate_out(root: &Path, explicit: Option<&Path>, host: &str, gate: &str) -> Result<PathBuf> {
    let out = crate::native::host_out(root, explicit, host)?;
    ensure_current(root, &out).with_context(|| {
        format!(
            "gate `{gate}`: the build dir is not current: run `cargo xtask native --host {host} --for {gate}`"
        )
    })?;
    let built = read(&out).map(Record::goals).unwrap_or_default();
    crate::gate_goals::ensure_built(root, gate, host, &built)?;
    Ok(out)
}

/// None when the build dir (`explicit`, else the checkout's) was built from `root` as it is now with every gate's goals for the build machine's host, else why not (gate_out's refusal). Land's `--build-dir` reports it; `check` refuses a full run on it before step 1.
pub fn build_dir_staleness(root: &Path, explicit: Option<&Path>) -> Result<Option<String>> {
    let arches = crate::arches::load(root)?;
    let host = crate::arches::build_machine(root, &arches)?.0;
    Ok(gate_out(root, explicit, host, crate::gate_goals::ALL)
        .err()
        .map(|e| format!("{e:#}")))
}

/// The record keys gate `gate` needs in guest `isa`'s foreign graph: its declared nb goals as `nb:<isa>:<module>`. The one list both [`foreign_out`] checks and `native --native-bridge <isa> --for <gate>` builds.
pub fn foreign_goals(root: &Path, gate: &str, isa: &str) -> Result<BTreeSet<String>> {
    Ok(crate::gate_goals::of(root, gate)?
        .nb
        .iter()
        .map(|m| crate::gate_goals::nb_key(isa, m))
        .collect())
}

/// Run-time gate `gate`'s foreign native-bridge graph for guest `isa` (native.rs `graph_dir` under the host's out dir `out`): refused unless built from the checkout now with every goal of [`foreign_goals`], else naming the `native --native-bridge <isa> --for <gate>` command that builds them.
pub fn foreign_out(root: &Path, out: &Path, host: &str, isa: &str, gate: &str) -> Result<PathBuf> {
    // graph dirs are keyed by the host's arch (a variant host's own guest is its arch's)
    let arches = crate::arches::load(root)?;
    let own = crate::arches::find_host(&arches, host).map_or(host, |h| h.arch_name);
    let dir = crate::native::graph_dir(out, own, isa);
    let fix = format!("cargo xtask native --host {host} --native-bridge {isa} --for {gate}");
    ensure_current(root, &dir).with_context(|| {
        format!("gate `{gate}`: the {isa} native-bridge graph is not current: run `{fix}`")
    })?;
    let built = read(&dir).map(Record::goals).unwrap_or_default();
    let missing: Vec<String> = foreign_goals(root, gate, isa)?
        .into_iter()
        .filter(|g| !built.contains(g))
        .collect();
    ensure!(
        missing.is_empty(),
        "gate `{gate}`: the {isa} native-bridge graph lacks {} goal(s): {}: run `{fix}`",
        missing.len(),
        missing.join(" ")
    );
    Ok(dir)
}

/// What a native build reads: the checkout's state, then each AOSP task copy bound over `src/aosp` (tree::overlays) as ` <project>=<its state>`, so a plain and a task build of one checkout differ; last the gate code it is judged by ([`GATE_CODE`]).
pub fn of_build(root: &Path) -> Result<String> {
    with_overlays(root, &crate::tree::overlays_of(root)?)
}

fn with_overlays(root: &Path, overlays: &[crate::tree::Overlay]) -> Result<String> {
    // one tree for both parts: a tracked_tree is a full `git add -u`
    let tree = tracked_tree(root)?;
    let mut s = of_tree(root, &tree)?;
    for o in overlays {
        s.push_str(&format!(" {}={}", o.project, of_checkout(&o.copy)?));
    }
    s.push_str(GATES);
    s.push_str(&gate_code(root, &tree)?);
    Ok(s)
}

/// A gate's guard: `out`, with the foreign graphs in it, was built from the checkout at `root` as it is now and every native gate passed.
pub fn ensure_current(root: &Path, out: &Path) -> Result<()> {
    let now = of_build(root)?;
    ensure_matches(&now, out)?;
    ensure_graphs(&now, &crate::native::guest_graphs(root, out)?)
}

fn ensure_graphs(now: &str, graphs: &[(String, PathBuf)]) -> Result<()> {
    for (arch, dir) in graphs {
        ensure_matches(now, dir).with_context(|| format!("the {arch} native-bridge graph"))?;
    }
    Ok(())
}

pub(crate) fn ensure_matches(now: &str, out: &Path) -> Result<()> {
    let Some(Record { state, run }) = read(out) else {
        bail!(
            "{} holds no record of the tree it was built from (a native run never finished there): run `cargo xtask native`",
            out.display()
        )
    };
    let source = |s: &str| s.rsplit_once(GATES).map_or(s, |(s, _)| s).to_owned();
    ensure!(
        state == now || source(&state) != source(now),
        "{} was gated by other gate code ({state}, the checkout is {now}): run `cargo xtask native`: nothing recompiles, its gates rerun",
        out.display()
    );
    ensure!(
        state == now,
        "{} was built from another source state ({state}, the checkout is {now}): run `cargo xtask native`",
        out.display()
    );
    if let Run::Red(gates) = run {
        bail!(
            "{}: the last native run from this source state FAILED: {}: fix it, then run `cargo xtask native`",
            out.display(),
            gates.join(", ")
        );
    }
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

    /// Two of [`GATE_CODE`], in the fixture repo.
    const LISTED: [&str; 2] = ["xtask/src/native_gates.rs", "xtask/src/licenses.rs"];

    fn repo(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("xtask-build-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("crates")).unwrap();
        std::fs::create_dir_all(d.join("xtask/src")).unwrap();
        std::fs::write(d.join("crates/a.rs"), "a").unwrap();
        for f in ["xtask/g.rs", "xtask/src/out.rs", LISTED[0], LISTED[1]] {
            std::fs::write(d.join(f), "g").unwrap();
        }
        std::fs::write(d.join("test-apks.toml"), "[fossify-notes]\n").unwrap();
        std::fs::write(d.join(MANIFEST), "[workspace]\n").unwrap();
        git(&d, &["init", "-q"], None).unwrap();
        git(&d, &["add", "-A"], None).unwrap();
        git(
            &d,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "base",
            ],
            None,
        )
        .unwrap();
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
        let state = with_overlays(&d, &[]).unwrap();
        assert!(forget(&out, &state).unwrap().is_empty());
        let goals: BTreeSet<String> = ["a".to_owned()].into();
        record(&out, &state, &goals, &[]).unwrap();
        ensure_current(&d, &out).expect("built from this state");
        // a run from the same state keeps the earlier goals; from another, starts over
        assert_eq!(forget(&out, &state).unwrap(), goals);
        record(&out, &state, &goals, &[]).unwrap();
        assert!(forget(&out, "other").unwrap().is_empty());
        record(&out, &state, &goals, &[]).unwrap();
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
        std::fs::write(d.join("xtask/src/out.rs"), "unlisted edit").unwrap();
        std::fs::write(d.join("xtask/src/x.rs"), "untracked").unwrap();
        ensure_current(&d, &out).expect("gate side is not native input");
        std::fs::write(d.join("test-apks.toml"), "[fossify-notes]\nsize = 1\n").unwrap();
        ensure_current(&d, &out).expect("test APK table is not native input");
        // an untracked file (a stray build output) neither counts nor lands in .git as a blob
        std::fs::write(d.join("crates/stray.so"), "build output").unwrap();
        ensure_current(&d, &out).expect("untracked files are not source state");
        let stray = git_stdin(&d, &["hash-object", "--stdin"], "build output").unwrap();
        assert!(
            git(&d, &["cat-file", "-e", &stray], None).is_err(),
            "untracked file stored in .git"
        );
        std::fs::write(d.join("crates/a.rs"), "edited").unwrap();
        let e = ensure_current(&d, &out).expect_err("source edited");
        assert!(
            format!("{e}").contains("built from another source state"),
            "{e}"
        );
        let staged = git(&d, &["diff", "--cached", "--name-only"], None).unwrap();
        assert!(
            staged.is_empty(),
            "the real index stays untouched: {staged}"
        );
        forget(&out, "").unwrap();
        assert!(
            ensure_current(&d, &out).is_err(),
            "a started native run forgets"
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A listed file's edit refuses with the gate-code text and no goal survives it; a source edit, alone or with one, keeps the source-state text; a record without the gates word is refused.
    #[test]
    fn gate_code_is_part_of_the_state() {
        let d = repo("gates");
        let out = d.with_extension("out");
        std::fs::create_dir_all(&out).unwrap();
        let now = || with_overlays(&d, &[]).unwrap();
        let refusal = || format!("{}", ensure_matches(&now(), &out).expect_err("refused"));
        let goals: BTreeSet<String> = ["a".to_owned()].into();
        let base = now();
        for file in LISTED {
            record(&out, &base, &goals, &[]).unwrap();
            ensure_matches(&now(), &out).expect("gated by this code");
            std::fs::write(d.join(file), "stricter").unwrap();
            let e = refusal();
            assert!(e.contains("was gated by other gate code"), "{file}: {e}");
            assert!(e.contains("run `cargo xtask native`"), "{e}");
            assert!(!e.contains("another source state"), "{e}");
            assert!(forget(&out, &now()).unwrap().is_empty(), "{file}");
            std::fs::write(d.join(file), "g").unwrap();
        }
        // a record from before the word
        record(&out, &of_checkout(&d).unwrap(), &goals, &[]).unwrap();
        refusal();
        record(&out, &base, &goals, &[]).unwrap();
        std::fs::write(d.join("crates/a.rs"), "edited").unwrap();
        for gate in ["g", "stricter"] {
            std::fs::write(d.join(LISTED[0]), gate).unwrap();
            let e = refusal();
            assert!(e.contains("built from another source state"), "{e}");
            assert!(!e.contains("gate code"), "{e}");
        }
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Modules [`GATE_CODE`] references that are not in it, each with why a change there changes no verdict on unchanged artifacts, or reaches it through the state.
    const GATE_SHARED: &[(&str, &str)] = &[
        ("arches", "parses arches.toml, in the state"),
        ("cross", "cross toolchain flags: shape artifacts"),
        (
            "deps",
            "parses deps.toml, in the state, and finds host tools",
        ),
        ("fetch", "makes checkouts"),
        ("git", "runs git"),
        ("native", "the order of work: artifact and report paths"),
        ("out", "build dir paths"),
        ("packages", "the cargo target dir's name"),
        ("par", "runs jobs in parallel"),
        ("sources", "parses sources.toml, in the state"),
        ("tree", "the tree's path"),
        ("upstream", "provider build recipes: shape artifacts"),
        ("verdict", "the verdict line's text"),
    ];

    /// A source file up to its test module.
    fn product_part(text: &str) -> &str {
        text.split_once("\n#[cfg(test)]\nmod tests")
            .map_or(text, |(code, _)| code)
    }

    /// The top-level names after each `crate::` of `text`, use lists (`crate::{a, b::{c}}`) included.
    fn crate_refs(text: &str) -> BTreeSet<String> {
        let ident = |s: &str| -> String {
            s.trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        };
        let mut refs = BTreeSet::new();
        for (at, word) in text.match_indices("crate::") {
            let rest = &text[at + word.len()..];
            let Some(list) = rest.strip_prefix('{') else {
                refs.insert(ident(rest));
                continue;
            };
            refs.insert(ident(list));
            let mut depth = 0;
            for (i, c) in list.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' if depth == 0 => break,
                    '}' => depth -= 1,
                    ',' if depth == 0 => {
                        refs.insert(ident(&list[i + 1..]));
                    }
                    _ => {}
                }
            }
        }
        refs.remove("");
        refs
    }

    #[test]
    fn crate_refs_reads_use_lists() {
        let refs = crate_refs("use crate::{\n    a, b::{self, c},\n    d,\n};\ncrate::e::f()");
        assert_eq!(refs, ["a", "b", "d", "e"].map(str::to_owned).into());
    }

    /// The list holds every module its files judge with; each path is tracked (`ls-tree` prints nothing for a missing one, so a renamed file would leave the hash silently), no GATE_SHARED entry is unused.
    #[test]
    fn gate_code_lists_what_its_files_use() {
        let root = crate::test_root();
        let listed = git(&root, &[&["ls-files", "--"], GATE_CODE].concat(), None).unwrap();
        assert_eq!(listed.lines().count(), GATE_CODE.len(), "tracked: {listed}");
        let mut unknown = Vec::new();
        let mut used = BTreeSet::new();
        for file in GATE_CODE {
            let text = std::fs::read_to_string(root.join(file)).unwrap();
            for module in crate_refs(product_part(&text)) {
                let gate = GATE_CODE.contains(&format!("xtask/src/{module}.rs").as_str());
                if !gate && !GATE_SHARED.iter().any(|(m, _)| *m == module) {
                    unknown.push(format!("{file}: {module}"));
                }
                used.insert(module);
            }
        }
        assert!(
            unknown.is_empty(),
            "in neither GATE_CODE nor GATE_SHARED: {unknown:?}"
        );
        for (m, why) in GATE_SHARED {
            assert!(used.contains(*m), "GATE_SHARED `{m}` ({why}): unused");
        }
    }

    /// native.rs names no failed gate itself: one added there would judge outside [`GATE_CODE`].
    #[test]
    fn native_fails_gates_only_in_the_hashed_file() {
        let text = std::fs::read_to_string(crate::test_root().join("xtask/src/native.rs")).unwrap();
        let code = product_part(&text);
        let steps: Vec<&str> = code
            .split("Failure::step(")
            .skip(1)
            .map(str::trim_start)
            .collect();
        let known = ["\"ninja\"", "&format!(\"native-bridge {arch} graph\")"];
        for step in &steps {
            assert!(
                known.iter().any(|k| step.starts_with(k)),
                "a gate in native.rs: {}",
                step.lines().next().unwrap_or_default()
            );
        }
        assert_eq!(steps.len(), known.len());
        assert!(!code.contains("Failure::gate("));
        assert!(!code.contains("Failure {"));
    }

    /// A red run is no built state: the same source state is refused by the failed gates' names (not as another state) and keeps no goal; a passing run after it is accepted; a red run of an older state reads as stale.
    #[test]
    fn a_red_run_is_not_built() {
        let out = std::env::temp_dir().join(format!("xtask-build-red-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let goals: BTreeSet<String> = ["a".to_owned()].into();
        let failed = [
            Failure::step("licence gate", "x.rs: not declared"),
            Failure::step("fronts gate", "y"),
        ];
        record(&out, "s", &goals, &[]).unwrap();
        ensure_matches("s", &out).expect("every gate passed");
        assert_eq!(read(&out).unwrap().goals(), goals);
        // the pass does not survive a red run from the same state
        record(&out, "s", &goals, &failed).unwrap();
        let e = format!("{}", ensure_matches("s", &out).expect_err("red"));
        assert!(e.contains("FAILED: licence gate, fronts gate:"), "{e}");
        assert!(!e.contains("another source state"), "{e}");
        assert!(forget(&out, "s").unwrap().is_empty(), "a red run's goal");
        record(&out, "s", &goals, &failed).unwrap();
        let e = format!("{}", ensure_matches("t", &out).expect_err("other state"));
        assert!(e.contains("another source state (s,"), "{e}");
        record(&out, "s", &goals, &[]).unwrap();
        ensure_matches("s", &out).expect("passed after the red run");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A host dir is current only with its foreign graphs: an unfinished, red or stale one refuses it, naming the arch.
    #[test]
    fn a_red_foreign_graph_refuses_its_host() {
        let dir = std::env::temp_dir().join(format!("xtask-build-fgn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let graphs = [("arm64".to_owned(), dir.clone())];
        let none = BTreeSet::new();
        let refusal = || format!("{:#}", ensure_graphs("s", &graphs).expect_err("refused"));
        let e = refusal();
        assert!(e.contains("arm64 native-bridge graph: "), "{e}");
        assert!(e.contains("no record"), "{e}");
        record(&dir, "s", &none, &[Failure::step("guest abi gate", "x")]).unwrap();
        let e = refusal();
        assert!(e.contains("FAILED: guest abi gate:"), "{e}");
        record(&dir, "r", &none, &[]).unwrap();
        let e = refusal();
        assert!(e.contains("another source state"), "{e}");
        record(&dir, "s", &none, &[]).unwrap();
        ensure_graphs("s", &graphs).expect("every graph passed");
        let _ = std::fs::remove_dir_all(&dir);
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
