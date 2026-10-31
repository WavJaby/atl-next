// SPDX-License-Identifier: MIT
//! `xtask patch`: our commits on upstream checkouts, kept as `patches/<upstream>/*.patch` and
//! re-applied on branch `atl-patches` by `xtask fetch`; plus the drift check that keeps the two identical.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail, ensure};

use crate::git::{Ident, git, git_isolated};
use crate::licenses::{self, Licenses};
use crate::sources::{self, Checkout, Pin};
use crate::{aosp, fetch, notice, workspace};

pub const BRANCH: &str = "atl-patches";
/// Commit `atl-patches` is based on. A rebase after a pin bump needs the old base, and shallow
/// checkouts hold no history linking two pins. A branch made by hand at the pin needs none (`base`).
const BASE_REF: &str = "refs/atl/base";
/// Trailers every patch commit carries: upstream status (Yocto values: Pending, Submitted [where],
/// Backport, Denied, Inappropriate [why]) and why the change cannot live in the launcher/loader/env.
const TRAILERS: [&str; 2] = ["Upstream-Status", "Patch-Reason"];

#[derive(clap::Args)]
pub struct Args {
    /// Write <pin>..atl-patches of these upstreams (e.g. qemu, aosp/art) to patches/<upstream>/.
    #[arg(long, value_name = "UPSTREAM", num_args = 1.., conflicts_with = "rebase")]
    export: Vec<String>,
    /// Move atl-patches of these upstreams from their old base onto the current pin (after a bump).
    #[arg(long, value_name = "UPSTREAM", num_args = 1..)]
    rebase: Vec<String>,
}

/// One upstream as this module sees it.
pub struct Up<'a> {
    c: &'a Checkout,
    checkout: PathBuf,
    patches: PathBuf,
}

impl<'a> Up<'a> {
    /// `c` checked out under `src`, its patches under `<root>/patches`.
    pub fn new(c: &'a Checkout, src: &Path, root: &Path) -> Self {
        Up {
            c,
            checkout: src.join(&c.dir),
            patches: root.join("patches").join(&c.dir),
        }
    }

    fn name(&self) -> String {
        self.c.dir.display().to_string()
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let sources = sources::load(root)?;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    // --rebase fetches, so AOSP rows need their URLs from the manifest
    let names = aosp::load(root, &sources.aosp)?.map(|m| m.names());
    let all = sources.checkouts(names.as_ref());
    let src = workspace::root_dir(root)?.join("src");
    let ups: Vec<Up> = all.iter().map(|c| Up::new(c, &src, root)).collect();
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
    if !args.export.is_empty() {
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

/// The drift check behind `xtask check`: every present checkout matches its pin and patches/.
pub fn check(root: &Path) -> Result<()> {
    let sources = sources::load(root)?;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let src = workspace::root_dir(root)?.join("src");
    let all = sources.checkouts(None);
    let ups: Vec<Up> = all.iter().map(|c| Up::new(c, &src, root)).collect();
    let (present, problems) = problems(&ups, &licenses);
    for p in &problems {
        eprintln!("drift: {p}");
    }
    ensure!(problems.is_empty(), "{} drift problems", problems.len());
    eprintln!(
        "drift: {present} checkouts in step, {} not fetched",
        all.len() - present
    );
    Ok(())
}

/// `xtask native`'s preflight: the build compiles the checkouts it reads as they are, so each must be
/// its pin + patches/. It reads every AOSP project (the converter takes the whole tree) and `upstreams`.
pub fn preflight<'a>(root: &Path, upstreams: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let sources = sources::load(root)?;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let src = workspace::root_dir(root)?.join("src");
    let upstreams: BTreeSet<&str> = upstreams.into_iter().collect();
    let all = sources.checkouts(None);
    let ups: Vec<Up> = all
        .iter()
        .filter(|c| built_from(c, &upstreams))
        .map(|c| Up::new(c, &src, root))
        .collect();
    in_step(&ups, &licenses)
}

fn built_from(c: &Checkout, upstreams: &BTreeSet<&str>) -> bool {
    match &c.pin {
        Pin::Aosp(_) => true,
        Pin::Git(name) => upstreams.contains(name.as_str()),
        Pin::Manifest => false,
    }
}

fn in_step(ups: &[Up], licenses: &Licenses) -> Result<()> {
    let (_, problems) = problems(ups, licenses);
    ensure!(
        problems.is_empty(),
        "upstream checkouts are not their pin + patches/:\n  {}",
        problems.join("\n  ")
    );
    Ok(())
}

/// Checkouts present, and what keeps each from matching its pin and patches/.
fn problems(ups: &[Up], licenses: &Licenses) -> (usize, Vec<String>) {
    let (mut present, mut problems) = (0, Vec::new());
    for u in ups {
        match drift(u, licenses) {
            Ok(None) => {}
            Ok(Some(p)) => {
                present += 1;
                problems.extend(p.into_iter().map(|p| format!("{}: {p}", u.name())));
            }
            Err(e) => problems.push(format!("{}: {e:#}", u.name())),
        }
    }
    (present, problems)
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
        match base(u)? {
            None => problems.push(no_base(u)),
            Some(b) if b != u.c.commit => problems.push(format!(
                "pin moved: xtask fetch --execute (xtask patch --rebase {} if it refuses)",
                u.name()
            )),
            Some(b) => match branch_patches(u, &b, licenses) {
                Ok(branch) if branch == stored => {}
                Ok(branch) if unexported(&branch, &stored).is_empty() => problems.push(format!(
                    "patches/{} holds patches {BRANCH} lacks: xtask fetch --execute",
                    u.name()
                )),
                Ok(_) => problems.push(format!(
                    "{BRANCH} differs from patches/{0}: xtask patch --export {0}",
                    u.name()
                )),
                Err(e) => problems.push(format!("{e:#}")),
            },
        }
    } else if !stored.is_empty() {
        problems.push(format!(
            "{} patches not applied: xtask fetch --execute",
            stored.len()
        ));
    } else if git(Some(dir), &["rev-parse", "HEAD"])? != u.c.commit {
        problems.push("not at pin: xtask fetch --execute".to_owned());
    }
    Ok(Some(problems))
}

/// `xtask fetch`'s second step, once the checkout is at its pin or on atl-patches: make atl-patches
/// the pin + patches/<upstream>. Refuses rather than drop a commit patches/ does not hold. Without
/// `execute` only says what it would do. None = nothing to report.
pub fn settle(
    u: &Up,
    licenses: &Licenses,
    ident: Option<&Ident>,
    execute: bool,
) -> Result<Option<String>> {
    let dir = &u.checkout;
    let stored = stored(&u.patches)?;
    let n = stored.len();
    if !dir.exists() {
        // dry run of a clone
        return Ok((n > 0).then(|| format!("then apply {n} patches")));
    }
    let on = on_branch(dir)?;
    if !on && stored.is_empty() {
        return Ok(None);
    }
    ensure!(
        !rebase_in_progress(dir)?,
        "rebase in progress: finish it (git rebase --continue / --abort) first"
    );
    let pin = u.c.commit.as_str();
    if has_branch(dir)? {
        let b = base(u)?.with_context(|| no_base(u))?;
        let branch = branch_patches(u, &b, licenses)
            .context("atl-patches holds commits patches/ cannot hold; fix them, then --export")?;
        let lost = unexported(&branch, &stored);
        if !lost.is_empty() {
            let how = if b == pin {
                format!("xtask patch --export {}", u.name())
            } else {
                format!("xtask patch --rebase {0}, then --export {0}", u.name())
            };
            bail!(
                "{BRANCH} holds {} commits not in patches/{} ({}): {how}; or, if they are stale, `git -C {} checkout --detach && git -C {} branch -D {BRANCH}`",
                lost.len(),
                u.name(),
                lost.join(", "),
                dir.display(),
                dir.display()
            );
        }
        if on && b == pin && branch == stored {
            if execute && git(Some(dir), &["rev-parse", "-q", "--verify", BASE_REF]).is_err() {
                git(Some(dir), &["update-ref", BASE_REF, pin])?;
            }
            return Ok(is_dirty(dir)?.then(|| "uncommitted changes".to_owned()));
        }
    }
    ensure!(!is_dirty(dir)?, "uncommitted changes: patches not applied");
    if !execute {
        return Ok(Some(format!("apply {n} patches")));
    }
    let ident = ident.context(
        "git user.name / user.email unset in the repo checkout: git am needs a committer",
    )?;
    rebuild(u, &stored, ident)?;
    Ok(Some(format!("applied {n} patches")))
}

/// atl-patches := pin + `stored` (git am). On failure the checkout's branch, HEAD and base go back
/// to what they were.
fn rebuild(u: &Up, stored: &[(String, String)], ident: &Ident) -> Result<()> {
    let dir = &u.checkout;
    let pin = u.c.commit.as_str();
    // fetch leaves a checkout on atl-patches where it is, so a new pin may not be here yet
    if git(Some(dir), &["cat-file", "-e", &format!("{pin}^{{commit}}")]).is_err() {
        fetch::verify_tag(u.c)?;
        fetch::fetch_pin(u.c, dir)?;
    }
    let was_on = on_branch(dir)?;
    let head = git(Some(dir), &["rev-parse", "HEAD"])?;
    let branch_ref = format!("refs/heads/{BRANCH}");
    let old_branch = git(Some(dir), &["rev-parse", "-q", "--verify", &branch_ref]).ok();
    let old_base = git(Some(dir), &["rev-parse", "-q", "--verify", BASE_REF]).ok();
    git(Some(dir), &["checkout", "-q", "-B", BRANCH, pin])?;
    git(Some(dir), &["update-ref", BASE_REF, pin])?;
    let files: Vec<String> = stored
        .iter()
        .map(|(name, _)| u.patches.join(name).display().to_string())
        .collect();
    if files.is_empty() {
        // `git am` without files reads stdin
        return Ok(());
    }
    let mut args = vec!["am", "-q", "--committer-date-is-author-date"];
    args.extend(files.iter().map(String::as_str));
    let Err(e) = git_isolated(dir, &args, Some(ident)) else {
        return Ok(());
    };
    let _ = git(Some(dir), &["am", "--abort"]);
    let restored = (|| -> Result<()> {
        git(Some(dir), &["checkout", "-q", "--detach", &head])?;
        match &old_branch {
            Some(b) => git(Some(dir), &["branch", "-q", "-f", BRANCH, b])?,
            None => git(Some(dir), &["branch", "-q", "-D", BRANCH])?,
        };
        if was_on {
            git(Some(dir), &["checkout", "-q", BRANCH])?;
        }
        match &old_base {
            Some(b) => git(Some(dir), &["update-ref", BASE_REF, b])?,
            None => git(Some(dir), &["update-ref", "-d", BASE_REF])?,
        };
        Ok(())
    })();
    let moved = old_base.as_deref().is_some_and(|b| b != pin);
    let next = if moved {
        format!(
            ": xtask patch --rebase {0}, resolve, then --export {0}",
            u.name()
        )
    } else {
        String::new()
    };
    match restored {
        Ok(()) => Err(e.context(format!(
            "patches/{} does not apply at the pin; checkout restored{next}",
            u.name()
        ))),
        Err(r) => Err(e.context(format!(
            "patches/{} does not apply at the pin, and restoring the checkout failed: {r:#}",
            u.name()
        ))),
    }
}

fn export(u: &Up, licenses: &Licenses) -> Result<String> {
    let dir = &u.checkout;
    ensure!(
        dir.exists() && on_branch(dir)?,
        "not on {BRANCH}: nothing to export"
    );
    ensure!(
        base(u)?.as_deref() == Some(u.c.commit.as_str()),
        "{BRANCH} is not based on the pin: xtask patch --rebase {}",
        u.name()
    );
    let patches = branch_patches(u, &u.c.commit, licenses)?;
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
    let old = base(u)?.with_context(|| no_base(u))?;
    if old == pin {
        return Ok("already on the pin".to_owned());
    }
    // `xtask fetch` refused this branch, so the new pin may not be here yet
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

/// Commits of `branch` whose patch text `stored` lacks: work that dropping the branch would lose.
/// Texts, not names: a name carries its position, a text is the commit's whole content.
fn unexported(branch: &[(String, String)], stored: &[(String, String)]) -> Vec<String> {
    let have: BTreeSet<&str> = stored.iter().map(|(_, t)| t.as_str()).collect();
    branch
        .iter()
        .filter(|(_, t)| !have.contains(t.as_str()))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Distinct temp dirs for concurrent exports in one process (`xtask fetch` settles in parallel).
static EXPORTS: AtomicUsize = AtomicUsize::new(0);

/// `<base>..atl-patches` as patch files, byte-for-byte reproducible: global git config ignored,
/// commit ids zeroed, full blob ids, no signature (`--no-stat` would drop the `---` line); each gets the SPDX expression of the files
/// it touches after its `---` line, where `git am` ignores it.
fn branch_patches(u: &Up, base: &str, licenses: &Licenses) -> Result<Vec<(String, String)>> {
    let dir = &u.checkout;
    let range = format!("{base}..{BRANCH}");
    let commits = commits_with_trailers(dir, &range)?;
    let tmp = std::env::temp_dir().join(format!(
        "xtask-patch-{}-{}",
        std::process::id(),
        EXPORTS.fetch_add(1, Ordering::Relaxed)
    ));
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

fn has_branch(dir: &Path) -> Result<bool> {
    Ok(git(
        Some(dir),
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/heads/{BRANCH}"),
        ],
    )
    .is_ok())
}

/// What atl-patches is based on: `BASE_REF`, else (a branch started by hand) the newest tag it contains -- a shallow checkout holds only the pinned tags, so that is the pin it was started at, even after a bump.
fn base(u: &Up) -> Result<Option<String>> {
    let dir = &u.checkout;
    if let Ok(b) = git(Some(dir), &["rev-parse", "-q", "--verify", BASE_REF]) {
        return Ok(Some(b));
    }
    let Ok(tag) = git(Some(dir), &["describe", "--tags", "--abbrev=0", BRANCH]) else {
        return Ok(None);
    };
    Ok(Some(git(
        Some(dir),
        &["rev-parse", &format!("refs/tags/{tag}^{{commit}}")],
    )?))
}

fn no_base(u: &Up) -> String {
    format!(
        "{BRANCH} has no {BASE_REF} and contains no pinned tag: rename it (`git -C {} branch -m {BRANCH} <name>`), then start {BRANCH} at the pin {}",
        u.checkout.display(),
        u.c.commit
    )
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

/// Throwaway upstreams and checkouts for the tests here and in `fetch`.
#[cfg(test)]
pub mod fixture {
    use super::*;

    const IDENT: [&str; 4] = ["-c", "user.name=t", "-c", "user.email=t@t"];

    /// A trailed patch commit message.
    pub const TRAILED: &str = "fix a\n\nUpstream-Status: Pending\nPatch-Reason: test\n";

    /// Temp tree `<root>`: `upstream/` (a repo with a.c, tag v1); checkouts go under `<root>/<src>/up`,
    /// patches under `<root>/patches/up`.
    pub struct Fixture {
        pub root: PathBuf,
        upstream: PathBuf,
    }

    impl Fixture {
        pub fn new(name: &str) -> Self {
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
            f.release("one", "v1");
            f
        }

        fn up(&self, args: &[&str]) -> String {
            git(Some(&self.upstream), args).unwrap()
        }

        /// Commit everything in the upstream and tag it.
        pub fn release(&self, msg: &str, tag: &str) {
            self.up(&["add", "-A"]);
            self.up(&[
                IDENT[0], IDENT[1], IDENT[2], IDENT[3], "commit", "-q", "-m", msg,
            ]);
            self.up(&[
                IDENT[0], IDENT[1], IDENT[2], IDENT[3], "tag", "-a", tag, "-m", tag,
            ]);
        }

        /// Write a file of the upstream (committed by the next `release`).
        pub fn write(&self, file: &str, text: &str) {
            std::fs::write(self.upstream.join(file), text).unwrap();
        }

        pub fn checkout(&self, tag: &str) -> Checkout {
            Checkout {
                dir: PathBuf::from("up"),
                url: Some(format!("file://{}", self.upstream.display())),
                tag: tag.into(),
                commit: self.up(&["rev-parse", &format!("{tag}^{{commit}}")]),
                pin: Pin::Git("up".into()),
                licence_key: "up".into(),
            }
        }

        /// `<root>/<src>`: a src directory as `xtask fetch` sees it.
        pub fn src(&self, src: &str) -> PathBuf {
            self.root.join(src)
        }

        /// `c` under `<root>/<src>`, whether checked out or not.
        pub fn at<'a>(&self, c: &'a Checkout, src: &str) -> Up<'a> {
            Up::new(c, &self.src(src), &self.root)
        }

        /// A checkout of `c` under `<root>/<src>`, detached at the pin.
        pub fn fetched<'a>(&self, c: &'a Checkout, src: &str) -> Up<'a> {
            let u = self.at(c, src);
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

    pub fn licenses() -> Licenses {
        toml::from_str("[artifact.runtime]\nallow = [\"MIT\"]\n").unwrap()
    }

    pub fn ident() -> Ident {
        Ident {
            name: "t".into(),
            email: "t@t".into(),
        }
    }

    /// Start atl-patches at the pin (as a developer does by hand) unless on it, and commit a line
    /// added to a.c with `msg`.
    pub fn patch_commit(u: &Up, msg: &str) {
        let d = &u.checkout;
        if !on_branch(d).unwrap() {
            git(Some(d), &["switch", "-q", "-c", BRANCH, &u.c.commit]).unwrap();
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

    pub fn export_ok(u: &Up) -> String {
        super::export(u, &licenses()).unwrap()
    }

    /// The drift test's problems for `u` (empty = in step).
    pub fn drift_of(u: &Up) -> Vec<String> {
        super::drift(u, &licenses()).unwrap().unwrap()
    }

    pub fn head(u: &Up) -> String {
        git(Some(&u.checkout), &["rev-parse", "HEAD"]).unwrap()
    }

    pub fn tree(u: &Up) -> String {
        git(Some(&u.checkout), &["rev-parse", "HEAD^{tree}"]).unwrap()
    }

    pub fn on_patch_branch(u: &Up) -> bool {
        on_branch(&u.checkout).unwrap()
    }

    pub fn base_ref(u: &Up) -> Option<String> {
        git(
            Some(&u.checkout),
            &["rev-parse", "-q", "--verify", BASE_REF],
        )
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    #[test]
    fn export_is_reproducible_and_clears_drift() {
        let f = Fixture::new("cycle");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "src");
        patch_commit(&u, TRAILED);

        let out = branch_patches(&u, &c.commit, &licenses()).unwrap();
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
        assert_eq!(branch_patches(&u, &c.commit, &licenses()).unwrap(), out);

        // a branch started by hand at the pin needs no base ref; its unexported commit is drift
        assert_eq!(base_ref(&u), None);
        assert!(drift_of(&u)[0].contains("xtask patch --export up"));
        export_ok(&u);
        assert_eq!(drift_of(&u), Vec::<String>::new());
    }

    #[test]
    fn missing_trailer_blocks_export() {
        let f = Fixture::new("trailer");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "src");
        patch_commit(&u, "no trailers here");
        let e = format!(
            "{:#}",
            branch_patches(&u, &c.commit, &licenses()).unwrap_err()
        );
        assert!(e.contains("lacks a `Upstream-Status:` trailer"), "{e}");
    }

    #[test]
    fn rebase_onto_new_pin() {
        let f = Fixture::new("rebase");
        let c1 = f.checkout("v1");
        let u1 = f.fetched(&c1, "src");
        patch_commit(&u1, TRAILED);
        // upstream moves on: a new file and tag v2
        f.write("b.c", "int b;\n");
        f.release("two", "v2");
        let c2 = f.checkout("v2");
        let u2 = f.at(&c2, "src");

        assert!(drift_of(&u2)[0].contains("pin moved"));
        rebase(&u2, &ident()).unwrap();
        assert_eq!(base_ref(&u2).as_deref(), Some(c2.commit.as_str()));
        assert!(u2.checkout.join("b.c").exists());
        assert!(
            std::fs::read_to_string(u2.checkout.join("a.c"))
                .unwrap()
                .contains("int more;")
        );
        assert_eq!(rebase(&u2, &ident()).unwrap(), "already on the pin");
    }

    /// The guards of `xtask native`'s preflight, each made to fire.
    #[test]
    fn preflight_guards() {
        let f = Fixture::new("preflight");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "src");
        let check =
            |u: &Up| in_step(std::slice::from_ref(u), &licenses()).map_err(|e| format!("{e:#}"));
        // detached at the pin, no patches: in step
        check(&u).unwrap();
        // an uncommitted edit
        std::fs::write(u.checkout.join("a.c"), "int edited;\n").unwrap();
        let e = check(&u).unwrap_err();
        assert!(e.contains("up: uncommitted changes"), "{e}");
        git(Some(&u.checkout), &["checkout", "-q", "--", "a.c"]).unwrap();
        // a commit patches/ lacks
        patch_commit(&u, TRAILED);
        let e = check(&u).unwrap_err();
        assert!(
            e.contains("atl-patches differs from patches/up: xtask patch --export up"),
            "{e}"
        );
        // exported: in step
        export_ok(&u);
        check(&u).unwrap();
        // patches/ ahead of the branch (a merged patch not applied yet): fetch, not export
        git(Some(&u.checkout), &["reset", "-q", "--hard", &c.commit]).unwrap();
        let e = check(&u).unwrap_err();
        assert!(
            e.contains("patches/up holds patches atl-patches lacks: xtask fetch --execute"),
            "{e}"
        );
    }

    #[test]
    fn branch_without_base_is_refused() {
        let f = Fixture::new("nobase");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "src");
        // a branch sharing no history with any pinned tag: nothing says what its commits are relative to
        let d = &u.checkout;
        git(Some(d), &["switch", "-q", "--orphan", BRANCH]).unwrap();
        std::fs::write(d.join("a.c"), "int x;\n").unwrap();
        git(Some(d), &["add", "a.c"]).unwrap();
        git(
            Some(d),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                TRAILED,
            ],
        )
        .unwrap();
        assert!(drift_of(&u)[0].contains("contains no pinned tag"));
        let e = format!(
            "{:#}",
            settle(&u, &licenses(), Some(&ident()), true).unwrap_err()
        );
        assert!(e.contains("contains no pinned tag"), "{e}");
    }

    #[test]
    fn preflight_reads_aosp_and_built_upstreams() {
        let c = |pin: Pin| Checkout {
            dir: PathBuf::from("x"),
            url: None,
            tag: "t".into(),
            commit: "c".into(),
            pin,
            licence_key: "x".into(),
        };
        let built = BTreeSet::from(["boringssl"]);
        assert!(built_from(
            &c(Pin::Aosp("frameworks/native".into())),
            &built
        ));
        assert!(built_from(&c(Pin::Git("boringssl".into())), &built));
        assert!(!built_from(&c(Pin::Git("qemu".into())), &built));
        assert!(!built_from(&c(Pin::Manifest), &built));
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
