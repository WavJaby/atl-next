// SPDX-License-Identifier: MIT
//! `xtask patch`: our commits on upstream checkouts, kept as `patches/<upstream>/*.patch` and
//! re-applied on branch `atl-patches`; plus the drift check that keeps the two identical.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};

use crate::git::{Ident, git, git_isolated};
use crate::licenses::{self, Licenses};
use crate::sources::{self, Checkout};
use crate::{fetch, notice, workspace};

const BRANCH: &str = "atl-patches";
/// Commit `atl-patches` is based on. A rebase after a pin bump needs the old base, and shallow
/// checkouts hold no history linking two pins.
const BASE_REF: &str = "refs/atl/base";
/// Trailers every patch commit carries: upstream status (Yocto values: Pending, Submitted [where],
/// Backport, Denied, Inappropriate [why]) and why the change cannot live in the launcher/loader/env.
const TRAILERS: [&str; 2] = ["Upstream-Status", "Patch-Reason"];

#[derive(clap::Args)]
pub struct Args {
    /// Put every upstream with patches on branch atl-patches at its pin, with patches/<upstream>/
    /// applied (git am). Refuses to drop commits that are not exported.
    #[arg(long, conflicts_with_all = ["export", "rebase"])]
    apply: bool,
    /// Write <pin>..atl-patches of these upstreams (e.g. qemu, aosp/art) to patches/<upstream>/.
    #[arg(long, value_name = "UPSTREAM", num_args = 1.., conflicts_with = "rebase")]
    export: Vec<String>,
    /// Move atl-patches of these upstreams from their old base onto the current pin (after a bump).
    #[arg(long, value_name = "UPSTREAM", num_args = 1..)]
    rebase: Vec<String>,
}

/// One upstream as this module sees it.
struct Up<'a> {
    c: &'a Checkout,
    checkout: PathBuf,
    patches: PathBuf,
}

impl Up<'_> {
    fn name(&self) -> String {
        self.c.dir.display().to_string()
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let sources = sources::load(root)?;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let all = sources.checkouts();
    let src = workspace::root_dir(root)?.join("src");
    let ups: Vec<Up> = all.iter().map(|c| up(c, &src, root)).collect();
    let pick = |names: &[String]| -> Result<Vec<&Up>> {
        names
            .iter()
            .map(|n| {
                ups.iter()
                    .find(|u| u.name() == *n)
                    .with_context(|| format!("no upstream {n}"))
            })
            .collect()
    };
    let mut failed = 0;
    let mut report = |name: String, r: Result<String>| match r {
        Ok(s) => println!("patch: {name:<40} {s}"),
        Err(e) => {
            failed += 1;
            println!("patch: {name:<40} FAILED: {e:#}");
        }
    };
    if args.apply {
        let ident = Ident::of_repo(root)?;
        for u in &ups {
            report(u.name(), apply(u, &ident, &licenses));
        }
    } else if !args.export.is_empty() {
        for u in pick(&args.export)? {
            report(u.name(), export(u, &licenses));
        }
    } else if !args.rebase.is_empty() {
        let ident = Ident::of_repo(root)?;
        for u in pick(&args.rebase)? {
            report(u.name(), rebase(u, &ident));
        }
    } else {
        for u in &ups {
            let shown = match drift(u, &licenses) {
                Ok(None) => Ok("no checkout".to_owned()),
                Ok(Some(p)) if p.is_empty() => Ok("in step".to_owned()),
                Ok(Some(p)) => Err(anyhow::anyhow!(p.join("; "))),
                Err(e) => Err(e),
            };
            report(u.name(), shown);
        }
    }
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn up<'a>(c: &'a Checkout, src: &Path, root: &Path) -> Up<'a> {
    Up {
        c,
        checkout: src.join(&c.dir),
        patches: root.join("patches").join(&c.dir),
    }
}

/// The drift check behind `xtask check`: every present checkout matches its pin and patches/.
pub fn check(root: &Path) -> Result<()> {
    let sources = sources::load(root)?;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let src = workspace::root_dir(root)?.join("src");
    let (mut present, mut problems) = (0, Vec::new());
    for c in &sources.checkouts() {
        let u = up(c, &src, root);
        match drift(&u, &licenses) {
            Ok(None) => {}
            Ok(Some(p)) => {
                present += 1;
                problems.extend(p.into_iter().map(|p| format!("{}: {p}", u.name())));
            }
            Err(e) => problems.push(format!("{}: {e:#}", u.name())),
        }
    }
    for p in &problems {
        eprintln!("drift: {p}");
    }
    ensure!(problems.is_empty(), "{} drift problems", problems.len());
    eprintln!(
        "drift: {present} checkouts in step, {} not fetched",
        sources.checkouts().len() - present
    );
    Ok(())
}

/// None when the checkout is absent; otherwise what keeps it from matching its pin and patches/.
fn drift(u: &Up, licenses: &Licenses) -> Result<Option<Vec<String>>> {
    let dir = &u.checkout;
    if !dir.exists() {
        return Ok(None);
    }
    let mut problems = Vec::new();
    if is_dirty(dir)? {
        problems.push("uncommitted changes".to_owned());
    }
    if rebase_in_progress(dir)? {
        problems.push("rebase in progress".to_owned());
        return Ok(Some(problems));
    }
    let stored = stored(&u.patches)?;
    if on_branch(dir)? {
        match base(dir)? {
            None => problems.push(format!("{BRANCH} has no {BASE_REF}: xtask patch --apply")),
            Some(b) if b != u.c.commit => {
                problems.push(format!("pin moved: xtask patch --rebase {}", u.name()));
            }
            Some(_) => match exported(u, licenses) {
                Ok(e) if e == stored => {}
                Ok(_) => problems.push(format!(
                    "{BRANCH} differs from patches/{0}: xtask patch --export {0}",
                    u.name()
                )),
                Err(e) => problems.push(format!("{e:#}")),
            },
        }
    } else if !stored.is_empty() {
        problems.push(format!(
            "{} patches not applied: xtask patch --apply",
            stored.len()
        ));
    } else if git(Some(dir), &["rev-parse", "HEAD"])? != u.c.commit {
        problems.push("not at pin: xtask fetch --execute".to_owned());
    }
    Ok(Some(problems))
}

fn apply(u: &Up, ident: &Ident, licenses: &Licenses) -> Result<String> {
    let stored = stored(&u.patches)?;
    if stored.is_empty() {
        return Ok("no patches".to_owned());
    }
    let dir = &u.checkout;
    ensure!(dir.exists(), "no checkout: xtask fetch --execute");
    ensure!(
        !is_dirty(dir)? && !rebase_in_progress(dir)?,
        "uncommitted changes or rebase in progress"
    );
    // never drop commits that patches/ does not hold
    if git(
        Some(dir),
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/heads/{BRANCH}"),
        ],
    )
    .is_ok()
    {
        let stale = format!(
            "or `git -C {} branch -D {BRANCH}` if they are stale",
            dir.display()
        );
        match base(dir)? {
            Some(b) if b == u.c.commit => {
                let current = exported(u, licenses)?;
                if current == stored {
                    return Ok("already applied".to_owned());
                }
                ensure!(
                    current.is_empty(),
                    "{BRANCH} differs from patches/: --export it, {stale}"
                );
            }
            Some(b) => {
                let n = git(
                    Some(dir),
                    &["rev-list", "--count", &format!("{b}..{BRANCH}")],
                )?;
                ensure!(
                    n == "0",
                    "{BRANCH} is based on an old pin: --rebase it, {stale}"
                );
            }
            None => {}
        }
    }
    if git(
        Some(dir),
        &["cat-file", "-e", &format!("{}^{{commit}}", u.c.commit)],
    )
    .is_err()
    {
        fetch::fetch_pin(u.c, dir)?;
    }
    git(Some(dir), &["checkout", "-q", "-B", BRANCH, &u.c.commit])?;
    git(Some(dir), &["update-ref", BASE_REF, &u.c.commit])?;
    let files: Vec<String> = stored
        .iter()
        .map(|(name, _)| u.patches.join(name).display().to_string())
        .collect();
    let mut args = vec!["am", "-q", "--committer-date-is-author-date"];
    args.extend(files.iter().map(String::as_str));
    if let Err(e) = git_isolated(dir, &args, Some(ident)) {
        let _ = git(Some(dir), &["am", "--abort"]);
        return Err(e.context("git am failed; branch reset to the pin"));
    }
    Ok(format!("applied {} patches", stored.len()))
}

fn export(u: &Up, licenses: &Licenses) -> Result<String> {
    let dir = &u.checkout;
    ensure!(
        dir.exists() && on_branch(dir)?,
        "not on {BRANCH}: nothing to export"
    );
    ensure!(
        base(dir)?.as_deref() == Some(u.c.commit.as_str()),
        "{BRANCH} is not based on the pin: xtask patch --rebase {}",
        u.name()
    );
    let patches = exported(u, licenses)?;
    std::fs::create_dir_all(&u.patches)?;
    for (name, _) in stored(&u.patches)? {
        std::fs::remove_file(u.patches.join(name))?;
    }
    for (name, text) in &patches {
        std::fs::write(u.patches.join(name), text)?;
    }
    let licence_note = if u.patches.join("LICENSE").exists() {
        ""
    } else {
        " (add patches/<upstream>/LICENSE: the upstream's licence text)"
    };
    Ok(format!("exported {} patches{licence_note}", patches.len()))
}

fn rebase(u: &Up, ident: &Ident) -> Result<String> {
    let dir = &u.checkout;
    let pin = u.c.commit.as_str();
    ensure!(dir.exists() && on_branch(dir)?, "not on {BRANCH}");
    ensure!(
        !rebase_in_progress(dir)?,
        "finish the rebase (git rebase --continue / --abort) first"
    );
    ensure!(!is_dirty(dir)?, "uncommitted changes");
    let old = base(dir)?.with_context(|| format!("no {BASE_REF}: xtask patch --apply"))?;
    if old == pin {
        return Ok("already on the pin".to_owned());
    }
    // `xtask fetch` leaves branch checkouts alone, so the new pin may not be here yet
    if git(Some(dir), &["cat-file", "-e", &format!("{pin}^{{commit}}")]).is_err() {
        fetch::verify_tag(u.c)?;
        fetch::fetch_pin(u.c, dir)?;
    }
    let resumed = git(Some(dir), &["merge-base", "--is-ancestor", pin, BRANCH]).is_ok();
    if !resumed {
        git_isolated(dir, &["rebase", "-q", "--onto", pin, &old, BRANCH], Some(ident)).with_context(
            || {
                format!(
                    "conflicts: resolve in {}, `git rebase --continue`, then rerun xtask patch --rebase {}",
                    dir.display(),
                    u.name()
                )
            },
        )?;
    }
    git(Some(dir), &["update-ref", BASE_REF, pin])?;
    Ok(format!(
        "on {}; now xtask patch --export {}",
        &pin[..12],
        u.name()
    ))
}

/// `<pin>..atl-patches` as patch files, byte-for-byte reproducible: global git config ignored,
/// commit ids zeroed, full blob ids, no signature (`--no-stat` would drop the `---` line); each gets the SPDX expression of the files
/// it touches after its `---` line, where `git am` ignores it.
fn exported(u: &Up, licenses: &Licenses) -> Result<Vec<(String, String)>> {
    let dir = &u.checkout;
    let range = format!("{}..{BRANCH}", u.c.commit);
    let commits = commits_with_trailers(dir, &range)?;
    let tmp = std::env::temp_dir().join(format!("xtask-patch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let out = tmp.display().to_string();
    let args = [
        "format-patch",
        "-q",
        "--zero-commit",
        "--no-signature",
        "--no-numbered",
        "--no-thread",
        "--full-index",
        "--binary",
        "-o",
        &out,
        &range,
    ];
    let result = (|| {
        git_isolated(dir, &args, None)?;
        let files = stored(&tmp)?;
        ensure!(
            files.len() == commits.len(),
            "format-patch wrote {} files for {} commits",
            files.len(),
            commits.len()
        );
        files
            .into_iter()
            .zip(&commits)
            .map(|((name, text), commit)| {
                let expr = patch_licence(dir, commit, &u.c.licence_key, licenses)?;
                Ok((name, insert_spdx(&text, &expr)?))
            })
            .collect()
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// Commits of `range`, oldest first, after checking each carries the required trailers.
fn commits_with_trailers(dir: &Path, range: &str) -> Result<Vec<String>> {
    let fields: String = TRAILERS
        .iter()
        .map(|t| format!("%x1f%(trailers:key={t},valueonly)"))
        .collect();
    let log = git(
        Some(dir),
        &[
            "log",
            "--reverse",
            &format!("--format=%H%x1f%s{fields}%x1e"),
            range,
        ],
    )?;
    log.split('\x1e')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|record| {
            let f: Vec<&str> = record.split('\x1f').collect();
            for (i, t) in TRAILERS.iter().enumerate() {
                ensure!(
                    f.get(i + 2).is_some_and(|v| !v.trim().is_empty()),
                    "commit {} \"{}\" lacks a `{t}:` trailer",
                    &f[0][..12.min(f[0].len())],
                    f.get(1).unwrap_or(&"")
                );
            }
            Ok(f[0].to_owned())
        })
        .collect()
}

/// Licence of everything `commit` touches, each file (old version, else new) resolved by
/// `notice::resolve`; several are joined with AND.
fn patch_licence(dir: &Path, commit: &str, key: &str, licenses: &Licenses) -> Result<String> {
    let paths = git(
        Some(dir),
        &["diff", "--name-only", &format!("{commit}^"), commit],
    )?;
    let mut exprs = BTreeSet::new();
    for path in paths.lines() {
        let text = git(Some(dir), &["show", &format!("{commit}^:{path}")])
            .or_else(|_| git(Some(dir), &["show", &format!("{commit}:{path}")]))?;
        let expr = notice::resolve(path, &text, licenses.upstream.get(key))?;
        ::spdx::Expression::parse(&expr).with_context(|| format!("{path}: bad SPDX `{expr}`"))?;
        exprs.insert(expr);
    }
    ensure!(!exprs.is_empty(), "commit {commit} touches no files");
    let parts: Vec<String> = exprs
        .into_iter()
        .map(|e| {
            if e.contains(" OR ") {
                format!("({e})")
            } else {
                e
            }
        })
        .collect();
    Ok(parts.join(" AND "))
}

fn insert_spdx(patch: &str, expr: &str) -> Result<String> {
    let at = patch
        .find("\n---\n")
        .context("patch has no `---` separator")?
        + "\n---\n".len();
    Ok(format!(
        "{}SPDX-License-Identifier: {expr}\n{}",
        &patch[..at],
        &patch[at..]
    ))
}

/// `*.patch` files of a directory, sorted by name, with their contents.
fn stored(dir: &Path) -> Result<Vec<(String, String)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "patch") {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push((name, std::fs::read_to_string(&path)?));
        }
    }
    out.sort();
    Ok(out)
}

fn is_dirty(dir: &Path) -> Result<bool> {
    Ok(!git(
        Some(dir),
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty())
}

fn on_branch(dir: &Path) -> Result<bool> {
    Ok(git(Some(dir), &["symbolic-ref", "-q", "--short", "HEAD"]).is_ok_and(|b| b == BRANCH))
}

fn base(dir: &Path) -> Result<Option<String>> {
    Ok(git(Some(dir), &["rev-parse", "-q", "--verify", BASE_REF]).ok())
}

fn rebase_in_progress(dir: &Path) -> Result<bool> {
    for state in ["rebase-merge", "rebase-apply"] {
        let p = git(Some(dir), &["rev-parse", "--git-path", state])?;
        if dir.join(p).exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::Pin;

    const IDENT: [&str; 4] = ["-c", "user.name=t", "-c", "user.email=t@t"];

    /// Temp tree: an upstream repo with tag v1, and a checkout of it at v1.
    struct Fixture {
        root: PathBuf,
        upstream: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("xtask-patch-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let upstream = root.join("upstream");
            std::fs::create_dir_all(&upstream).unwrap();
            let f = Fixture { root, upstream };
            f.up(&["init", "-q"]);
            std::fs::write(
                f.upstream.join("a.c"),
                "// SPDX-License-Identifier: GPL-2.0-or-later\nint a;\n",
            )
            .unwrap();
            f.up(&["add", "a.c"]);
            f.commit_upstream("one");
            f.up(&[
                IDENT[0], IDENT[1], IDENT[2], IDENT[3], "tag", "-a", "v1", "-m", "v1",
            ]);
            f
        }

        fn up(&self, args: &[&str]) -> String {
            git(Some(&self.upstream), args).unwrap()
        }

        fn commit_upstream(&self, msg: &str) -> String {
            self.up(&[
                IDENT[0], IDENT[1], IDENT[2], IDENT[3], "commit", "-q", "-m", msg,
            ]);
            self.up(&["rev-parse", "HEAD"])
        }

        fn checkout(&self, tag: &str) -> Checkout {
            Checkout {
                dir: PathBuf::from("up"),
                url: format!("file://{}", self.upstream.display()),
                tag: tag.into(),
                commit: self.up(&["rev-parse", &format!("{tag}^{{commit}}")]),
                pin: Pin::Git("up".into()),
                licence_key: "up".into(),
            }
        }

        /// A fetched checkout of `c` under `<root>/<sub>`.
        fn fetched<'a>(&self, c: &'a Checkout, sub: &str) -> Up<'a> {
            let u = Up {
                c,
                checkout: self.root.join(sub),
                patches: self.root.join("patches"),
            };
            std::fs::create_dir_all(&u.checkout).unwrap();
            git(Some(&u.checkout), &["init", "-q"]).unwrap();
            fetch::fetch_pin(c, &u.checkout).unwrap();
            git(
                Some(&u.checkout),
                &["checkout", "-q", "--detach", &c.commit],
            )
            .unwrap();
            u
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn licenses() -> Licenses {
        toml::from_str("[artifact.runtime]\nallow = [\"MIT\"]\n").unwrap()
    }

    fn ident() -> Ident {
        Ident {
            name: "t".into(),
            email: "t@t".into(),
        }
    }

    /// Start atl-patches at the pin and commit a change to a.c with `msg`.
    fn patch_commit(u: &Up, msg: &str) {
        let d = &u.checkout;
        if !on_branch(d).unwrap() {
            git(Some(d), &["checkout", "-q", "-B", BRANCH, &u.c.commit]).unwrap();
            git(Some(d), &["update-ref", BASE_REF, &u.c.commit]).unwrap();
        }
        let a = d.join("a.c");
        let text = std::fs::read_to_string(&a).unwrap();
        std::fs::write(&a, format!("{text}int more;\n")).unwrap();
        git(
            Some(d),
            &[
                IDENT[0], IDENT[1], IDENT[2], IDENT[3], "commit", "-q", "-am", msg,
            ],
        )
        .unwrap();
    }

    const TRAILED: &str = "fix a\n\nUpstream-Status: Pending\nPatch-Reason: test\n";

    #[test]
    fn export_drift_apply() {
        let f = Fixture::new("cycle");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "src");
        patch_commit(&u, TRAILED);

        let out = exported(&u, &licenses()).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].0.starts_with("0001-fix-a"), "{}", out[0].0);
        // licence of a.c (its SPDX tag) right after the `---` separator
        assert!(
            out[0]
                .1
                .contains("\n---\nSPDX-License-Identifier: GPL-2.0-or-later\n"),
            "{}",
            out[0].1
        );
        // byte-for-byte stable
        assert_eq!(exported(&u, &licenses()).unwrap(), out);

        // unexported commit = drift; after export = in step
        assert!(drift(&u, &licenses()).unwrap().unwrap()[0].contains("differs from patches/"));
        export(&u, &licenses()).unwrap();
        assert_eq!(
            drift(&u, &licenses()).unwrap().unwrap(),
            Vec::<String>::new()
        );

        // a fresh checkout gets the same tree from the exported patches
        let fresh = Up {
            c: &c,
            checkout: f.root.join("src2"),
            patches: u.patches.clone(),
        };
        let _ = f.fetched(&c, "src2");
        assert_eq!(
            apply(&fresh, &ident(), &licenses()).unwrap(),
            "applied 1 patches"
        );
        let tree = |d: &Path| git(Some(d), &["rev-parse", "HEAD^{tree}"]).unwrap();
        assert_eq!(tree(&fresh.checkout), tree(&u.checkout));
        assert_eq!(
            apply(&fresh, &ident(), &licenses()).unwrap(),
            "already applied"
        );
    }

    #[test]
    fn missing_trailer_blocks_export() {
        let f = Fixture::new("trailer");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "src");
        patch_commit(&u, "no trailers here");
        let e = format!("{:#}", exported(&u, &licenses()).unwrap_err());
        assert!(e.contains("lacks a `Upstream-Status:` trailer"), "{e}");
    }

    #[test]
    fn rebase_onto_new_pin() {
        let f = Fixture::new("rebase");
        let c1 = f.checkout("v1");
        let u1 = f.fetched(&c1, "src");
        patch_commit(&u1, TRAILED);
        // upstream moves on: a new file and tag v2
        std::fs::write(f.upstream.join("b.c"), "int b;\n").unwrap();
        f.up(&["add", "b.c"]);
        f.commit_upstream("two");
        f.up(&[
            IDENT[0], IDENT[1], IDENT[2], IDENT[3], "tag", "-a", "v2", "-m", "v2",
        ]);
        let c2 = f.checkout("v2");
        let u2 = Up {
            c: &c2,
            checkout: u1.checkout.clone(),
            patches: u1.patches.clone(),
        };

        assert!(drift(&u2, &licenses()).unwrap().unwrap()[0].contains("pin moved"));
        rebase(&u2, &ident()).unwrap();
        assert_eq!(
            base(&u2.checkout).unwrap().as_deref(),
            Some(c2.commit.as_str())
        );
        assert!(u2.checkout.join("b.c").exists());
        assert!(
            std::fs::read_to_string(u2.checkout.join("a.c"))
                .unwrap()
                .contains("int more;")
        );
        assert_eq!(rebase(&u2, &ident()).unwrap(), "already on the pin");
    }

    #[test]
    fn licence_join_and_insert() {
        assert_eq!(
            insert_spdx("Subject: x\n\nbody\n---\ndiff --git a b\n", "MIT").unwrap(),
            "Subject: x\n\nbody\n---\nSPDX-License-Identifier: MIT\ndiff --git a b\n"
        );
        assert!(insert_spdx("no separator", "MIT").is_err());
    }
}
