// SPDX-License-Identifier: MIT
//! `xtask land <branch>`: puts a task branch on origin/main, with the branch's own patches checked before the push.
//! Why this order: `src/<up>` is one checkout shared by every worktree and holds origin/main's pins and patches (`fetch::Main`), so a branch's unlanded patches exist only in its task copies (`src-wt/<branch>/<up>`), which only the branch's worktree resolves (`atl-layout`).
//! Main has no task copies, so `check` run on main sees `src/` alone. That is why the branch is checked in its own worktree before the push, and why its copies go only after main and `src/` reached the new commit.
//! Nothing reaches origin before the push. A task copy is a worktree of `src/` on `atl-patches-<branch>`: it leaves every other worktree's checkout alone.
//! Cleanup runs in a child `cargo xtask land <branch> --cleanup --execute` in main: this process's binary predates the sync, so it cannot parse the tables the sync brought in.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::{Layout, MAIN};

use crate::check;
use crate::fetch;
use crate::git::{Ident, git, git_raw};
use crate::patch;
use crate::slot::{self, SLOTS};
use crate::slot_prune;
use crate::sources::{self, Sources};
use crate::verdict::{self, Failure, Verdict};

/// The note a task keeps in its worktree: untracked by design, copied to `out/` when the branch lands.
const STATE_NOTE: &str = ".state-note.md";

#[derive(clap::Args)]
pub struct Args {
    /// Task branch to land: checked out in a worktree, and a fast-forward of origin/main.
    branch: String,
    /// Act: check, push, bring the main checkout and `src/` to the new commit, check again, clean up. Without it, prints the steps only.
    #[arg(long)]
    execute: bool,
    /// Internal: the cleanup stage's child (`cleanup_execute`); runs only the cleanup. Needs `--execute`.
    #[arg(long, hide = true, requires = "execute")]
    cleanup: bool,
}

/// What `land` runs beside its git work: the check of a checkout, the fetch of the main checkout and the cleanup child. Tests substitute all three.
struct Runner<'a> {
    /// The checkout's verdict line, Ok only when that check passed (`check_verdict`).
    check: &'a dyn Fn(&Path) -> Result<String>,
    /// `xtask fetch --execute` in the main checkout.
    fetch: &'a dyn Fn(&Path) -> Result<()>,
    /// The cleanup child for the main checkout and the branch; its note line (`cleanup_execute`).
    cleanup: &'a dyn Fn(&Path, &str) -> Result<String>,
}

/// One step of a landing, in run order.
#[derive(Debug, PartialEq, Eq)]
enum Stage {
    /// A task copy for an upstream whose patches/ or pin differ from origin/main's.
    Edit(String),
    /// `cargo xtask check` in the branch's worktree.
    Verify,
    Push,
    /// Main fast-forwards to origin/main, then `fetch --execute` moves `src/` there.
    Sync,
    /// `cargo xtask check` in the main checkout.
    VerifyMain,
    /// Task copies, the state note (kept in out/), the worktree and the branch, in a child (`cleanup_execute`): this process's binary predates the sync.
    Cleanup,
}

impl Stage {
    fn name(&self) -> &'static str {
        match self {
            Stage::Edit(_) => "edit",
            Stage::Verify => "verify",
            Stage::Push => "push",
            Stage::Sync => "sync",
            Stage::VerifyMain => "verify-main",
            Stage::Cleanup => "cleanup",
        }
    }
}

/// The preflight's facts: what the stages need, read without writing.
struct Plan {
    repo: PathBuf,
    branch: String,
    wt: PathBuf,
    /// origin/main before the push.
    origin: String,
    /// The branch's tip: origin/main after the push.
    head: String,
    /// Upstreams needing a task copy in the worktree.
    edits: Vec<String>,
    /// Upstreams whose task copy exists already.
    copies: Vec<String>,
}

/// `xtask land <branch>`: prints the steps unless `--execute`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.cleanup {
        println!("{}", clean_up_child(root, &args.branch)?);
        return Ok(ExitCode::SUCCESS);
    }
    let plan = Plan::new(root, &args.branch, args.execute)?;
    let stages = plan.stages();
    if !args.execute {
        for line in plan.describe(&stages) {
            println!("{line}");
        }
        println!(
            "land: dry run, nothing changed ({} steps; `--execute` acts)",
            stages.len()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let runner = Runner {
        check: &check_verdict,
        fetch: &fetch_execute,
        cleanup: &cleanup_execute,
    };
    let line = plan.execute(&stages, &runner);
    println!("{line}");
    Ok(if line.starts_with("land: ok") {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

impl Plan {
    /// The read-only preflight: every refusal is an error and nothing is written. `update` first fetches origin's refs (`--execute`); a dry run judges origin/main as last fetched.
    fn new(repo: &Path, branch: &str, update: bool) -> Result<Self> {
        ensure!(branch != MAIN, "`{MAIN}` is the trunk: land a task branch");
        ensure!(
            !branch.contains('/'),
            "branch {branch} holds `/`: task copies live in src-wt/<branch>/: rename the branch"
        );
        let here = git(Some(repo), &["symbolic-ref", "-q", "--short", "HEAD"])
            .unwrap_or_else(|_| "a detached HEAD".to_owned());
        ensure!(
            here == MAIN,
            "run from the main checkout, on `{MAIN}`: {} is on {here}",
            repo.display()
        );
        let changes = git_raw(repo, &["status", "--porcelain"])?;
        ensure!(
            changes.trim().is_empty(),
            "main checkout {} has changes:\n{}",
            repo.display(),
            changes.trim_end()
        );
        if update {
            git(Some(repo), &["fetch", "-q", "origin"]).context("fetching origin")?;
        }
        let origin = git(
            Some(repo),
            &["rev-parse", "--verify", "-q", fetch::MAIN_REF],
        )
        .with_context(|| format!("{} does not resolve: fetch origin first", fetch::MAIN_REF))?;
        let head = git(
            Some(repo),
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{branch}^{{commit}}"),
            ],
        )
        .with_context(|| format!("no branch {branch}"))?;
        ensure!(
            is_ancestor(repo, &origin, &head)?,
            "branch {branch} is not a fast-forward of {} ({}): rebase it onto that first",
            fetch::MAIN_REF,
            short(&origin)
        );
        let main_head = git(Some(repo), &["rev-parse", "HEAD"])?;
        ensure!(
            main_head == origin,
            "main is at {}, {} at {}: `git merge --ff-only origin/main` if main is behind; push or drop its own commits if ahead",
            short(&main_head),
            fetch::MAIN_REF,
            short(&origin)
        );
        let worktrees = git_raw(repo, &["worktree", "list", "--porcelain"])?;
        let wt = find_worktree(&worktrees, branch).with_context(|| {
            format!("branch {branch} is not checked out in a worktree: `git worktree add <ws>/wt/{branch} {branch}`")
        })?;
        let status = git_raw(&wt, &["status", "--porcelain"])?;
        let dirty = dirty_lines(&status);
        ensure!(
            dirty.is_empty(),
            "worktree {} has uncommitted work: {}",
            wt.display(),
            first(&dirty)
        );
        patch::check(repo).context("src/ is not origin/main's pin + patches: `cargo xtask fetch --execute` in the main checkout, then land again")?;

        let layout = Layout::of(repo)?;
        let wt_layout = Layout::of(&wt)?;
        let sources = branch_sources(repo, &wt, branch)?;
        let main = fetch::Main::load(repo)?;
        let manifest = sources.manifest_checkout();
        let manifest_stored = patch::stored(&wt.join("patches").join(&manifest.dir))?;
        if let Some(why) = main.refusal(&manifest, &manifest_stored) {
            bail!("{why}");
        }
        let mut edits = Vec::new();
        for c in sources.checkouts(None) {
            if !main.lists(&c) {
                // nothing on main reads a new upstream, so the shared checkout serves it; it has to be there to be checked
                ensure!(
                    layout.src().join(&c.dir).exists(),
                    "upstream {} is new to origin/main and not fetched: `cargo xtask fetch --execute` in {} creates it, then land again",
                    c.dir.display(),
                    wt.display()
                );
                continue;
            }
            let stored = patch::stored(&wt.join("patches").join(&c.dir))?;
            if main.refusal(&c, &stored).is_some() && wt_layout.locate(&c.dir).task.is_none() {
                edits.push(c.dir.display().to_string());
            }
        }
        Ok(Plan {
            repo: repo.to_path_buf(),
            branch: branch.to_owned(),
            wt,
            origin,
            head,
            edits,
            copies: copies_of(&wt_layout, &sources),
        })
    }

    /// The stages in run order; the push is absent when origin/main already is the branch.
    fn stages(&self) -> Vec<Stage> {
        let mut stages: Vec<Stage> = self.edits.iter().cloned().map(Stage::Edit).collect();
        stages.push(Stage::Verify);
        if self.head != self.origin {
            stages.push(Stage::Push);
        }
        stages.extend([Stage::Sync, Stage::VerifyMain, Stage::Cleanup]);
        stages
    }

    fn describe(&self, stages: &[Stage]) -> Vec<String> {
        stages
            .iter()
            .enumerate()
            .map(|(i, stage)| format!("{}. {}", i + 1, self.describe_stage(stage)))
            .collect()
    }

    fn describe_stage(&self, stage: &Stage) -> String {
        let (branch, wt, repo) = (&self.branch, self.wt.display(), self.repo.display());
        match stage {
            Stage::Edit(up) => format!(
                "edit {up}: task copy src-wt/{branch}/{up} on atl-patches-{branch} (its patches/ or pin differ from {})",
                fetch::MAIN_REF
            ),
            Stage::Verify => format!(
                "verify in {wt}: `cargo xtask check` must print `check: ok` (the task copies hold the branch's patches)"
            ),
            Stage::Push => format!(
                "push: {} {} -> {} (git push origin refs/heads/{branch}:refs/heads/main)",
                fetch::MAIN_REF,
                short(&self.origin),
                short(&self.head)
            ),
            Stage::Sync => format!(
                "sync in {repo}: merge --ff-only origin/main, then `cargo xtask fetch --execute` (src/ moves to {})",
                short(&self.head)
            ),
            Stage::VerifyMain => {
                format!("verify in {repo}: `cargo xtask check` must print `check: ok`")
            }
            Stage::Cleanup => {
                let gone: Vec<&str> = self
                    .copies
                    .iter()
                    .chain(&self.edits)
                    .map(String::as_str)
                    .collect();
                let gone = if gone.is_empty() {
                    "none".to_owned()
                } else {
                    gone.join(", ")
                };
                format!(
                    "cleanup: task copies {gone}, worktree {wt} (state note kept in out/), branch {branch}"
                )
            }
        }
    }

    /// Runs the stages in order and returns the verdict line. The first failure stops the run: before the push nothing reached origin, after it the message says what did.
    fn execute(&self, stages: &[Stage], runner: &Runner) -> String {
        let total = stages.len();
        for stage in stages {
            match self.run_stage(stage, runner) {
                Ok(note) => println!("land: {} ok: {note}", stage.name()),
                Err(e) => {
                    println!("land: {} FAILED: {e:#}", stage.name());
                    return Verdict {
                        cmd: "land",
                        total: Some((total, "steps")),
                        skipped: Vec::new(),
                        failures: vec![Failure::step(stage.name(), format!("{e:#}"))],
                    }
                    .line();
                }
            }
        }
        Verdict {
            cmd: "land",
            total: Some((total, "steps")),
            skipped: Vec::new(),
            failures: Vec::new(),
        }
        .line()
    }

    fn run_stage(&self, stage: &Stage, runner: &Runner) -> Result<String> {
        match stage {
            Stage::Edit(up) => self.edit(up),
            Stage::Verify => (runner.check)(self.wt.as_path())
                .with_context(|| format!("nothing pushed: check in {} failed", self.wt.display())),
            Stage::Push => {
                let refspec = format!("refs/heads/{}:refs/heads/main", self.branch);
                git(Some(&self.repo), &["push", "-q", "origin", &refspec])
                    .context("nothing pushed: git push refused")?;
                Ok(format!("origin/main is {}", short(&self.head)))
            }
            Stage::Sync => self.sync(runner),
            Stage::VerifyMain => (runner.check)(self.repo.as_path()).with_context(|| {
                format!(
                    "pushed: origin/main is {}, and the check in {} failed",
                    short(&self.head),
                    self.repo.display()
                )
            }),
            Stage::Cleanup => (runner.cleanup)(&self.repo, &self.branch)
                .context("pushed and checked, but cleanup failed: finish it by hand"),
        }
    }

    fn edit(&self, up: &str) -> Result<String> {
        let sources = sources::load(&self.wt)?;
        let c = sources
            .checkouts(None)
            .into_iter()
            .find(|c| c.dir.display().to_string() == up)
            .with_context(|| format!("no upstream {up} in the branch's {}", sources::FILE))?;
        patch::edit(
            &c,
            &Layout::of(&self.wt)?,
            &self.wt,
            &Ident::of_repo(&self.wt)?,
        )
    }

    fn sync(&self, runner: &Runner) -> Result<String> {
        let pushed = format!("pushed: origin/main is {}", short(&self.head));
        git(Some(&self.repo), &["fetch", "-q", "origin"])
            .with_context(|| format!("{pushed}; fetching origin failed"))?;
        git(
            Some(&self.repo),
            &["merge", "--ff-only", "-q", fetch::MAIN_REF],
        )
        .with_context(|| format!("{pushed}; main did not fast-forward"))?;
        (runner.fetch)(self.repo.as_path()).with_context(|| {
            format!(
                "{pushed}; src/ did not move: `cargo xtask fetch --execute` in {}",
                self.repo.display()
            )
        })?;
        Ok(format!("main and src/ at {}", short(&self.head)))
    }
}

/// `cargo xtask check` in `dir`, run by that checkout's own xtask: its verdict line, or an error carrying it when the check did not pass.
fn check_verdict(dir: &Path) -> Result<String> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "check"]).current_dir(dir);
    let (status, stdout, _) = check::run_teed(cmd)
        .with_context(|| format!("running cargo xtask check in {}", dir.display()))?;
    let line = verdict::last_line(&stdout, "check")
        .unwrap_or("check: no verdict line")
        .to_owned();
    ensure!(status.success() && passed(&line), "{line} ({status})");
    Ok(line)
}

/// `cargo xtask fetch --execute` in `dir`, run by that checkout's own xtask: after the fast-forward, the landed code moves `src/`.
fn fetch_execute(dir: &Path) -> Result<()> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "fetch", "--execute"]).current_dir(dir);
    let (status, _, _) = check::run_teed(cmd)
        .with_context(|| format!("running cargo xtask fetch in {}", dir.display()))?;
    ensure!(
        status.success(),
        "cargo xtask fetch --execute exited with {status}"
    );
    Ok(())
}

/// The cleanup child: `cargo xtask land <branch> --cleanup --execute` in the main checkout, run by that checkout's own xtask.
/// `cargo run` rebuilds it from the source the sync moved to; the pre-sync binary cannot parse the tables the sync brought (`licenses.toml` `[test_only]`). Returns the note line it printed.
fn cleanup_execute(repo: &Path, branch: &str) -> Result<String> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "land", branch, "--cleanup", "--execute"])
        .current_dir(repo);
    let (status, stdout, stderr) = check::run_teed(cmd)
        .with_context(|| format!("running cargo xtask land --cleanup in {}", repo.display()))?;
    ensure!(
        status.success(),
        "{status}: {}",
        stderr
            .rfind("Error:")
            .map_or(stderr.trim_end(), |at| stderr[at..].trim_end())
    );
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
        .context("the cleanup child printed no note")
}

/// The cleanup child's body. The parent names only the branch, so the worktree is looked up again here.
fn clean_up_child(repo: &Path, branch: &str) -> Result<String> {
    let porcelain = git_raw(repo, &["worktree", "list", "--porcelain"])?;
    let wt = find_worktree(&porcelain, branch)
        .with_context(|| format!("branch {branch} is not checked out in a worktree"))?;
    let ws = Layout::of(repo)?.workspace().to_path_buf();
    let note = clean_up(repo, &ws, &wt, branch)?;
    prune_slots_after_land(repo, &ws);
    Ok(note)
}

/// After a land: `slot prune --all --execute`. A busy slot is skipped; a failure is logged, never fatal, since the land is already pushed.
fn prune_slots_after_land(repo: &Path, ws: &Path) {
    let slots: Vec<u32> = (1..=SLOTS).collect();
    let swept = slot::holders_of(repo).and_then(|holders| {
        slot_prune::sweep(ws, &holders, &slots, slot_prune::PRUNE_AFTER_DAYS, true)
    });
    match swept {
        Ok(sweep) if sweep.failed == 0 => {}
        Ok(sweep) => eprintln!(
            "slot prune after land: {} slot(s) failed (rows above)",
            sweep.failed
        ),
        Err(e) => eprintln!("slot prune after land failed: {e:#}"),
    }
}

/// The task copies, then the state note (kept in out/), the checkout's target (`slot::leave`), the worktree and the branch. Copies go through `patch --done --task`, the command's own path.
fn clean_up(repo: &Path, ws: &Path, wt: &Path, branch: &str) -> Result<String> {
    let copies = copies_of(&Layout::new(ws, Some(branch)), &sources::load(wt)?);
    if !copies.is_empty() {
        let failed = patch::done_task(repo, branch, copies.clone())?;
        ensure!(failed == 0, "{failed} task copies not removed (rows above)");
    }
    let note = wt.join(STATE_NOTE);
    if note.is_file() {
        let out = ws.join("out");
        std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
        let kept = out.join(format!("{branch}-state-note.md"));
        std::fs::copy(&note, &kept).with_context(|| format!("keeping {}", note.display()))?;
    }
    let target = slot::leave(wt, ws, slot::record_slot(wt)?)?;
    let wt_arg = wt.display().to_string();
    git(Some(repo), &["worktree", "remove", "--force", &wt_arg])?;
    git(Some(repo), &["branch", "-d", branch])?;
    Ok(format!(
        "{} task copies, {target}, worktree {wt_arg}, branch {branch}",
        copies.len()
    ))
}

/// A verdict line that passed: `check: ok`, optionally followed by its counts.
fn passed(line: &str) -> bool {
    line == "check: ok" || line.starts_with("check: ok (")
}

/// Whether `ancestor` is an ancestor of `descendant`; git's exit 0 and 1 are the answers, anything else an error.
fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .context("running git merge-base")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("git merge-base --is-ancestor {ancestor} {descendant}: {status}"),
    }
}

/// The worktree with `branch` checked out, from `git worktree list --porcelain` (blocks of `worktree <path>` and `branch refs/heads/<name>` lines).
fn find_worktree(porcelain: &str, branch: &str) -> Option<PathBuf> {
    let want = format!("branch refs/heads/{branch}");
    porcelain.split("\n\n").find_map(|block| {
        let path = block.lines().find_map(|l| l.strip_prefix("worktree "))?;
        block
            .lines()
            .any(|l| l == want)
            .then(|| PathBuf::from(path))
    })
}

/// Uncommitted work in `git status --porcelain` lines; the state note is untracked by design and is not work.
fn dirty_lines(porcelain: &str) -> Vec<&str> {
    let note = format!("?? {STATE_NOTE}");
    porcelain
        .lines()
        .filter(|l| !l.is_empty() && *l != note)
        .collect()
}

/// The checkout dirs whose task copy `layout`'s task holds.
fn copies_of(layout: &Layout, sources: &Sources) -> Vec<String> {
    sources
        .checkouts(None)
        .iter()
        .filter(|c| layout.locate(&c.dir).task.is_some())
        .map(|c| c.dir.display().to_string())
        .collect()
}

/// The first three of `lines`, joined.
fn first(lines: &[&str]) -> String {
    lines.iter().take(3).copied().collect::<Vec<_>>().join(", ")
}

/// The branch's sources.toml, read by this (main's) xtask. A file main's own sources.toml reads fine but the branch's does not is a schema change: this xtask cannot judge
/// the branch, and the bare parse error (`TOML parse error at line 1, column 1`) does not say so. `root` resolves from xtask's manifest dir, so the branch's xtask cannot land
/// into main either: the land goes by hand.
fn branch_sources(repo: &Path, wt: &Path, branch: &str) -> Result<sources::Sources> {
    sources::load(wt).or_else(|e| {
        if sources::load(repo).is_err() {
            return Err(e);
        }
        bail!(
            "branch {branch} changes the sources.toml schema: main's xtask cannot read {} ({e:#}). Land by hand: `cargo xtask check` in that worktree (the branch's xtask), then in {} `git merge --ff-only {branch}`, `git push`, `cargo xtask fetch --execute` (now the branch's xtask)",
            wt.display(),
            repo.display()
        )
    })
}

/// A commit as the messages show it.
fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCES: &str = "[aosp]\nremote = \"https://android.googlesource.com/\"\ntag = \"android-16.0.0_r1\"\nmanifest = \"0123456789abcdef0123456789abcdef01234567\"\nprojects = {}\n";

    /// A branch whose sources.toml main's schema cannot read (a renamed table) is named as a schema change; when main's own file fails too, the plain error stays.
    #[test]
    fn schema_change_is_named() {
        let root = std::env::temp_dir().join(format!("xtask-land-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (repo, wt) = (root.join("repo"), root.join("wt"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        write(&repo, "sources.toml", SOURCES);
        write(
            &wt,
            "sources.toml",
            &SOURCES.replace("[aosp]", "[platform]"),
        );
        let e = format!(
            "{:#}",
            branch_sources(&repo, &wt, "b").expect_err("new schema")
        );
        assert!(
            e.contains("branch b changes the sources.toml schema"),
            "{e}"
        );
        write(&wt, "sources.toml", SOURCES);
        assert!(branch_sources(&repo, &wt, "b").is_ok());
        write(&repo, "sources.toml", "not toml [");
        write(&wt, "sources.toml", "not toml [");
        let e = format!(
            "{:#}",
            branch_sources(&repo, &wt, "b").expect_err("both broken")
        );
        assert!(!e.contains("schema"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Temp tree: `origin.git` (bare, the remote), `ws/repo` (the main checkout on main, pushed) and `ws/wt/land` (a worktree on `land`, one commit ahead). No upstreams, so `check` has nothing to fetch.
    struct Env {
        root: PathBuf,
        origin: PathBuf,
        repo: PathBuf,
        wt: PathBuf,
    }

    impl Env {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("xtask-land-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let origin = root.join("origin.git");
            let repo = root.join("ws").join("repo");
            let wt = root.join("ws").join("wt").join("land");
            std::fs::create_dir_all(&repo).unwrap();
            // HEAD of the bare remote must name main, or a clone of it has no branch to push onto
            git(
                Some(&root),
                &["init", "-q", "--bare", "-b", "main", "origin.git"],
            )
            .unwrap();
            git(Some(&repo), &["init", "-q", "-b", "main"]).unwrap();
            git(Some(&repo), &["config", "user.name", "t"]).unwrap();
            git(Some(&repo), &["config", "user.email", "t@t"]).unwrap();
            // the tables `Plan` and `patch::check` read
            write(&repo, "sources.toml", SOURCES);
            write(
                &repo,
                "licenses.toml",
                "[artifact.runtime]\nallow = [\"MIT\"]\n",
            );
            write(&repo, "deny.toml", "[licenses]\nallow = [\"MIT\"]\n");
            write(&repo, "aosp-prune.toml", "");
            commit(&repo, "base");
            let origin_arg = origin.display().to_string();
            git(Some(&repo), &["remote", "add", "origin", &origin_arg]).unwrap();
            git(Some(&repo), &["push", "-q", "origin", "main"]).unwrap();
            let wt_arg = wt.display().to_string();
            git(
                Some(&repo),
                &["worktree", "add", "-q", "-b", "land", &wt_arg],
            )
            .unwrap();
            write(&wt, "change.txt", "land\n");
            commit(&wt, "land");
            Env {
                root,
                origin,
                repo,
                wt,
            }
        }

        fn origin_main(&self) -> String {
            git(Some(&self.origin), &["rev-parse", "refs/heads/main"]).unwrap()
        }

        /// Another clone pushes to origin's main: origin/main moves on, this repo does not.
        fn advance_origin(&self) {
            let other = self.root.join("other");
            let origin_arg = self.origin.display().to_string();
            let other_arg = other.display().to_string();
            git(Some(&self.root), &["clone", "-q", &origin_arg, &other_arg]).unwrap();
            write(&other, "other.txt", "other\n");
            commit(&other, "other");
            git(Some(&other), &["push", "-q", "origin", "main"]).unwrap();
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn write(dir: &Path, file: &str, text: &str) {
        std::fs::write(dir.join(file), text).unwrap();
    }

    fn commit(dir: &Path, msg: &str) {
        git(Some(dir), &["add", "-A"]).unwrap();
        git(
            Some(dir),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        )
        .unwrap();
    }

    #[test]
    fn dry_run_lists_the_stages_and_changes_nothing() {
        let env = Env::new("dry");
        let before = env.origin_main();
        let plan = Plan::new(&env.repo, "land", false).unwrap();
        let stages = plan.stages();
        let names: Vec<&str> = stages.iter().map(Stage::name).collect();
        assert_eq!(names, ["verify", "push", "sync", "verify-main", "cleanup"]);
        let lines = plan.describe(&stages);
        assert!(
            lines[1].contains("refs/heads/land:refs/heads/main"),
            "{}",
            lines[1]
        );
        assert!(lines[4].contains("branch land"), "{}", lines[4]);
        assert_eq!(env.origin_main(), before);
        assert!(env.wt.is_dir());
    }

    #[test]
    fn branch_behind_origin_main_is_refused() {
        let env = Env::new("nonff");
        env.advance_origin();
        let moved = env.origin_main();
        let err = Plan::new(&env.repo, "land", true)
            .err()
            .expect("a branch behind origin/main must be refused");
        assert!(format!("{err:#}").contains("not a fast-forward"), "{err:#}");
        assert_eq!(env.origin_main(), moved);
    }

    #[test]
    fn failing_check_stops_before_the_push() {
        let env = Env::new("failing");
        let before = env.origin_main();
        let plan = Plan::new(&env.repo, "land", true).unwrap();
        let stages = plan.stages();
        let check_fn = |_: &Path| -> Result<String> {
            bail!("check: FAILED 1 of 12 steps: drift; first error (drift): \"x\"")
        };
        let fetch_fn = |_: &Path| -> Result<()> {
            panic!("the main checkout was fetched after a failed check")
        };
        let cleanup_fn = |_: &Path, _: &str| -> Result<String> {
            panic!("the cleanup child ran after a failed check")
        };
        let line = plan.execute(
            &stages,
            &Runner {
                check: &check_fn,
                fetch: &fetch_fn,
                cleanup: &cleanup_fn,
            },
        );
        assert!(
            line.starts_with(
                "land: FAILED 1 of 5 steps: verify; first error (verify): \"nothing pushed"
            ),
            "{line}"
        );
        assert_eq!(env.origin_main(), before);
        assert!(env.wt.is_dir());
    }

    #[test]
    fn passing_checks_land_and_clean_up() {
        let env = Env::new("landed");
        let head = git(Some(&env.wt), &["rev-parse", "HEAD"]).unwrap();
        let plan = Plan::new(&env.repo, "land", true).unwrap();
        let check_fn = |_: &Path| -> Result<String> { Ok("check: ok (12 steps)".to_owned()) };
        let fetch_fn = |_: &Path| -> Result<()> { Ok(()) };
        // the cleanup child, run in this process: the stage must reach it through the runner
        let children = std::cell::RefCell::new(Vec::new());
        let cleanup_fn = |repo: &Path, branch: &str| -> Result<String> {
            children
                .borrow_mut()
                .push((repo.to_path_buf(), branch.to_owned()));
            clean_up_child(repo, branch)
        };
        let stages = plan.stages();
        let line = plan.execute(
            &stages,
            &Runner {
                check: &check_fn,
                fetch: &fetch_fn,
                cleanup: &cleanup_fn,
            },
        );
        assert_eq!(line, "land: ok (5 steps)");
        assert_eq!(*children.borrow(), [(env.repo.clone(), "land".to_owned())]);
        assert_eq!(env.origin_main(), head);
        assert_eq!(git(Some(&env.repo), &["rev-parse", "HEAD"]).unwrap(), head);
        assert!(!env.wt.exists(), "the worktree is not removed");
        assert!(
            git(
                Some(&env.repo),
                &["rev-parse", "--verify", "-q", "refs/heads/land"]
            )
            .is_err(),
            "the branch is not removed"
        );
    }

    #[test]
    fn cleanup_removes_the_task_copies_from_main() {
        let env = Env::new("copies");
        // an upstream checkout `src/up` and its task copy `src-wt/land/up`, as `patch --edit` leaves them
        let up = env.root.join("ws").join("src").join("up");
        std::fs::create_dir_all(&up).unwrap();
        git(Some(&up), &["init", "-q"]).unwrap();
        write(&up, "a.c", "int a;\n");
        commit(&up, "up");
        let pin = git(Some(&up), &["rev-parse", "HEAD"]).unwrap();
        let copy = env.root.join("ws").join("src-wt").join("land").join("up");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        let copy_arg = copy.display().to_string();
        git(
            Some(&up),
            &["worktree", "add", "-q", "--detach", &copy_arg, &pin],
        )
        .unwrap();
        // the row `done` reads from main's sources.toml; main has no task copies, so only the land's layout resolves it
        let sources = format!(
            "{SOURCES}\n[up]\ngit = \"file:///unused\"\ntag = \"v1\"\ncommit = \"{pin}\"\n"
        );
        write(&env.repo, "sources.toml", &sources);
        assert_eq!(
            patch::done_task(&env.repo, "land", vec!["up".to_owned()]).unwrap(),
            0
        );
        assert!(!copy.exists(), "the task copy is kept");
        assert!(up.is_dir(), "the shared checkout went with the copy");
    }

    #[test]
    fn clean_up_removes_the_checkouts_target_and_its_slot_build() {
        let env = Env::new("target");
        let ws = env.root.join("ws");
        let slot_target = ws.join("out/slot-1/target");
        std::fs::create_dir_all(slot_target.join("debug")).unwrap();
        write(&slot_target.join("debug"), "build.txt", "x\n");
        std::os::unix::fs::symlink(&slot_target, env.wt.join("target")).unwrap();
        std::fs::write(env.repo.join(".git/worktrees/land/atl-out-slot"), "1\n").unwrap();
        // `branch -d` in clean_up refuses a branch that is not merged into the checked-out main
        git(Some(&env.repo), &["merge", "-q", "--ff-only", "land"]).unwrap();
        let summary = clean_up(&env.repo, &ws, &env.wt, "land").expect("clean up");
        assert!(summary.contains("target removed:"), "{summary}");
        assert!(
            !slot_target.exists(),
            "the slot's build leaves with its holder"
        );
        assert!(!env.wt.exists(), "the worktree is removed");
    }

    #[test]
    fn only_a_passing_verdict_passes() {
        assert!(passed("check: ok"));
        assert!(passed(
            "check: ok (12 steps, 1 skipped: converter-packages)"
        ));
        assert!(!passed("check: okay"));
        assert!(!passed(
            "check: FAILED 1 of 12 steps: drift; first error (drift): \"x\""
        ));
    }

    #[test]
    fn worktree_lookup_matches_the_whole_branch_name() {
        let list = "worktree /a/repo\nHEAD 1\nbranch refs/heads/main\n\nworktree /a/wt/land\nHEAD 2\nbranch refs/heads/land\n\nworktree /a/wt/landing\nHEAD 3\nbranch refs/heads/landing\n";
        assert_eq!(
            find_worktree(list, "land"),
            Some(PathBuf::from("/a/wt/land"))
        );
        assert_eq!(
            find_worktree(list, "landing"),
            Some(PathBuf::from("/a/wt/landing"))
        );
        assert_eq!(find_worktree(list, "lan"), None);
    }

    #[test]
    fn the_state_note_is_the_only_untracked_file_allowed() {
        assert!(dirty_lines("?? .state-note.md\n").is_empty());
        assert_eq!(
            dirty_lines(" M src/a.rs\n?? .state-note.md\n?? b.txt\n"),
            vec![" M src/a.rs", "?? b.txt"]
        );
    }
}
