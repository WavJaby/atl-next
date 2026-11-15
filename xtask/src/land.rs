// SPDX-License-Identifier: MIT
//! `xtask land <branch>`: puts a task branch on origin/main, with the branch's own patches checked before the push.
//! Why this order: `src/<up>` is one checkout shared by every worktree and holds origin/main's pins and patches (`fetch::Main`), so a branch's unlanded patches exist only in its task copies (`src-wt/<branch>/<up>`), which only the branch's worktree resolves (`atl-layout`).
//! Main has no task copies, so `check` run on main sees `src/` alone. That is why the branch is checked before the push in a checkout on the branch, and why its copies go only after main and `src/` reached the new commit.
//! That checkout is the land worktree [`LAND_WT`] (build dir `ws/land`), never the branch's own: one fixed path whose build dir holds the last landed tree, so Build compiles only the branch's diff (cargo outputs key on the checkout path too).
//! It checks the branch out beside the branch's own worktree (`--ignore-other-worktrees`; land changes neither), so the task copies resolve by branch name as there. Green and pushed -> `ws/land` is published as `base/<commit>`, the copy new build dirs start from.
//! Failed before the push -> the land worktree goes back to origin/main and `ws/land` is re-copied from the newest base: no failed branch's outputs stay in the dir the next base comes from.
//! Nothing reaches origin before the push. A task copy is a worktree of `src/` on `atl-patches-<branch>`: it leaves every other worktree's checkout alone.
//! This process is main's xtask, so it never parses the branch's tables: a branch may change their schema. What they say comes from the branch's own xtask in its worktree (`--facts` child, `patch --edit`, `check`).
//! Cleanup runs in a child `cargo xtask land <branch> --cleanup --execute` in main: this process's binary predates the sync, so it cannot parse the tables the sync brought in.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::{Layout, MAIN};

use crate::fetch;
use crate::git::{git, git_raw};
use crate::out;
use crate::out_prune;
use crate::patch;
use crate::sources::{self, Sources};
use crate::timing::{self, Timings};
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{self, Failure, Verdict};

/// The note a task keeps in its worktree: untracked by design, copied to `out/` when the branch lands.
const STATE_NOTE: &str = ".state-note.md";

/// The land worktree, workspace-relative; its git worktree name is [`out::LAND_ID`], so its build dir is `ws/land`. Made detached at origin/main by the first land.
pub const LAND_WT: &str = "wt/land";

#[derive(clap::Args)]
pub struct Args {
    /// Task branch to land: checked out in a worktree, and a fast-forward of origin/main.
    branch: String,
    /// Act: build and check in the land worktree, push, bring the main checkout and `src/` to the new commit, publish the land build dir as a base, clean up. Without it, prints the steps only.
    #[arg(long)]
    execute: bool,
    /// Internal: the cleanup stage's child (`cleanup_execute`); runs only the cleanup. Needs `--execute`.
    #[arg(long, hide = true, requires = "execute")]
    cleanup: bool,
    /// Internal: the preflight's child in the branch's worktree (`facts_child`); prints what the branch's tables say, read-only.
    #[arg(long, hide = true, conflicts_with_all = ["execute", "cleanup"])]
    facts: bool,
    /// Internal: the build dir child in the land worktree (`build_dir_child`); prints whether its build dir is current for every gate's goals, read-only.
    #[arg(long, hide = true, conflicts_with_all = ["execute", "cleanup", "facts"])]
    build_dir: bool,
}

/// What the branch's own tables say, as the branch's xtask reads them (`branch_facts`).
#[derive(Debug, Default, PartialEq, Eq)]
struct BranchFacts {
    /// Upstreams needing a task copy in the worktree.
    edits: Vec<String>,
    /// Upstreams whose task copy exists already.
    copies: Vec<String>,
    /// The build machine's arches.toml host: what Build builds the land build dir for.
    host: String,
}

/// `(worktree, branch)` -> the branch's facts; `facts_child` in production.
type FactsFn<'a> = dyn Fn(&Path, &str) -> Result<BranchFacts> + 'a;

/// What `land` runs beside its git work: the build and check of the land worktree, the fetch of the main checkout, the branch's task copies, the base publish and reset, and the cleanup child. Tests substitute them.
struct Runner<'a> {
    /// `xtask patch --edit <up>` in the branch's worktree, by the branch's xtask (`edit_execute`).
    edit: &'a dyn Fn(&Path, &str) -> Result<String>,
    /// The checkout's verdict line, Ok only when that check passed (`check_verdict`).
    check: &'a dyn Fn(&Path) -> Result<String>,
    /// `xtask fetch --execute` in the main checkout.
    fetch: &'a dyn Fn(&Path) -> Result<()>,
    /// The cleanup child for the main checkout and the branch; its note line (`cleanup_execute`).
    cleanup: &'a dyn Fn(&Path, &str) -> Result<String>,
    /// `(checkout, host)` -> `xtask native --for all-gates` there, by its own xtask (`native_execute`).
    native: &'a dyn Fn(&Path, &str) -> Result<String>,
    /// `(land worktree, branch)` -> None when its build dir is current for every gate's goals, else why not; by the branch's xtask (`build_dir_child`).
    stale: &'a dyn Fn(&Path, &str) -> Result<Option<String>>,
    /// The land worktree's build dir published as `base/<its HEAD>` (`out::publish`); its line.
    publish: &'a dyn Fn(&Path) -> Result<String>,
    /// The land worktree's build dir deleted and copied again from the newest base (`out::recopy`); its line.
    recopy: &'a dyn Fn(&Path) -> Result<String>,
}

/// One step of a landing, in run order.
#[derive(Debug, PartialEq, Eq)]
enum Stage {
    /// A task copy for an upstream whose patches/ or pin differ from origin/main's.
    Edit(String),
    /// The land worktree on the branch (`Plan::enter`), made first when missing.
    Enter,
    /// The land build dir brought to the branch's tree: `xtask native --for all-gates` there unless already current (`Plan::build`). Verify's check refuses a stale build dir by name.
    Build,
    /// `cargo xtask check` in the land worktree.
    Verify,
    Push,
    /// Main fast-forwards to origin/main, then `fetch --execute` moves `src/` there.
    Sync,
    /// The land worktree detached at the pushed commit, its build dir published as `base/<commit>` (`Plan::publish`).
    Publish,
    /// Task copies, the state note (kept in out/), the worktree and the branch, in a child (`cleanup_execute`): this process's binary predates the sync.
    Cleanup,
}

impl Stage {
    fn name(&self) -> &'static str {
        match self {
            Stage::Edit(_) => "edit",
            Stage::Enter => "enter",
            Stage::Build => "build",
            Stage::Verify => "verify",
            Stage::Push => "push",
            Stage::Sync => "sync",
            Stage::Publish => "publish",
            Stage::Cleanup => "cleanup",
        }
    }

    /// A failure here leaves origin as it was: the land worktree is reset (`Plan::reset`).
    fn before_push(&self) -> bool {
        matches!(
            self,
            Stage::Edit(_) | Stage::Enter | Stage::Build | Stage::Verify | Stage::Push
        )
    }
}

/// The preflight's facts: what the stages need, read without writing.
struct Plan {
    repo: PathBuf,
    branch: String,
    /// The branch's own worktree: task copies are made from it (Edit), and it goes in Cleanup.
    wt: PathBuf,
    /// The land worktree ([`LAND_WT`]); may not exist yet (Enter makes it).
    land: PathBuf,
    /// origin/main before the push.
    origin: String,
    /// The branch's tip: origin/main after the push.
    head: String,
    /// Upstreams needing a task copy in the worktree.
    edits: Vec<String>,
    /// Upstreams whose task copy exists already.
    copies: Vec<String>,
    /// The build machine's host arch, from the branch's arches.toml (`BranchFacts::host`).
    host: String,
}

/// `xtask land <branch>`: prints the steps unless `--execute`.
pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.cleanup {
        println!("{}", clean_up_child(root, &args.branch)?);
        return Ok(ExitCode::SUCCESS);
    }
    if args.facts {
        let f = branch_facts(root)?;
        for e in &f.edits {
            println!("{FACT_EDIT}{e}");
        }
        for c in &f.copies {
            println!("{FACT_COPY}{c}");
        }
        println!("{FACT_HOST}{}", f.host);
        return Ok(ExitCode::SUCCESS);
    }
    if args.build_dir {
        match build_dir_staleness(root)? {
            None => println!("{BUILD_DIR_CURRENT}"),
            Some(why) => println!("{BUILD_DIR_STALE}{why}"),
        }
        return Ok(ExitCode::SUCCESS);
    }
    let started = Instant::now();
    let plan = Plan::new(root, &args.branch, args.execute, &facts_child)?;
    let stages = plan.stages();
    // preflight = origin fetch + read-only checks; not a stage, so outside the table
    println!("land: preflight ({}) ok", timing::secs(started.elapsed()));
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
        edit: &edit_execute,
        check: &check_verdict,
        fetch: &fetch_execute,
        cleanup: &cleanup_execute,
        native: &native_execute,
        stale: &build_dir_child,
        publish: &out::publish,
        recopy: &out::recopy,
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
    /// `facts`: what the branch's own tables say (`facts_child` = the branch's xtask in its worktree); this process never parses them, since its binary is main's.
    fn new(repo: &Path, branch: &str, update: bool, facts: &FactsFn<'_>) -> Result<Self> {
        ensure!(branch != MAIN, "`{MAIN}` is the trunk: land a task branch");
        ensure!(
            branch != out::LAND_ID,
            "`{}` names the land worktree {LAND_WT}: rename the branch",
            out::LAND_ID
        );
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
        let land = Layout::of(repo)?.workspace().join(LAND_WT);
        land_ready(&land)?;
        let worktrees = git_raw(repo, &["worktree", "list", "--porcelain"])?;
        let wt = find_worktree(&worktrees, branch, &land).with_context(|| {
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
        let BranchFacts {
            edits,
            copies,
            host,
        } = facts(&wt, branch)?;
        Ok(Plan {
            repo: repo.to_path_buf(),
            branch: branch.to_owned(),
            wt,
            land,
            origin,
            head,
            edits,
            copies,
            host,
        })
    }

    /// The stages in run order; the push is absent when origin/main already is the branch.
    fn stages(&self) -> Vec<Stage> {
        let mut stages: Vec<Stage> = self.edits.iter().cloned().map(Stage::Edit).collect();
        // after the edits: a new task copy changes what native builds (build_state::of_build)
        stages.extend([Stage::Enter, Stage::Build, Stage::Verify]);
        if self.head != self.origin {
            stages.push(Stage::Push);
        }
        stages.extend([Stage::Sync, Stage::Publish, Stage::Cleanup]);
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
        let land = self.land.display();
        match stage {
            Stage::Edit(up) => format!(
                "edit {up}: task copy src-wt/{branch}/{up} on atl-patches-{branch} (its patches/ or pin differ from {})",
                fetch::MAIN_REF
            ),
            Stage::Enter => format!(
                "enter {land}: check out {branch} there (made detached at {} first if missing)",
                fetch::MAIN_REF
            ),
            Stage::Build => format!(
                "build in {land}: `cargo xtask native --host {} --for {}` unless ws/{} is current for the branch's tree",
                self.host,
                crate::gate_goals::ALL,
                out::LAND_ID
            ),
            Stage::Verify => format!(
                "verify in {land}: `cargo xtask check` must print `check: ok` (the task copies hold the branch's patches); a failure up to the push resets {land} to {} and re-copies ws/{} from the newest base",
                fetch::MAIN_REF,
                out::LAND_ID
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
            Stage::Publish => format!(
                "publish: {land} detached at {}, ws/{} copied to base/{}",
                short(&self.head),
                out::LAND_ID,
                short(&self.head)
            ),
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
    /// Each stage's line carries its wall time; the time table closes the run, before the verdict line, on success or failure.
    fn execute(&self, stages: &[Stage], runner: &Runner) -> String {
        let total = stages.len();
        let mut times = Timings::default();
        for stage in stages {
            let started = Instant::now();
            let result = self.run_stage(stage, runner);
            let took = started.elapsed();
            times.push(stage.name(), took);
            match result {
                Ok(note) => println!("land: {} ({}) ok: {note}", stage.name(), timing::secs(took)),
                Err(e) => {
                    println!(
                        "land: {} ({}) FAILED: {e:#}",
                        stage.name(),
                        timing::secs(took)
                    );
                    let started = Instant::now();
                    let after = self.after_failure(stage, runner);
                    times.push("reset", started.elapsed());
                    println!("land: reset: {after}");
                    println!("{}", times.table("land").trim_end());
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
        println!("{}", times.table("land").trim_end());
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
            Stage::Edit(up) => (runner.edit)(&self.wt, up),
            Stage::Enter => self.enter(),
            Stage::Build => self.build(runner),
            Stage::Verify => (runner.check)(self.land.as_path()).with_context(|| {
                format!("nothing pushed: check in {} failed", self.land.display())
            }),
            Stage::Push => {
                let refspec = format!("refs/heads/{}:refs/heads/main", self.branch);
                git(Some(&self.repo), &["push", "-q", "origin", &refspec])
                    .context("nothing pushed: git push refused")?;
                Ok(format!("origin/main is {}", short(&self.head)))
            }
            Stage::Sync => self.sync(runner),
            Stage::Publish => self.publish(runner),
            Stage::Cleanup => (runner.cleanup)(&self.repo, &self.branch)
                .context("pushed and checked, but cleanup failed: finish it by hand"),
        }
    }

    /// The land worktree on the branch. Missing -> made detached at origin/main first (its build dir follows from the newest ancestor base on the first xtask command there, `out::ensure`).
    /// `--ignore-other-worktrees`: the branch's own worktree keeps it checked out; land writes neither the branch nor that worktree until Cleanup, which detaches this one first (Publish).
    fn enter(&self) -> Result<String> {
        let mut made = " ";
        if !self.land.exists() {
            let parent = self.land.parent().context("land worktree has no parent")?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            let arg = self.land.display().to_string();
            git(
                Some(&self.repo),
                &["worktree", "add", "-q", "--detach", &arg, &self.origin],
            )
            .context("nothing pushed: making the land worktree")?;
            made = " made, ";
        }
        let id = out::checkout_id(&self.land)?;
        ensure!(
            id == out::LAND_ID,
            "nothing pushed: {} is git worktree {id}, not {}: `git worktree move` or remove it",
            self.land.display(),
            out::LAND_ID
        );
        git(
            Some(&self.land),
            &["checkout", "-q", "--ignore-other-worktrees", &self.branch],
        )
        .with_context(|| {
            format!(
                "nothing pushed: checking {} out in the land worktree",
                self.branch
            )
        })?;
        out::prepare(&self.land).context("nothing pushed: making the land build dir")?;
        Ok(format!("{}{made}on {}", self.land.display(), self.branch))
    }

    /// The land build dir as Verify needs it: built from the branch's tree (after the edits) with every gate's goals.
    /// Current -> nothing runs. Stale, or the question unanswerable -> native first: it is the reference, a needless build costs time, a skipped one fails Verify.
    fn build(&self, runner: &Runner) -> Result<String> {
        let why = match (runner.stale)(&self.land, &self.branch) {
            Ok(None) => return Ok("build dir current for the branch, not rebuilt".to_owned()),
            Ok(Some(why)) => why,
            Err(e) => format!("build dir state not read: {e:#}"),
        };
        println!("land: build: {why}: building ws/{}", out::LAND_ID);
        let note = (runner.native)(&self.land, &self.host)
            .with_context(|| format!("nothing pushed: native in {} failed", self.land.display()))?;
        Ok(format!("{note} ({why})"))
    }

    /// Detaches the land worktree at the pushed commit (Cleanup deletes the branch), then publishes its build dir: built and checked from this tree, at the canonical path. Never fatal: the land is pushed and verified; a missing base only makes the next children copy an older one.
    fn publish(&self, runner: &Runner) -> Result<String> {
        git(
            Some(&self.land),
            &["checkout", "-q", "--detach", &self.head],
        )
        .with_context(|| {
            format!(
                "pushed: origin/main is {}; detaching the land worktree failed",
                short(&self.head)
            )
        })?;
        Ok((runner.publish)(&self.land).unwrap_or_else(|e| format!("base not published: {e:#}")))
    }

    /// After a failed stage: before the push the land worktree goes back to origin/main and its build dir is re-copied from the newest base (no failed branch's outputs reach a later base); after it the worktree is detached at the pushed commit, its build dir kept (built from that commit).
    /// Returns what was done; a reset that itself fails is reported, the next land's preflight refuses an unready worktree by name.
    fn after_failure(&self, stage: &Stage, runner: &Runner) -> String {
        if matches!(stage, Stage::Edit(_)) || !self.land.exists() {
            return "land worktree untouched".to_owned();
        }
        let (at, recopy) = if stage.before_push() {
            (&self.origin, true)
        } else {
            (&self.head, false)
        };
        if let Err(e) = git(Some(&self.land), &["checkout", "-q", "-f", "--detach", at]) {
            return format!(
                "FAILED to detach {} at {}: {e:#}",
                self.land.display(),
                short(at)
            );
        }
        let detached = format!("{} detached at {}", self.land.display(), short(at));
        if !recopy {
            return format!("{detached}, ws/{} kept", out::LAND_ID);
        }
        match (runner.recopy)(&self.land) {
            Ok(line) => format!("{detached}; {line}"),
            Err(e) => format!("{detached}; re-copy FAILED: {e:#}"),
        }
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

/// The land worktree as the preflight accepts it: absent (Enter makes it), or a clean checkout on a detached HEAD (a land between Enter and Publish holds it on a branch; a crashed one leaves it there).
fn land_ready(land: &Path) -> Result<()> {
    if !land.exists() {
        return Ok(());
    }
    let on = git(Some(land), &["symbolic-ref", "-q", "--short", "HEAD"]).ok();
    ensure!(
        on.is_none(),
        "land worktree {} is on {}: another land runs, or one crashed: `git -C {} checkout --detach {}` once none runs",
        land.display(),
        on.unwrap_or_default(),
        land.display(),
        fetch::MAIN_REF
    );
    let changes = git_raw(land, &["status", "--porcelain"])?;
    ensure!(
        changes.trim().is_empty(),
        "land worktree {} has changes:\n{}",
        land.display(),
        changes.trim_end()
    );
    Ok(())
}

/// `--facts` output line prefixes.
const FACT_EDIT: &str = "land-facts: edit ";
const FACT_COPY: &str = "land-facts: copy ";
const FACT_HOST: &str = "land-facts: host ";

/// The branch's facts from its own tables, read by whichever xtask runs this: the `--facts` child in the worktree `wt`, i.e. the branch's xtask, so a table schema the branch changes is read by the code that changed it.
/// origin/main's tables (`fetch::Main`) are read through the refs the worktree shares with the main checkout.
fn branch_facts(wt: &Path) -> Result<BranchFacts> {
    let layout = Layout::of(wt)?;
    let sources = sources::load(wt)?;
    let main = fetch::Main::load(wt)?;
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
        if main.refusal(&c, &stored).is_some() && layout.locate(&c.dir).task.is_none() {
            edits.push(c.dir.display().to_string());
        }
    }
    let arches = crate::arches::load(wt)?;
    Ok(BranchFacts {
        edits,
        copies: copies_of(&layout, &sources),
        host: crate::arches::build_machine(wt, &arches)?.0.to_owned(),
    })
}

/// `cargo xtask land <branch> --facts` in the worktree, run by the branch's xtask (the binary its Verify stage runs too): main's binary cannot read a table schema the branch changes.
fn facts_child(wt: &Path, branch: &str) -> Result<BranchFacts> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "land", branch, "--facts"])
        .current_dir(wt);
    let run = unit_use::run(&mut cmd, Units::Plain, Echo::Nothing)
        .with_context(|| format!("running cargo xtask land --facts in {}", wt.display()))?;
    ensure!(
        run.status.success(),
        "the branch's xtask in {} could not read its tables ({}): {}",
        wt.display(),
        run.status,
        run.stderr
            .rfind("Error:")
            .map_or(run.stderr.trim_end(), |at| run.stderr[at..].trim_end())
    );
    Ok(parse_facts(&run.stdout))
}

fn parse_facts(stdout: &str) -> BranchFacts {
    let mut f = BranchFacts::default();
    for line in stdout.lines() {
        if let Some(e) = line.strip_prefix(FACT_EDIT) {
            f.edits.push(e.to_owned());
        } else if let Some(c) = line.strip_prefix(FACT_COPY) {
            f.copies.push(c.to_owned());
        } else if let Some(h) = line.strip_prefix(FACT_HOST) {
            f.host = h.to_owned();
        }
    }
    f
}

/// `--build-dir` output lines.
const BUILD_DIR_CURRENT: &str = "land-build-dir: current";
const BUILD_DIR_STALE: &str = "land-build-dir: stale ";

/// None when the build dir of the checkout at `root` was built from it as it is now with every gate's goals for the build machine's host, else why not (build_state::gate_out's refusal).
fn build_dir_staleness(root: &Path) -> Result<Option<String>> {
    let arches = crate::arches::load(root)?;
    let host = crate::arches::build_machine(root, &arches)?.0;
    Ok(
        crate::build_state::gate_out(root, None, host, crate::gate_goals::ALL)
            .err()
            .map(|e| format!("{e:#}")),
    )
}

/// `cargo xtask land <branch> --build-dir` in the worktree, run by the branch's xtask: the build dir record and the goal tables are the branch's.
fn build_dir_child(wt: &Path, branch: &str) -> Result<Option<String>> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "land", branch, "--build-dir"])
        .current_dir(wt);
    let run = unit_use::run(&mut cmd, Units::Plain, Echo::Nothing)
        .with_context(|| format!("running cargo xtask land --build-dir in {}", wt.display()))?;
    ensure!(
        run.status.success(),
        "{}: {}",
        run.status,
        run.stderr.trim_end()
    );
    parse_build_dir(&run.stdout)
}

fn parse_build_dir(stdout: &str) -> Result<Option<String>> {
    for line in stdout.lines() {
        if line == BUILD_DIR_CURRENT {
            return Ok(None);
        }
        if let Some(why) = line.strip_prefix(BUILD_DIR_STALE) {
            return Ok(Some(why.to_owned()));
        }
    }
    bail!("the build dir child printed no build dir line")
}

/// `cargo xtask native --host <host> --for all-gates` in `repo` (the land worktree), run by its own xtask: the build dir that checkout's check reads, built from it as it is now.
fn native_execute(repo: &Path, host: &str) -> Result<String> {
    let mut cmd = crate::cargo_command();
    cmd.args([
        "xtask",
        "native",
        "--host",
        host,
        "--for",
        crate::gate_goals::ALL,
    ])
    .current_dir(repo);
    let run = unit_use::run(&mut cmd, Units::Plain, Echo::Both)
        .with_context(|| format!("running cargo xtask native in {}", repo.display()))?;
    ensure!(
        run.status.success(),
        "cargo xtask native --host {host} --for {} exited with {}",
        crate::gate_goals::ALL,
        run.status
    );
    Ok(format!("native {host} for {}", crate::gate_goals::ALL))
}

/// `cargo xtask patch --edit <up>` in the worktree, run by the branch's xtask: the task copy is built from the branch's tables.
fn edit_execute(wt: &Path, up: &str) -> Result<String> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "patch", "--edit", up]).current_dir(wt);
    let run = unit_use::run(&mut cmd, Units::Plain, Echo::Both)
        .with_context(|| format!("running cargo xtask patch --edit in {}", wt.display()))?;
    ensure!(
        run.status.success(),
        "cargo xtask patch --edit {up} exited with {}",
        run.status
    );
    Ok(format!("task copy of {up}"))
}

/// `cargo xtask check` in `dir`, run by that checkout's own xtask: its verdict line, or an error carrying it when the check did not pass.
fn check_verdict(dir: &Path) -> Result<String> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "check"]).current_dir(dir);
    let run = unit_use::run(&mut cmd, Units::Plain, Echo::Both)
        .with_context(|| format!("running cargo xtask check in {}", dir.display()))?;
    let line = verdict::last_line(&run.stdout, "check")
        .unwrap_or("check: no verdict line")
        .to_owned();
    ensure!(
        run.status.success() && passed(&line),
        "{line} ({})",
        run.status
    );
    Ok(line)
}

/// `cargo xtask fetch --execute` in `dir`, run by that checkout's own xtask: after the fast-forward, the landed code moves `src/`.
fn fetch_execute(dir: &Path) -> Result<()> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "fetch", "--execute"]).current_dir(dir);
    let run = unit_use::run(&mut cmd, Units::Plain, Echo::Both)
        .with_context(|| format!("running cargo xtask fetch in {}", dir.display()))?;
    ensure!(
        run.status.success(),
        "cargo xtask fetch --execute exited with {}",
        run.status
    );
    Ok(())
}

/// The cleanup child: `cargo xtask land <branch> --cleanup --execute` in the main checkout, run by that checkout's own xtask.
/// `cargo run` rebuilds it from the source the sync moved to; the pre-sync binary cannot parse the tables the sync brought (`licenses.toml` `[test_only]`). Returns the note line it printed.
fn cleanup_execute(repo: &Path, branch: &str) -> Result<String> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "land", branch, "--cleanup", "--execute"])
        .current_dir(repo);
    let run = unit_use::run(&mut cmd, Units::Plain, Echo::Both)
        .with_context(|| format!("running cargo xtask land --cleanup in {}", repo.display()))?;
    ensure!(
        run.status.success(),
        "{}: {}",
        run.status,
        run.stderr
            .rfind("Error:")
            .map_or(run.stderr.trim_end(), |at| run.stderr[at..].trim_end())
    );
    run.stdout
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
    let ws = Layout::of(repo)?.workspace().to_path_buf();
    let wt = find_worktree(&porcelain, branch, &ws.join(LAND_WT))
        .with_context(|| format!("branch {branch} is not checked out in a worktree"))?;
    let note = clean_up(repo, &ws, &wt, branch)?;
    prune_units_after_land(repo, &ws);
    Ok(note)
}

/// After a land: `out prune --units --execute` over every live checkout's build dir. The trees no build dir owns (EGL probes) are not pruned here: `out prune --units` does that by hand. A busy dir is skipped; a failure is logged, never fatal, since the land is already pushed.
fn prune_units_after_land(repo: &Path, ws: &Path) {
    let swept = out::live_dirs(repo)
        .and_then(|ids| out_prune::sweep(ws, &ids, false, out_prune::PRUNE_AFTER_DAYS, true));
    match swept {
        Ok(sweep) if sweep.failed == 0 => {}
        Ok(sweep) => eprintln!(
            "unit prune after land: {} dir(s) failed (rows above)",
            sweep.failed
        ),
        Err(e) => eprintln!("unit prune after land failed: {e:#}"),
    }
}

/// The task copies, then the state note (kept in out/), the checkout's target and build dir (`out::leave`), the worktree and the branch. Copies go through `patch --done --task`, the command's own path.
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
    let target = out::leave(wt, ws)?;
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

/// The worktree with `branch` checked out other than the land worktree `land` (which holds it during a land), from `git worktree list --porcelain` (blocks of `worktree <path>` and `branch refs/heads/<name>` lines).
fn find_worktree(porcelain: &str, branch: &str, land: &Path) -> Option<PathBuf> {
    let want = format!("branch refs/heads/{branch}");
    porcelain.split("\n\n").find_map(|block| {
        let path = block.lines().find_map(|l| l.strip_prefix("worktree "))?;
        (Path::new(path) != land && block.lines().any(|l| l == want)).then(|| PathBuf::from(path))
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

/// A commit as the messages show it.
fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCES: &str = "[aosp]\nremote = \"https://android.googlesource.com/\"\ntag = \"android-16.0.0_r1\"\nmanifest = \"0123456789abcdef0123456789abcdef01234567\"\nprojects = {}\n";

    /// The branch's facts in this process: the tests' stand-in for `facts_child` (this code is the branch's xtask here).
    fn in_process(wt: &Path, _: &str) -> Result<BranchFacts> {
        branch_facts(wt)
    }

    fn no_edit(_: &Path, up: &str) -> Result<String> {
        panic!("edit stage ran for {up}")
    }

    /// A branch adding a sources.toml key this xtask refuses (a schema change, as F1's `[aosp.needed_by]` was to main's) still plans and lands: the preflight takes the branch's tables only from `facts` (the branch's xtask), never parses them itself.
    #[test]
    fn branch_tables_are_read_by_the_branchs_xtask() {
        let env = Env::new("schema");
        write(
            &env.wt,
            "sources.toml",
            &format!("{SOURCES}[aosp.needed_by]\nfuture_key = 1\n[future]\nx = 1\n"),
        );
        commit(&env.wt, "schema change");
        assert!(
            sources::load(&env.wt).is_err(),
            "the fixture must be a schema this xtask refuses"
        );
        let asked = std::cell::RefCell::new(Vec::new());
        let facts = |wt: &Path, branch: &str| -> Result<BranchFacts> {
            asked
                .borrow_mut()
                .push((wt.to_path_buf(), branch.to_owned()));
            Ok(BranchFacts {
                edits: Vec::new(),
                copies: vec!["up".to_owned()],
                host: "h".to_owned(),
            })
        };
        let plan = Plan::new(&env.repo, "task", true, &facts)
            .expect("main's binary must not parse the branch's tables");
        assert_eq!(*asked.borrow(), [(env.wt.clone(), "task".to_owned())]);
        assert_eq!(plan.copies, ["up"]);
        let check_fn = |_: &Path| -> Result<String> { Ok("check: ok (12 steps)".to_owned()) };
        let fetch_fn = |_: &Path| -> Result<()> { Ok(()) };
        let cleanup_fn = |_: &Path, _: &str| -> Result<String> { Ok("cleaned".to_owned()) };
        let line = plan.execute(
            &plan.stages(),
            &Runner {
                edit: &no_edit,
                check: &check_fn,
                fetch: &fetch_fn,
                cleanup: &cleanup_fn,
                native: &no_native,
                stale: &current,
                publish: &no_publish,
                recopy: &no_recopy,
            },
        );
        assert_eq!(line, "land: ok (7 steps)");
        // the child's output is what the parent plans from
        assert_eq!(
            parse_facts(
                "noise\nland-facts: edit aosp/x\nland-facts: copy up\nland-facts: host x86_64\n"
            ),
            BranchFacts {
                edits: vec!["aosp/x".into()],
                copies: vec!["up".into()],
                host: "x86_64".into(),
            }
        );
    }

    fn no_native(d: &Path, host: &str) -> Result<String> {
        panic!("native ran in {} for {host}", d.display())
    }

    fn no_publish(_: &Path) -> Result<String> {
        Ok("out: base/c from ws/land (reflink)".to_owned())
    }

    fn no_recopy(d: &Path) -> Result<String> {
        panic!("ws/land of {} re-copied after a green land", d.display())
    }

    /// The worktree's build dir is current: Build runs nothing.
    fn current(_: &Path, _: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// A build dir built before the rebase, simulated: `stale` compares its record with the tree, `native` rebuilds it, `check` refuses it by name as check.rs ConverterPackages does (build_state::ensure_current); `recopy` puts it back to the base state.
    struct BuildDir {
        built: std::cell::RefCell<&'static str>,
        calls: std::cell::RefCell<Vec<String>>,
    }

    const REBASED: &str = "rebased";
    const BASE: &str = "pre-rebase";
    const STALE_MSG: &str = "was built from another source state";

    impl BuildDir {
        fn before_rebase() -> Self {
            BuildDir {
                built: std::cell::RefCell::new(BASE),
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn run(&self, plan: &Plan, stages: &[Stage]) -> String {
            let stale_fn = |d: &Path, _: &str| -> Result<Option<String>> {
                self.calls
                    .borrow_mut()
                    .push(format!("stale? {}", d.display()));
                let b = *self.built.borrow();
                Ok((b != REBASED).then(|| format!("build dir {STALE_MSG} ({b})")))
            };
            let native_fn = |d: &Path, host: &str| -> Result<String> {
                self.calls
                    .borrow_mut()
                    .push(format!("native {host} {}", d.display()));
                *self.built.borrow_mut() = REBASED;
                Ok("built".to_owned())
            };
            let check_fn = |d: &Path| -> Result<String> {
                self.calls
                    .borrow_mut()
                    .push(format!("check {}", d.display()));
                let b = *self.built.borrow();
                ensure!(b == REBASED, "check: FAILED: build dir {STALE_MSG} ({b})");
                Ok("check: ok (12 steps)".to_owned())
            };
            let publish_fn = |d: &Path| -> Result<String> {
                self.calls
                    .borrow_mut()
                    .push(format!("publish {}", d.display()));
                Ok("published".to_owned())
            };
            let recopy_fn = |d: &Path| -> Result<String> {
                self.calls
                    .borrow_mut()
                    .push(format!("recopy {}", d.display()));
                *self.built.borrow_mut() = BASE;
                Ok("re-copied".to_owned())
            };
            let fetch_fn = |_: &Path| -> Result<()> { Ok(()) };
            let cleanup_fn = |_: &Path, _: &str| -> Result<String> { Ok("cleaned".to_owned()) };
            plan.execute(
                stages,
                &Runner {
                    edit: &no_edit,
                    check: &check_fn,
                    fetch: &fetch_fn,
                    cleanup: &cleanup_fn,
                    native: &native_fn,
                    stale: &stale_fn,
                    publish: &publish_fn,
                    recopy: &recopy_fn,
                },
            )
        }
    }

    /// A build dir from before the branch: Build rebuilds it in the land worktree, Verify passes there on the first attempt, Publish copies it; the branch's own worktree is never built or checked.
    #[test]
    fn stale_build_dir_is_built_before_verify() {
        let env = Env::new("stale");
        let plan = Plan::new(&env.repo, "task", true, &in_process).unwrap();
        let dir = BuildDir::before_rebase();
        assert_eq!(dir.run(&plan, &plan.stages()), "land: ok (7 steps)");
        let land = env.land.display();
        assert_eq!(
            *dir.calls.borrow(),
            [
                format!("stale? {land}"),
                format!("native {} {land}", plan.host),
                format!("check {land}"),
                format!("publish {land}"),
            ]
        );
    }

    /// The break: without Build, the same stale build dir fails Verify by name and nothing is pushed; the land worktree goes back to origin/main and its build dir is re-copied.
    #[test]
    fn without_build_the_stale_build_dir_fails_verify() {
        let env = Env::new("nobuild");
        let before = env.origin_main();
        let plan = Plan::new(&env.repo, "task", true, &in_process).unwrap();
        let stages: Vec<Stage> = plan
            .stages()
            .into_iter()
            .filter(|s| *s != Stage::Build)
            .collect();
        let dir = BuildDir::before_rebase();
        let line = dir.run(&plan, &stages);
        assert!(
            line.starts_with("land: FAILED 1 of 6 steps: verify;") && line.contains(STALE_MSG),
            "{line}"
        );
        assert_eq!(env.origin_main(), before);
        let land = env.land.display();
        assert_eq!(
            *dir.calls.borrow(),
            [format!("check {land}"), format!("recopy {land}")]
        );
        assert_eq!(env.land_head(), (None, before));
    }

    /// A build dir already built from the branch's tree: Build asks, runs no native, and the land goes through.
    #[test]
    fn current_build_dir_is_not_rebuilt() {
        let env = Env::new("current");
        let plan = Plan::new(&env.repo, "task", true, &in_process).unwrap();
        let dir = BuildDir::before_rebase();
        *dir.built.borrow_mut() = REBASED;
        assert_eq!(dir.run(&plan, &plan.stages()), "land: ok (7 steps)");
        let land = env.land.display();
        assert_eq!(
            *dir.calls.borrow(),
            [
                format!("stale? {land}"),
                format!("check {land}"),
                format!("publish {land}")
            ]
        );
        assert_eq!(
            parse_build_dir("x\nland-build-dir: current\n").unwrap(),
            None
        );
        assert_eq!(
            parse_build_dir("land-build-dir: stale no record\n")
                .unwrap()
                .as_deref(),
            Some("no record")
        );
        assert!(parse_build_dir("noise\n").is_err());
    }

    /// The real question on a checkout whose build dir never finished a native run: stale, naming the missing record.
    #[test]
    fn a_build_dir_without_a_record_is_stale() {
        let env = Env::new("norecord");
        let why = build_dir_staleness(&env.wt)
            .unwrap()
            .expect("no record must be stale");
        assert!(why.contains("no record"), "{why}");
    }

    /// Two lands back to back: the first makes the land worktree, the second finds it detached at the first's commit and reuses it (its build dir is the one the first published).
    #[test]
    fn a_second_land_reuses_the_land_worktree() {
        let env = Env::new("twice");
        let first = git(Some(&env.wt), &["rev-parse", "HEAD"]).unwrap();
        let plan = Plan::new(&env.repo, "task", true, &in_process).unwrap();
        let dir = BuildDir::before_rebase();
        assert_eq!(dir.run(&plan, &plan.stages()), "land: ok (7 steps)");
        assert_eq!(env.land_head(), (None, first.clone()));
        // the mock cleanup left the branch's worktree: remove it as the real child would
        let wt_arg = env.wt.display().to_string();
        git(Some(&env.repo), &["worktree", "remove", "--force", &wt_arg]).unwrap();
        git(Some(&env.repo), &["branch", "-D", "task"]).unwrap();
        let wt2 = env.root.join("ws").join("wt").join("task2");
        let wt2_arg = wt2.display().to_string();
        git(
            Some(&env.repo),
            &["worktree", "add", "-q", "-b", "task2", &wt2_arg],
        )
        .unwrap();
        write(&wt2, "second.txt", "2\n");
        commit(&wt2, "second");
        let second = git(Some(&wt2), &["rev-parse", "HEAD"]).unwrap();
        let plan = Plan::new(&env.repo, "task2", true, &in_process).unwrap();
        let dir = BuildDir::before_rebase();
        assert_eq!(dir.run(&plan, &plan.stages()), "land: ok (7 steps)");
        assert!(
            !dir.calls.borrow().iter().any(|c| c.starts_with("recopy")),
            "a green land re-copied"
        );
        assert_eq!(env.land_head(), (None, second.clone()));
        assert_eq!(env.origin_main(), second);
    }

    /// The preflight refuses a land worktree left on a branch (a running or crashed land) and a branch named like the land worktree.
    #[test]
    fn an_unready_land_worktree_is_refused() {
        let env = Env::new("unready");
        let land_arg = env.land.display().to_string();
        git(
            Some(&env.repo),
            &["worktree", "add", "-q", "-b", "other", &land_arg],
        )
        .unwrap();
        let err = Plan::new(&env.repo, "task", true, &in_process)
            .err()
            .expect("a land worktree on a branch must be refused");
        assert!(format!("{err:#}").contains("is on other"), "{err:#}");
        git(Some(&env.land), &["checkout", "-q", "--detach"]).unwrap();
        Plan::new(&env.repo, "task", true, &in_process).expect("detached and clean: ready");
        let err = Plan::new(&env.repo, out::LAND_ID, true, &in_process)
            .err()
            .expect("a branch named land must be refused");
        assert!(format!("{err:#}").contains("rename the branch"), "{err:#}");
    }

    /// Temp tree: `origin.git` (bare, the remote), `ws/repo` (the main checkout on main, pushed) and `ws/wt/task` (a worktree on `task`, one commit ahead); `ws/wt/land` is where the land worktree goes. No upstreams, so `check` has nothing to fetch.
    struct Env {
        root: PathBuf,
        origin: PathBuf,
        repo: PathBuf,
        wt: PathBuf,
        land: PathBuf,
    }

    impl Env {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("xtask-land-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let origin = root.join("origin.git");
            let repo = root.join("ws").join("repo");
            let wt = root.join("ws").join("wt").join("task");
            let land = root.join("ws").join(LAND_WT);
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
            // the facts read the build machine's host row: the repo's own table, which lists every machine xtask runs on
            let arches = std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../arches.toml"),
            )
            .unwrap();
            write(&repo, "arches.toml", &arches);
            commit(&repo, "base");
            let origin_arg = origin.display().to_string();
            git(Some(&repo), &["remote", "add", "origin", &origin_arg]).unwrap();
            git(Some(&repo), &["push", "-q", "origin", "main"]).unwrap();
            git(Some(&repo), &["fetch", "-q", "origin"]).unwrap();
            let wt_arg = wt.display().to_string();
            git(
                Some(&repo),
                &["worktree", "add", "-q", "-b", "task", &wt_arg],
            )
            .unwrap();
            write(&wt, "change.txt", "task\n");
            commit(&wt, "task");
            Env {
                root,
                origin,
                repo,
                wt,
                land,
            }
        }

        fn origin_main(&self) -> String {
            git(Some(&self.origin), &["rev-parse", "refs/heads/main"]).unwrap()
        }

        /// The land worktree's branch (None = detached) and commit.
        fn land_head(&self) -> (Option<String>, String) {
            (
                git(Some(&self.land), &["symbolic-ref", "-q", "--short", "HEAD"]).ok(),
                git(Some(&self.land), &["rev-parse", "HEAD"]).unwrap(),
            )
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
        let plan = Plan::new(&env.repo, "task", false, &in_process).unwrap();
        let stages = plan.stages();
        let names: Vec<&str> = stages.iter().map(Stage::name).collect();
        assert_eq!(
            names,
            [
                "enter", "build", "verify", "push", "sync", "publish", "cleanup"
            ]
        );
        let lines = plan.describe(&stages);
        assert!(
            lines[3].contains("refs/heads/task:refs/heads/main"),
            "{}",
            lines[3]
        );
        assert!(lines[6].contains("branch task"), "{}", lines[6]);
        assert_eq!(env.origin_main(), before);
        assert!(env.wt.is_dir());
        assert!(!env.land.exists(), "a dry run made the land worktree");
    }

    #[test]
    fn branch_behind_origin_main_is_refused() {
        let env = Env::new("nonff");
        env.advance_origin();
        let moved = env.origin_main();
        let err = Plan::new(&env.repo, "task", true, &in_process)
            .err()
            .expect("a branch behind origin/main must be refused");
        assert!(format!("{err:#}").contains("not a fast-forward"), "{err:#}");
        assert_eq!(env.origin_main(), moved);
    }

    #[test]
    fn failing_check_stops_before_the_push() {
        let env = Env::new("failing");
        let before = env.origin_main();
        let plan = Plan::new(&env.repo, "task", true, &in_process).unwrap();
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
        let publish_fn =
            |_: &Path| -> Result<String> { panic!("a base was published after a failed check") };
        let recopied = std::cell::RefCell::new(0);
        let recopy_fn = |_: &Path| -> Result<String> {
            *recopied.borrow_mut() += 1;
            Ok("re-copied".to_owned())
        };
        let line = plan.execute(
            &stages,
            &Runner {
                edit: &no_edit,
                check: &check_fn,
                fetch: &fetch_fn,
                cleanup: &cleanup_fn,
                native: &no_native,
                stale: &current,
                publish: &publish_fn,
                recopy: &recopy_fn,
            },
        );
        assert!(
            line.starts_with(
                "land: FAILED 1 of 7 steps: verify; first error (verify): \"nothing pushed"
            ),
            "{line}"
        );
        assert_eq!(env.origin_main(), before);
        assert!(env.wt.is_dir());
        assert_eq!(*recopied.borrow(), 1);
        assert_eq!(env.land_head(), (None, before));
    }

    #[test]
    fn passing_checks_land_and_clean_up() {
        let env = Env::new("landed");
        let head = git(Some(&env.wt), &["rev-parse", "HEAD"]).unwrap();
        let plan = Plan::new(&env.repo, "task", true, &in_process).unwrap();
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
                edit: &no_edit,
                check: &check_fn,
                fetch: &fetch_fn,
                cleanup: &cleanup_fn,
                native: &no_native,
                stale: &current,
                publish: &no_publish,
                recopy: &no_recopy,
            },
        );
        assert_eq!(line, "land: ok (7 steps)");
        assert_eq!(*children.borrow(), [(env.repo.clone(), "task".to_owned())]);
        assert_eq!(env.origin_main(), head);
        assert_eq!(git(Some(&env.repo), &["rev-parse", "HEAD"]).unwrap(), head);
        assert!(!env.wt.exists(), "the worktree is not removed");
        assert!(
            git(
                Some(&env.repo),
                &["rev-parse", "--verify", "-q", "refs/heads/task"]
            )
            .is_err(),
            "the branch is not removed"
        );
        assert_eq!(
            env.land_head(),
            (None, head),
            "land worktree stays, detached"
        );
    }

    #[test]
    fn cleanup_removes_the_task_copies_from_main() {
        let env = Env::new("copies");
        // an upstream checkout `src/up` and its task copy `src-wt/task/up`, as `patch --edit` leaves them
        let up = env.root.join("ws").join("src").join("up");
        std::fs::create_dir_all(&up).unwrap();
        git(Some(&up), &["init", "-q"]).unwrap();
        write(&up, "a.c", "int a;\n");
        commit(&up, "up");
        let pin = git(Some(&up), &["rev-parse", "HEAD"]).unwrap();
        let copy = env.root.join("ws").join("src-wt").join("task").join("up");
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
            patch::done_task(&env.repo, "task", vec!["up".to_owned()]).unwrap(),
            0
        );
        assert!(!copy.exists(), "the task copy is kept");
        assert!(up.is_dir(), "the shared checkout went with the copy");
    }

    #[test]
    fn clean_up_removes_the_checkouts_target_and_its_build_dir() {
        let env = Env::new("target");
        let ws = env.root.join("ws");
        let dir = out::dir_of(&ws, "task");
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        write(&dir.join("target/debug"), "build.txt", "x\n");
        std::os::unix::fs::symlink(dir.join("target"), env.wt.join("target")).unwrap();
        // `branch -d` in clean_up refuses a branch that is not merged into the checked-out main
        git(Some(&env.repo), &["merge", "-q", "--ff-only", "task"]).unwrap();
        let summary = clean_up(&env.repo, &ws, &env.wt, "task").expect("clean up");
        assert!(summary.contains("removed:"), "{summary}");
        assert!(!dir.exists(), "the build dir leaves with its checkout");
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

    /// The whole branch name matches, and the land worktree (on the branch during a land) is never the branch's worktree.
    #[test]
    fn worktree_lookup_matches_the_whole_branch_name() {
        let list = "worktree /a/repo\nHEAD 1\nbranch refs/heads/main\n\nworktree /a/wt/land\nHEAD 2\nbranch refs/heads/task\n\nworktree /a/wt/task\nHEAD 2\nbranch refs/heads/task\n\nworktree /a/wt/tasking\nHEAD 3\nbranch refs/heads/tasking\n";
        let land = Path::new("/a/wt/land");
        assert_eq!(
            find_worktree(list, "task", land),
            Some(PathBuf::from("/a/wt/task"))
        );
        assert_eq!(
            find_worktree(list, "tasking", land),
            Some(PathBuf::from("/a/wt/tasking"))
        );
        assert_eq!(find_worktree(list, "tas", land), None);
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
