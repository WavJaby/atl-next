// SPDX-License-Identifier: MIT
//! `xtask patch`: our commits on upstream checkouts, kept as `patches/<upstream>/*.patch` and
//! re-applied on the checkout's patch branch by `xtask fetch`; the task copies (`--edit`, `--done`) a task edits them in; plus the drift check that keeps checkout and patches/ identical.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::aosp::Manifest;
use crate::git::{Ident, git, git_isolated, git_raw};
use crate::licenses::{self, Licenses};
use crate::prune::{self, Prune};
use crate::sources::{self, Checkout, Pin, Sources};
use crate::{aosp, farm, fetch, notice};

/// Patch branch of a shared `src/` checkout; a task copy's is `atl-patches-<task>` (git branches are shared by a repository's worktrees).
pub const BRANCH: &str = "atl-patches";
/// Commit the patch branch is based on. A rebase after a pin bump needs the old base, and shallow
/// checkouts hold no history linking two pins. Per worktree (`refs/worktree/`), so `src/<up>` and each task copy keep their own. A branch made by hand at the pin needs none (`base`).
const BASE_REF: &str = "refs/worktree/atl/base";
/// Trailers every patch commit carries: upstream status (Yocto values: Pending, Submitted [where],
/// Backport, Denied, Inappropriate [why]) and why the change cannot live in the launcher/loader/env.
const TRAILERS: [&str; 2] = ["Upstream-Status", "Patch-Reason"];
/// Untracked files named in a drift message; the rest are counted.
const STRAYS_SHOWN: usize = 3;
/// `patches/<upstream>/LICENSE` is a copy of the first of these at the top of the upstream at its pin, written by `--export` and compared by the drift test.
/// AOSP projects ship no LICENSE: their licence text is `NOTICE` (`MODULE_LICENSE_*` is an empty marker naming it), so NOTICE counts, after the conventional names.
const LICENCE_FILES: [&str; 3] = ["LICENSE", "COPYING", "NOTICE"];
const LICENCE_COPY: &str = "LICENSE";
/// The fix for patches/ ahead of a checkout: the normal state on main between merging a patch change and the next fetch.
const FETCH: &str = "cargo xtask fetch --execute (the step after merging a change to patches/)";

#[derive(clap::Args)]
pub struct Args {
    /// Write <pin>..<patch branch> of these upstreams (e.g. qemu, aosp/art) to patches/<upstream>/, from the task copy where there is one, with LICENSE = the upstream's LICENSE, COPYING or NOTICE at the pin (first found). Refused with no commits past the pin.
    #[arg(long, value_name = "UPSTREAM", num_args = 1.., conflicts_with_all = ["rebase", "edit", "done"])]
    export: Vec<String>,
    /// Move the patch branch of these upstreams from their old base onto the current pin (after a bump).
    #[arg(long, value_name = "UPSTREAM", num_args = 1.., conflicts_with_all = ["edit", "done"])]
    rebase: Vec<String>,
    /// Make task copies of these upstreams for this repo checkout's branch <task>: src-wt/<task>/<upstream>, a git worktree of src/<upstream> on atl-patches-<task> = pin + this checkout's patches/<upstream>/. Every xtask command then uses it instead of src/<upstream>.
    #[arg(long, value_name = "UPSTREAM", num_args = 1.., conflicts_with = "done")]
    edit: Vec<String>,
    /// Remove these task copies and their branch; refused while one holds uncommitted, untracked or unexported work.
    #[arg(long, value_name = "UPSTREAM", num_args = 1..)]
    done: Vec<String>,
}

/// One upstream as this module sees it: the checkout the layout resolves, and its patch branch.
pub struct Up<'a> {
    c: &'a Checkout,
    checkout: PathBuf,
    patches: PathBuf,
    branch: String,
    /// The task owning the checkout; None = shared `src/`.
    task: Option<String>,
    /// Untracked files xtask itself puts there (`declared`), paths inside the checkout.
    untracked: BTreeSet<String>,
}

impl<'a> Up<'a> {
    /// `c` where `layout` resolves it, its patches under `<root>/patches`.
    pub fn new(c: &'a Checkout, layout: &Layout, root: &Path) -> Self {
        let l = layout.locate(&c.dir);
        Up {
            c,
            branch: match &l.task {
                Some(t) => format!("{BRANCH}-{t}"),
                None => BRANCH.to_owned(),
            },
            checkout: l.dir,
            task: l.task,
            patches: root.join("patches").join(&c.dir),
            untracked: BTreeSet::new(),
        }
    }

    fn name(&self) -> String {
        self.c.dir.display().to_string()
    }

    pub fn checkout(&self) -> &Path {
        &self.checkout
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn is_shared(&self) -> bool {
        self.task.is_none()
    }

    /// patches/<upstream>/ of this repo checkout.
    pub fn stored(&self) -> Result<Vec<(String, String)>> {
        stored(&self.patches)
    }

    fn on_branch(&self) -> Result<bool> {
        Ok(git(
            Some(&self.checkout),
            &["symbolic-ref", "-q", "--short", "HEAD"],
        )
        .is_ok_and(|b| b == self.branch))
    }

    fn has_branch(&self) -> Result<bool> {
        has_branch(&self.checkout, &self.branch)
    }
}

/// Every checkout of `all` as `layout` places it, with the untracked files `declared` for it.
fn ups<'a>(
    all: &'a [Checkout],
    layout: &Layout,
    root: &Path,
    declared: &BTreeMap<PathBuf, BTreeSet<String>>,
) -> Vec<Up<'a>> {
    all.iter()
        .map(|c| {
            let mut u = Up::new(c, layout, root);
            u.untracked = declared.get(&c.dir).cloned().unwrap_or_default();
            u
        })
        .collect()
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let sources = sources::load(root)?;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    // --rebase and --edit fetch, so AOSP rows need their URLs from the manifest
    let manifest = aosp::load(root, &sources.aosp)?;
    let all = sources.checkouts(manifest.as_ref().map(Manifest::names).as_ref());
    let layout = Layout::of(root)?;
    let declared = declared(&sources, manifest.as_ref(), &prune::load(root, &sources)?);
    let ups = ups(&all, &layout, root, &declared);
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
        let main = fetch::Main::load(root)?;
        for u in pick(&args.rebase)? {
            let r = shared_allowed(u, &main).and_then(|()| rebase(u, &ident));
            report(u.name(), r);
        }
    } else if !args.edit.is_empty() {
        let ident = Ident::of_repo(root)?;
        for u in pick(&args.edit)? {
            report(u.name(), edit(u.c, &layout, root, &ident));
        }
    } else if !args.done.is_empty() {
        let picked = pick(&args.done)?;
        for u in &picked {
            report(u.name(), done(u, &layout, &licenses));
        }
        if picked.iter().any(|u| matches!(u.c.pin, Pin::Aosp(_))) {
            // the farm follows the task copies left
            farm::aosp_tree(&layout, &all, manifest.as_ref())?;
        }
    } else {
        for u in &ups {
            let shown = match drift(u, &licenses) {
                Ok(None) => Ok("no checkout".to_owned()),
                Ok(Some(p)) if p.is_empty() => Ok(match &u.task {
                    Some(_) => format!("in step (task copy {})", u.checkout.display()),
                    None => "in step".to_owned(),
                }),
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

/// A mutation of a shared checkout (rebase, fetch's settle) only for a repo checkout whose pin and patches/ are main's.
pub fn shared_allowed(u: &Up, main: &fetch::Main) -> Result<()> {
    if u.is_shared()
        && let Some(why) = main.refusal(u.c, &u.stored()?)
    {
        bail!("{why}");
    }
    Ok(())
}

/// The drift check behind `xtask check`: every present checkout matches its pin and patches/.
pub fn check(root: &Path) -> Result<()> {
    let sources = sources::load(root)?;
    let licenses = licenses::load(root, &sources.upstream_names())?;
    let all = sources.checkouts(None);
    let ups = loaded_ups(root, &sources, &all)?;
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
    let upstreams: BTreeSet<&str> = upstreams.into_iter().collect();
    let all: Vec<Checkout> = sources
        .checkouts(None)
        .into_iter()
        .filter(|c| built_from(c, &upstreams))
        .collect();
    in_step(&loaded_ups(root, &sources, &all)?, &licenses)
}

/// `ups` with the declared untracked files read from the manifest and the prune table.
fn loaded_ups<'a>(root: &Path, sources: &Sources, all: &'a [Checkout]) -> Result<Vec<Up<'a>>> {
    let manifest = aosp::load(root, &sources.aosp)?;
    let declared = declared(sources, manifest.as_ref(), &prune::load(root, sources)?);
    Ok(ups(all, &Layout::of(root)?, root, &declared))
}

/// Untracked files xtask itself puts in AOSP checkouts, by checkout dir (`aosp/<project>`), as paths inside it: the manifest's link/copy destinations (`xtask fetch`) and the prune markers (`xtask soong`). Any other untracked file is drift: a new file not committed on the patch branch.
fn declared(
    sources: &Sources,
    manifest: Option<&Manifest>,
    prune: &Prune,
) -> BTreeMap<PathBuf, BTreeSet<String>> {
    let mut out: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut add = |tree_path: &str| {
        // the longest pinned project holding it
        let owner = sources
            .aosp
            .projects
            .keys()
            .filter(|p| tree_path.starts_with(&format!("{p}/")))
            .max_by_key(|p| p.len());
        if let Some(p) = owner {
            out.entry(Path::new("aosp").join(p))
                .or_default()
                .insert(tree_path[p.len() + 1..].to_owned());
        }
    };
    for project in manifest.iter().flat_map(|m| m.projects.values()) {
        for link in &project.links {
            add(&link.dest);
        }
    }
    for dir in prune.keys() {
        add(&format!("{dir}/{}", prune::MARKER));
    }
    out
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
    let strays = strays(u)?;
    if !strays.is_empty() {
        let more = strays.len().saturating_sub(STRAYS_SHOWN);
        problems.push(format!(
            "untracked {}{}: commit on {} or delete",
            strays[..strays.len().min(STRAYS_SHOWN)].join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            },
            u.branch
        ));
    }
    if rebase_in_progress(dir)? {
        problems.push("rebase in progress".to_owned());
        return Ok(Some(problems));
    }
    let stored = stored(&u.patches)?;
    if !stored.is_empty()
        && let Some(p) = licence_problem(u)?
    {
        problems.push(p);
    }
    if u.on_branch()? {
        match base(u)? {
            None => problems.push(no_base(u)),
            Some(b) if b != u.c.commit => problems.push(format!(
                "pin moved: xtask fetch --execute (xtask patch --rebase {} if it refuses)",
                u.name()
            )),
            Some(b) => match branch_patches(u, &b, licenses) {
                Ok(branch) if branch == stored => {}
                Ok(branch) if unexported(&branch, &stored).is_empty() => problems.push(format!(
                    "patches/{} holds patches {} lacks: {FETCH}",
                    u.name(),
                    u.branch
                )),
                Ok(_) => problems.push(format!(
                    "{} differs from patches/{1}: xtask patch --export {1}",
                    u.branch,
                    u.name()
                )),
                Err(e) => problems.push(format!("{e:#}")),
            },
        }
    } else if !stored.is_empty() {
        problems.push(format!("{} patches not applied: {FETCH}", stored.len()));
    } else if git(Some(dir), &["rev-parse", "HEAD"])? != u.c.commit {
        problems.push("not at pin: xtask fetch --execute".to_owned());
    }
    Ok(Some(problems))
}

/// Untracked files of the checkout (`.gitignore`d ones excluded) that xtask did not put there.
fn strays(u: &Up) -> Result<Vec<String>> {
    let out = git(
        Some(&u.checkout),
        &["ls-files", "--others", "--exclude-standard"],
    )?;
    Ok(out
        .lines()
        .filter(|f| !u.untracked.contains(*f))
        .map(str::to_owned)
        .collect())
}

/// `xtask fetch`'s second step, once the checkout is at its pin or on its patch branch: make the branch
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
    let on = u.on_branch()?;
    if !on && stored.is_empty() {
        return Ok(None);
    }
    ensure!(
        !rebase_in_progress(dir)?,
        "rebase in progress: finish it (git rebase --continue / --abort) first"
    );
    let pin = u.c.commit.as_str();
    if u.has_branch()? {
        let b = base(u)?.with_context(|| no_base(u))?;
        let branch = branch_patches(u, &b, licenses).with_context(|| {
            format!(
                "{} holds commits patches/ cannot hold; fix them, then --export",
                u.branch
            )
        })?;
        let lost = unexported(&branch, &stored);
        if !lost.is_empty() {
            let how = if b == pin {
                format!("xtask patch --export {}", u.name())
            } else {
                format!("xtask patch --rebase {0}, then --export {0}", u.name())
            };
            bail!(
                "{0} holds {1} commits not in patches/{2} ({3}): {how}; or, if they are stale, `git -C {4} checkout --detach && git -C {4} branch -D {0}`",
                u.branch,
                lost.len(),
                u.name(),
                lost.join(", "),
                dir.display(),
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

/// patch branch := pin + `stored` (git am). On failure the checkout's branch, HEAD and base go back
/// to what they were.
fn rebuild(u: &Up, stored: &[(String, String)], ident: &Ident) -> Result<()> {
    let dir = &u.checkout;
    let pin = u.c.commit.as_str();
    let branch = u.branch.as_str();
    // fetch leaves a checkout on its patch branch where it is, so a new pin may not be here yet
    if git(Some(dir), &["cat-file", "-e", &format!("{pin}^{{commit}}")]).is_err() {
        fetch::verify_tag(u.c)?;
        fetch::fetch_pin(u.c, dir)?;
    }
    let was_on = u.on_branch()?;
    let head = git(Some(dir), &["rev-parse", "HEAD"])?;
    let branch_ref = format!("refs/heads/{branch}");
    let old_branch = git(Some(dir), &["rev-parse", "-q", "--verify", &branch_ref]).ok();
    let old_base = git(Some(dir), &["rev-parse", "-q", "--verify", BASE_REF]).ok();
    git(Some(dir), &["checkout", "-q", "-B", branch, pin])?;
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
            Some(b) => git(Some(dir), &["branch", "-q", "-f", branch, b])?,
            None => git(Some(dir), &["branch", "-q", "-D", branch])?,
        };
        if was_on {
            git(Some(dir), &["checkout", "-q", branch])?;
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

/// `--edit`: the task copy of `c`, a worktree of `src/<up>` on `atl-patches-<task>` = pin + this checkout's patches/.
/// It commits as the repo checkout's identity (`ident`): `user.*` in the copy's own worktree config (`extensions.worktreeConfig`), so it goes with the copy and `src/<up>`, where nobody commits, gets none.
fn edit(c: &Checkout, layout: &Layout, root: &Path, ident: &Ident) -> Result<String> {
    let (shared, copy, task) = layout.edit_paths(&c.dir)?;
    let branch = format!("{BRANCH}-{task}");
    ensure!(
        !copy.exists(),
        "{} exists: the task copy is there already",
        copy.display()
    );
    ensure!(
        shared.join(".git").exists(),
        "{} is not fetched: xtask fetch --execute",
        shared.display()
    );
    ensure!(
        !has_branch(&shared, &branch)?,
        "branch {branch} exists in {0} without a task copy: `git -C {0} branch -D {branch}` if it is stale",
        shared.display()
    );
    // only objects and the tag ref: the shared checkout itself does not move
    if git(
        Some(&shared),
        &["cat-file", "-e", &format!("{}^{{commit}}", c.commit)],
    )
    .is_err()
    {
        fetch::verify_tag(c)?;
        fetch::fetch_pin(c, &shared)?;
    }
    if let Some(parent) = copy.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path = copy.display().to_string();
    git(
        Some(&shared),
        &["worktree", "add", "-q", "--detach", &path, &c.commit],
    )?;
    let u = Up::new(c, layout, root);
    ensure!(
        u.checkout == copy,
        "the layout does not resolve {}",
        copy.display()
    );
    let stored = u.stored()?;
    let made = (|| {
        git(
            Some(&shared),
            &["config", "extensions.worktreeConfig", "true"],
        )?;
        for (key, value) in [("user.name", &ident.name), ("user.email", &ident.email)] {
            git(Some(&copy), &["config", "--worktree", key, value])?;
        }
        rebuild(&u, &stored, ident)
    })();
    if let Err(e) = made {
        return Err(match remove_copy(&shared, &copy, &branch) {
            Ok(()) => e.context("task copy removed again"),
            Err(r) => e.context(format!("removing the task copy failed too: {r:#}")),
        });
    }
    Ok(format!(
        "task copy {} on {branch} = pin + {} patches",
        copy.display(),
        stored.len()
    ))
}

/// `--done`: remove the task copy and its branch, unless that would lose work.
fn done(u: &Up, layout: &Layout, licenses: &Licenses) -> Result<String> {
    let (shared, copy, _) = layout.edit_paths(&u.c.dir)?;
    ensure!(
        !u.is_shared(),
        "no task copy {}: nothing to remove",
        copy.display()
    );
    ensure!(
        !is_dirty(&u.checkout)?,
        "uncommitted changes in {}",
        copy.display()
    );
    let strays = strays(u)?;
    ensure!(
        strays.is_empty(),
        "untracked {} in {}",
        strays.join(", "),
        copy.display()
    );
    if u.has_branch()? {
        let b = base(u)?.with_context(|| no_base(u))?;
        let lost = unexported(&branch_patches(u, &b, licenses)?, &u.stored()?);
        ensure!(
            lost.is_empty(),
            "{} holds {} commits not in patches/{} ({}): xtask patch --export {}",
            u.branch,
            lost.len(),
            u.name(),
            lost.join(", "),
            u.name()
        );
    }
    remove_copy(&shared, &copy, &u.branch)?;
    layout.remove_empty_parents(&copy);
    Ok(format!("task copy removed; src/{} in use again", u.name()))
}

fn remove_copy(shared: &Path, copy: &Path, branch: &str) -> Result<()> {
    // --force: the caller checked there is nothing to lose (or made the copy a moment ago)
    git(
        Some(shared),
        &["worktree", "remove", "--force", &copy.display().to_string()],
    )?;
    if has_branch(shared, branch)? {
        git(Some(shared), &["branch", "-q", "-D", branch])?;
    }
    Ok(())
}

fn export(u: &Up, licenses: &Licenses) -> Result<String> {
    let dir = &u.checkout;
    ensure!(
        dir.exists() && u.on_branch()?,
        "{} not on {}: nothing to export",
        dir.display(),
        u.branch
    );
    ensure!(
        base(u)?.as_deref() == Some(u.c.commit.as_str()),
        "{} is not based on the pin: xtask patch --rebase {}",
        u.branch,
        u.name()
    );
    let patches = branch_patches(u, &u.c.commit, licenses)?;
    ensure!(
        !patches.is_empty(),
        "{} has no commits past the pin: nothing to export (to drop every patch, delete patches/{}/)",
        u.branch,
        u.name()
    );
    let (licence_file, licence) = upstream_licence(u)?;
    std::fs::create_dir_all(&u.patches)?;
    for (name, _) in stored(&u.patches)? {
        std::fs::remove_file(u.patches.join(name))?;
    }
    for (name, text) in &patches {
        std::fs::write(u.patches.join(name), text)?;
    }
    std::fs::write(u.patches.join(LICENCE_COPY), licence)?;
    Ok(format!(
        "exported {} patches, {LICENCE_COPY} = {licence_file} at the pin",
        patches.len()
    ))
}

/// The upstream's licence text: the first of `LICENCE_FILES` that is a regular file at the pin (a symlink's blob is its target path, a tree no text), with its name.
fn upstream_licence(u: &Up) -> Result<(&'static str, String)> {
    for name in LICENCE_FILES {
        // `<mode> <type> <blob>\t<name>`, empty when absent
        let entry = git(Some(&u.checkout), &["ls-tree", &u.c.commit, "--", name])?;
        let mut f = entry.split_whitespace();
        if let (Some(mode), Some(_), Some(blob)) = (f.next(), f.next(), f.next())
            && mode.starts_with("100")
        {
            return Ok((name, git_raw(&u.checkout, &["cat-file", "blob", blob])?));
        }
    }
    bail!(
        "{} has none of {} at the pin: no licence text for patches/{}/{LICENCE_COPY}",
        u.name(),
        LICENCE_FILES.join(", "),
        u.name()
    )
}

/// What keeps `patches/<upstream>/LICENSE` from being the upstream's licence text at the pin.
fn licence_problem(u: &Up) -> Result<Option<String>> {
    let (name, want) = upstream_licence(u)?;
    let path = u.patches.join(LICENCE_COPY);
    let fix = format!("xtask patch --export {}", u.name());
    Ok(match std::fs::read_to_string(&path) {
        Err(_) => Some(format!(
            "patches/{}/{LICENCE_COPY} missing: {fix}",
            u.name()
        )),
        Ok(have) if have != want => Some(format!(
            "patches/{}/{LICENCE_COPY} is not {name} at the pin: {fix}",
            u.name()
        )),
        Ok(_) => None,
    })
}

fn rebase(u: &Up, ident: &Ident) -> Result<String> {
    let dir = &u.checkout;
    let pin = u.c.commit.as_str();
    ensure!(dir.exists() && u.on_branch()?, "not on {}", u.branch);
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
    let resumed = git(Some(dir), &["merge-base", "--is-ancestor", pin, &u.branch]).is_ok();
    if !resumed {
        git_isolated(
            dir,
            &["rebase", "-q", "--onto", pin, &old, &u.branch],
            Some(ident),
        )
        .with_context(|| {
            format!(
                "conflicts: resolve in {}, `git rebase --continue`, then rerun xtask patch --rebase {}",
                dir.display(),
                u.name()
            )
        })?;
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

/// `<base>..<patch branch>` as patch files, byte-for-byte reproducible: global git config ignored,
/// commit ids zeroed, full blob ids, no signature (`--no-stat` would drop the `---` line); each gets the SPDX expression of the files
/// it touches after its `---` line, where `git am` ignores it.
fn branch_patches(u: &Up, base: &str, licenses: &Licenses) -> Result<Vec<(String, String)>> {
    let dir = &u.checkout;
    let range = format!("{base}..{}", u.branch);
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

/// Tracked files changed. Untracked ones are `strays`' concern: fetch may rebuild a branch beside them.
fn is_dirty(dir: &Path) -> Result<bool> {
    Ok(!git(
        Some(dir),
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty())
}

fn has_branch(dir: &Path, branch: &str) -> Result<bool> {
    Ok(git(
        Some(dir),
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok())
}

/// What the patch branch is based on: `BASE_REF`, else (a branch started by hand) the newest tag it contains -- a shallow checkout holds only the pinned tags, so that is the pin it was started at, even after a bump.
fn base(u: &Up) -> Result<Option<String>> {
    let dir = &u.checkout;
    if let Ok(b) = git(Some(dir), &["rev-parse", "-q", "--verify", BASE_REF]) {
        return Ok(Some(b));
    }
    let Ok(tag) = git(Some(dir), &["describe", "--tags", "--abbrev=0", &u.branch]) else {
        return Ok(None);
    };
    Ok(Some(git(
        Some(dir),
        &["rev-parse", &format!("refs/tags/{tag}^{{commit}}")],
    )?))
}

fn no_base(u: &Up) -> String {
    format!(
        "{0} has no {BASE_REF} and contains no pinned tag: rename it (`git -C {1} branch -m {0} <name>`), then start {0} at the pin {2}",
        u.branch,
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

    /// The fixture upstream's licence text.
    pub const LICENCE: &str = "the up licence\n";

    /// Temp tree `<root>`: `upstream/` (a repo with a.c and LICENSE, tag v1); workspaces `<root>/<ws>` hold
    /// `src/up` and `src-wt/<task>/up`, patches live under `<root>/patches/up`.
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
            f.write("LICENSE", LICENCE);
            f.release("one", "v1");
            f
        }

        /// Delete a file of the upstream (committed by the next `release`).
        pub fn remove(&self, file: &str) {
            std::fs::remove_file(self.upstream.join(file)).unwrap();
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

        /// Workspace `<root>/<ws>` as a repo checkout on `branch` sees it (None = main).
        pub fn layout(&self, ws: &str, branch: Option<&str>) -> Layout {
            Layout::new(&self.root.join(ws), branch)
        }

        /// `c` in workspace `<root>/<ws>` on main, whether checked out or not.
        pub fn at<'a>(&self, c: &'a Checkout, ws: &str) -> Up<'a> {
            Up::new(c, &self.layout(ws, None), &self.root)
        }

        /// `c` as a repo checkout on `branch` resolves it in workspace `<root>/<ws>`.
        pub fn on<'a>(&self, c: &'a Checkout, ws: &str, branch: &str) -> Up<'a> {
            Up::new(c, &self.layout(ws, Some(branch)), &self.root)
        }

        /// A checkout of `c` in `<root>/<ws>/src`, detached at the pin.
        pub fn fetched<'a>(&self, c: &'a Checkout, ws: &str) -> Up<'a> {
            let u = self.at(c, ws);
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

    /// Start the patch branch at the pin (as a developer does by hand) unless on it, and commit a line
    /// added to a.c with `msg`.
    pub fn patch_commit(u: &Up, msg: &str) {
        let d = &u.checkout;
        if !u.on_branch().unwrap() {
            git(Some(d), &["switch", "-q", "-c", &u.branch, &u.c.commit]).unwrap();
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
        u.on_branch().unwrap()
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
        let u = f.fetched(&c, "ws");
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
        let u = f.fetched(&c, "ws");
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
        let u1 = f.fetched(&c1, "ws");
        patch_commit(&u1, TRAILED);
        // upstream moves on: a new file and tag v2
        f.write("b.c", "int b;\n");
        f.release("two", "v2");
        let c2 = f.checkout("v2");
        let u2 = f.at(&c2, "ws");

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
        let mut u = f.fetched(&c, "ws");
        let check =
            |u: &Up| in_step(std::slice::from_ref(u), &licenses()).map_err(|e| format!("{e:#}"));
        // detached at the pin, no patches: in step
        check(&u).unwrap();
        // an uncommitted edit
        std::fs::write(u.checkout.join("a.c"), "int edited;\n").unwrap();
        let e = check(&u).unwrap_err();
        assert!(e.contains("up: uncommitted changes"), "{e}");
        git(Some(&u.checkout), &["checkout", "-q", "--", "a.c"]).unwrap();
        // a new file never committed; one xtask declares is no stray, nor is an ignored one
        for file in ["new.c", "link.h", "build.o"] {
            std::fs::write(u.checkout.join(file), "").unwrap();
        }
        std::fs::write(u.checkout.join(".git/info/exclude"), "*.o\n").unwrap();
        u.untracked.insert("link.h".into());
        let e = check(&u).unwrap_err();
        assert!(
            e.contains("up: untracked new.c: commit on atl-patches or delete"),
            "{e}"
        );
        std::fs::remove_file(u.checkout.join("new.c")).unwrap();
        check(&u).unwrap();
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
            e.contains("patches/up holds patches atl-patches lacks: cargo xtask fetch --execute (the step after merging"),
            "{e}"
        );
        // off the patch branch with patches/ non-empty: the same fix
        git(
            Some(&u.checkout),
            &["checkout", "-q", "--detach", &c.commit],
        )
        .unwrap();
        let e = check(&u).unwrap_err();
        assert!(
            e.contains("1 patches not applied: cargo xtask fetch --execute"),
            "{e}"
        );
    }

    /// `patches/<up>/LICENSE` is derived by export from the upstream at the pin and checked by the drift test.
    #[test]
    fn licence_copy_is_the_upstream_text() {
        let f = Fixture::new("licence");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "ws");
        patch_commit(&u, TRAILED);
        let out = export_ok(&u);
        assert!(out.contains("LICENSE = LICENSE at the pin"), "{out}");
        let copy = u.patches.join(LICENCE_COPY);
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), LICENCE);
        assert_eq!(drift_of(&u), Vec::<String>::new());
        // a hand edit, then a deletion: drift, fixed by export
        std::fs::write(&copy, "edited\n").unwrap();
        assert_eq!(
            drift_of(&u),
            ["patches/up/LICENSE is not LICENSE at the pin: xtask patch --export up"]
        );
        std::fs::remove_file(&copy).unwrap();
        assert_eq!(
            drift_of(&u),
            ["patches/up/LICENSE missing: xtask patch --export up"]
        );

        // an AOSP-style upstream: NOTICE only (a symlinked LICENSE is no licence text)
        f.remove("LICENSE");
        f.write("NOTICE", "the notice\n");
        std::os::unix::fs::symlink("NOTICE", f.root.join("upstream/LICENSE")).unwrap();
        f.release("two", "v2");
        let c2 = f.checkout("v2");
        let u2 = f.fetched(&c2, "ws2");
        patch_commit(&u2, TRAILED);
        std::fs::remove_dir_all(&u2.patches).unwrap();
        let out = export_ok(&u2);
        assert!(out.contains("LICENSE = NOTICE at the pin"), "{out}");
        assert_eq!(
            std::fs::read_to_string(u2.patches.join(LICENCE_COPY)).unwrap(),
            "the notice\n"
        );

        // no licence text at all: export refuses and writes nothing
        f.remove("NOTICE");
        f.remove("LICENSE");
        f.release("three", "v3");
        let c3 = f.checkout("v3");
        let u3 = f.fetched(&c3, "ws3");
        patch_commit(&u3, TRAILED);
        std::fs::remove_dir_all(&u3.patches).unwrap();
        let e = format!("{:#}", export(&u3, &licenses()).unwrap_err());
        assert!(
            e.contains("up has none of LICENSE, COPYING, NOTICE at the pin"),
            "{e}"
        );
        assert!(!u3.patches.exists());
    }

    #[test]
    fn export_without_commits_creates_nothing() {
        let f = Fixture::new("empty");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "ws");
        // on the patch branch, at the pin, nothing committed
        git(Some(&u.checkout), &["switch", "-q", "-c", BRANCH]).unwrap();
        let e = format!("{:#}", export(&u, &licenses()).unwrap_err());
        assert!(
            e.contains("atl-patches has no commits past the pin: nothing to export"),
            "{e}"
        );
        assert!(!u.patches.exists());
    }

    #[test]
    fn declared_untracked_files() {
        let sources: Sources = toml::from_str(
            "[aosp]\nremote = \"https://h/\"\ntag = \"t\"\nmanifest = \"m\"\n\
             [aosp.projects]\n\"build\" = \"c\"\n\"build/make\" = \"c\"\n",
        )
        .unwrap();
        let m = crate::aosp::parse(r#"<manifest><default revision="r"/><project path="build/make" name="m"><linkfile src="core" dest="build/core"/><linkfile src="x" dest="Makefile"/></project></manifest>"#).unwrap();
        let prune: Prune =
            toml::from_str("[\"build/make/target/generic\"]\nreason = \"r\"\n").unwrap();
        let d = declared(&sources, Some(&m), &prune);
        // each file in the longest project holding it; one outside every project is nobody's
        assert_eq!(
            d,
            BTreeMap::from([
                (
                    PathBuf::from("aosp/build"),
                    BTreeSet::from(["core".to_owned()])
                ),
                (
                    PathBuf::from("aosp/build/make"),
                    BTreeSet::from(["target/generic/.find-ignore".to_owned()])
                ),
            ])
        );
    }

    #[test]
    fn branch_without_base_is_refused() {
        let f = Fixture::new("nobase");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "ws");
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

    /// `--edit` makes the task copy every command then resolves; `--done` removes it only when nothing is lost.
    #[test]
    fn task_copy_cycle() {
        let f = Fixture::new("edit");
        let c = f.checkout("v1");
        let shared = f.fetched(&c, "ws");
        // main's patches, applied in src/ as fetch does
        patch_commit(&shared, TRAILED);
        export_ok(&shared);
        let layout = f.layout("ws", Some("t"));
        let err = |r: Result<String>| format!("{:#}", r.unwrap_err());

        // refused on main and with a stale branch of the task's name
        let e = err(edit(&c, &f.layout("ws", None), &f.root, &ident()));
        assert!(e.contains("task copies belong to a task branch"), "{e}");
        git(Some(&shared.checkout), &["branch", "atl-patches-t"]).unwrap();
        let e = err(edit(&c, &layout, &f.root, &ident()));
        assert!(e.contains("branch atl-patches-t exists"), "{e}");
        git(Some(&shared.checkout), &["branch", "-D", "atl-patches-t"]).unwrap();

        edit(&c, &layout, &f.root, &ident()).unwrap();
        let task = f.on(&c, "ws", "t");
        assert_eq!(task.checkout, f.root.join("ws/src-wt/t/up"));
        assert_eq!(task.branch, "atl-patches-t");
        // pin + this checkout's patches, its own base ref; src/ untouched
        assert_eq!(tree(&task), tree(&shared));
        assert_eq!(base_ref(&task).as_deref(), Some(c.commit.as_str()));
        assert_eq!(drift_of(&task), Vec::<String>::new());
        assert!(on_patch_branch(&shared));
        let e = err(edit(&c, &layout, &f.root, &ident()));
        assert!(e.contains("the task copy is there already"), "{e}");
        // a plain `git commit` in the copy works with no global config: the repo checkout's identity is the copy's own; src/ gets none
        git_isolated(
            &task.checkout,
            &["commit", "-q", "--allow-empty", "-m", "who"],
            None,
        )
        .unwrap();
        assert_eq!(
            git(Some(&task.checkout), &["log", "-1", "--format=%an <%ae>"]).unwrap(),
            "t <t@t>"
        );
        git(Some(&task.checkout), &["reset", "-q", "--hard", "HEAD~1"]).unwrap();
        assert!(git_isolated(&shared.checkout, &["config", "user.name"], None).is_err());

        // work in the copy: --done refuses each kind of unsaved work
        std::fs::write(task.checkout.join("a.c"), "int dirty;\n").unwrap();
        let e = err(done(&task, &layout, &licenses()));
        assert!(e.contains("uncommitted changes"), "{e}");
        git(Some(&task.checkout), &["checkout", "-q", "--", "a.c"]).unwrap();
        std::fs::write(task.checkout.join("new.c"), "").unwrap();
        let e = err(done(&task, &layout, &licenses()));
        assert!(e.contains("untracked new.c"), "{e}");
        std::fs::remove_file(task.checkout.join("new.c")).unwrap();
        patch_commit(
            &task,
            "fix b\n\nUpstream-Status: Pending\nPatch-Reason: test\n",
        );
        let e = err(done(&task, &layout, &licenses()));
        assert!(
            e.contains("atl-patches-t holds 1 commits not in patches/up"),
            "{e}"
        );
        // exported from the task copy, src/ still main's
        export_ok(&task);
        assert_eq!(drift_of(&task), Vec::<String>::new());
        assert!(!drift_of(&shared).is_empty());
        done(&task, &layout, &licenses()).unwrap();
        assert!(!f.root.join("ws/src-wt/t").exists());
        assert!(!has_branch(&shared.checkout, "atl-patches-t").unwrap());
        assert!(f.on(&c, "ws", "t").is_shared());
        let e = err(done(&f.on(&c, "ws", "t"), &layout, &licenses()));
        assert!(e.contains("nothing to remove"), "{e}");

        // --edit again: the task's exported patches come back
        edit(&c, &layout, &f.root, &ident()).unwrap();
        let again = f.on(&c, "ws", "t");
        assert_eq!(drift_of(&again), Vec::<String>::new());
        // a failed git am leaves no copy behind
        done(&again, &layout, &licenses()).unwrap();
        std::fs::write(f.root.join("patches/up/0003-bad.patch"), "not a patch\n").unwrap();
        let e = err(edit(&c, &layout, &f.root, &ident()));
        assert!(e.contains("task copy removed again"), "{e}");
        assert!(!f.root.join("ws/src-wt/t/up").exists());
        assert!(!has_branch(&shared.checkout, "atl-patches-t").unwrap());
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
