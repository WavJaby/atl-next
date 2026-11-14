// SPDX-License-Identifier: MIT
//! `xtask land <branch>`: puts a task branch on origin/main, with the branch's own patches checked before the push.
//! Why this order: `src/<up>` is one checkout shared by every worktree and holds origin/main's pins and patches (`fetch::Main`), so a branch's unlanded patches exist only in its task copies (`src-wt/<branch>/<up>`), which only the branch's worktree resolves (`atl-layout`).
//! Main has no task copies, so `check` run on main sees `src/` alone. That is why the branch is checked in its own worktree before the push, and why its copies go only after main and `src/` reached the new commit.
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
use crate::patch;
use crate::slot;
use crate::slot_prune;
use crate::sources::{self, Sources};
use crate::timing::{self, Timings};
use crate::unit_use::{self, Echo, Units};
use crate::verdict::{self, Failure, Verdict};
use crate::ws_config;

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
    /// Internal: the preflight's child in the branch's worktree (`facts_child`); prints what the branch's tables say, read-only.
    #[arg(long, hide = true, conflicts_with_all = ["execute", "cleanup"])]
    facts: bool,
    /// Internal: the inputs child (`inputs_child`) in the worktree or the main checkout; prints what that checkout's check reads (`check_inputs`), read-only.
    #[arg(long, hide = true, conflicts_with_all = ["execute", "cleanup", "facts"])]
    inputs: bool,
}

/// What the branch's own tables say, as the branch's xtask reads them (`branch_facts`).
#[derive(Debug, Default, PartialEq, Eq)]
struct BranchFacts {
    /// Upstreams needing a task copy in the worktree.
    edits: Vec<String>,
    /// Upstreams whose task copy exists already.
    copies: Vec<String>,
    /// The build machine's arches.toml host: what verify-main builds the main checkout's slot for.
    host: String,
}

/// What a checkout's `check` reads from the source side, as `check_inputs` hashes it: `.` = the checkout's tracked files, then each upstream dir as the checkout's layout resolves it (task copy or `src/`) -> its tree, or [`ABSENT`].
type Inputs = std::collections::BTreeMap<String, String>;

/// An upstream not fetched where the layout resolves it.
const ABSENT: &str = "absent";

/// `(worktree, branch)` -> the branch's facts; `facts_child` in production.
type FactsFn<'a> = dyn Fn(&Path, &str) -> Result<BranchFacts> + 'a;

/// What `land` runs beside its git work: the check of a checkout, the fetch of the main checkout, the branch's task copies and the cleanup child. Tests substitute them.
struct Runner<'a> {
    /// `xtask patch --edit <up>` in the worktree, by the branch's xtask (`edit_execute`).
    edit: &'a dyn Fn(&Path, &str) -> Result<String>,
    /// The checkout's verdict line, Ok only when that check passed (`check_verdict`).
    check: &'a dyn Fn(&Path) -> Result<String>,
    /// `xtask fetch --execute` in the main checkout.
    fetch: &'a dyn Fn(&Path) -> Result<()>,
    /// The cleanup child for the main checkout and the branch; its note line (`cleanup_execute`).
    cleanup: &'a dyn Fn(&Path, &str) -> Result<String>,
    /// `(checkout, branch)` -> what that checkout's check reads, by its own xtask (`inputs_child`).
    inputs: &'a dyn Fn(&Path, &str) -> Result<Inputs>,
    /// `(main checkout, host)` -> `xtask native --for all-gates` there, by its own xtask (`native_execute`): verify-main's slot rebuild.
    native: &'a dyn Fn(&Path, &str) -> Result<String>,
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
    /// The main checkout's verdict: Verify's, when main's check inputs equal the ones Verify checked (`Plan::verify_main`); else `xtask native` + `cargo xtask check` there.
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
    if args.inputs {
        for (name, tree) in check_inputs(root)? {
            println!("{INPUT}{name} {tree}");
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
        inputs: &inputs_child,
        native: &native_execute,
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
        let BranchFacts {
            edits,
            copies,
            host,
        } = facts(&wt, branch)?;
        Ok(Plan {
            repo: repo.to_path_buf(),
            branch: branch.to_owned(),
            wt,
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
            Stage::VerifyMain => format!(
                "verify in {repo}: verify's verdict if its check inputs equal the worktree's, else `cargo xtask native --host {} --for {}` then `cargo xtask check` must print `check: ok`",
                self.host,
                crate::gate_goals::ALL
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
        // what Verify's check read; None until it passed and was read
        let mut verified: Option<Inputs> = None;
        for stage in stages {
            let started = Instant::now();
            let result = self.run_stage(stage, runner, &mut verified);
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

    fn run_stage(
        &self,
        stage: &Stage,
        runner: &Runner,
        verified: &mut Option<Inputs>,
    ) -> Result<String> {
        match stage {
            Stage::Edit(up) => (runner.edit)(&self.wt, up),
            Stage::Verify => {
                let line = (runner.check)(self.wt.as_path()).with_context(|| {
                    format!("nothing pushed: check in {} failed", self.wt.display())
                })?;
                // read after the check: nothing in land writes the worktree or its task copies between the two
                Ok(match (runner.inputs)(&self.wt, &self.branch) {
                    Ok(inputs) => {
                        let note = format!("{line}; {} check inputs read", inputs.len());
                        *verified = Some(inputs);
                        note
                    }
                    Err(e) => format!(
                        "{line}; check inputs not read, verify-main will check main in full: {e:#}"
                    ),
                })
            }
            Stage::Push => {
                let refspec = format!("refs/heads/{}:refs/heads/main", self.branch);
                git(Some(&self.repo), &["push", "-q", "origin", &refspec])
                    .context("nothing pushed: git push refused")?;
                Ok(format!("origin/main is {}", short(&self.head)))
            }
            Stage::Sync => self.sync(runner),
            Stage::VerifyMain => self.verify_main(runner, verified.as_ref()),
            Stage::Cleanup => (runner.cleanup)(&self.repo, &self.branch)
                .context("pushed and checked, but cleanup failed: finish it by hand"),
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

    /// The main checkout's verdict.
    /// Invariant for the skip: `check` is a function of the checkout's tracked files (its own code included), the upstream trees its layout resolves, the slot's build and the workspace's shared config/caches.
    /// The slot's build is in turn derived from the first two by `native`; the shared part is the same files for both checkouts. So when main's tracked files and resolved upstream trees (`check_inputs`)
    /// equal what Verify's check read in the worktree (task copies included), main's check would reach Verify's verdict: it is recorded, and neither main's slot rebuild nor its check runs.
    /// Main's slot stays as it was; its gates refuse it by name until the next `native` there (build_state::ensure_current). Not covered: untracked or ignored files a check reads (both checkouts are clean, the state note aside).
    /// Inputs that differ (fetch built `src/<up>` other than the task copy, an unexported edit, Verify's inputs unread) -> native for every gate's goals (a stale slot failed "built from another source state" otherwise), then check.
    fn verify_main(&self, runner: &Runner, verified: Option<&Inputs>) -> Result<String> {
        let pushed = || {
            format!(
                "pushed: origin/main is {}, and the check in {} failed",
                short(&self.head),
                self.repo.display()
            )
        };
        let why = match verified.map(|v| (v, (runner.inputs)(&self.repo, &self.branch))) {
            None => "the worktree's check inputs were not read".to_owned(),
            Some((v, Ok(now))) if now == *v => {
                return Ok(format!(
                    "verify's verdict: main's {} check inputs equal the worktree's (slot not rebuilt, check not run)",
                    now.len()
                ));
            }
            Some((v, Ok(now))) => format!("check inputs differ: {}", differing(v, &now)),
            Some((_, Err(e))) => format!("main's check inputs not read: {e:#}"),
        };
        println!("land: verify-main: {why}: rebuilding main's slot, then checking");
        (runner.native)(&self.repo, &self.host).with_context(pushed)?;
        let line = (runner.check)(self.repo.as_path()).with_context(pushed)?;
        Ok(format!("{line} ({why})"))
    }
}

/// The first few names whose trees differ between `a` and `b`, either side's absence included.
fn differing(a: &Inputs, b: &Inputs) -> String {
    let names: Vec<&str> = a
        .keys()
        .chain(b.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(String::as_str)
        .collect();
    format!("{} ({} in all)", first(&names), names.len())
}

/// What `check` in the checkout at `root` reads from the source side (`Inputs`), each tree as its tracked files are on disk (build_state::tracked_tree).
/// Every upstream sources.toml lists (the AOSP manifest included), located as this checkout's layout locates it: a branch worktree sees its task copies, main sees `src/`.
fn check_inputs(root: &Path) -> Result<Inputs> {
    let layout = Layout::of(root)?;
    let sources = sources::load(root)?;
    let mut ups = sources.checkouts(None);
    ups.push(sources.manifest_checkout());
    let dirs: Vec<(String, PathBuf)> = ups
        .iter()
        .map(|c| (c.dir.display().to_string(), layout.locate(&c.dir).dir))
        .collect();
    let workers = std::thread::available_parallelism().map_or(1, usize::from);
    let trees = crate::par::map(&dirs, workers, |(name, dir)| -> Result<String> {
        if !dir.exists() {
            return Ok(ABSENT.to_owned());
        }
        crate::build_state::tracked_tree(dir).with_context(|| format!("reading upstream {name}"))
    });
    let mut out = Inputs::new();
    out.insert(".".to_owned(), crate::build_state::tracked_tree(root)?);
    for ((name, _), tree) in dirs.into_iter().zip(trees) {
        out.insert(name, tree?);
    }
    Ok(out)
}

/// `--facts` output line prefixes.
const FACT_EDIT: &str = "land-facts: edit ";
const FACT_COPY: &str = "land-facts: copy ";
const FACT_HOST: &str = "land-facts: host ";
/// `--inputs` output line prefix: `<name> <tree>` follows.
const INPUT: &str = "land-inputs: ";

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

/// `cargo xtask land <branch> --inputs` in `dir` (the worktree or the main checkout), run by that checkout's own xtask: both sides hash with the code being landed.
fn inputs_child(dir: &Path, branch: &str) -> Result<Inputs> {
    let mut cmd = crate::cargo_command();
    cmd.args(["xtask", "land", branch, "--inputs"])
        .current_dir(dir);
    let run = unit_use::run(&mut cmd, Units::Plain, Echo::Nothing)
        .with_context(|| format!("running cargo xtask land --inputs in {}", dir.display()))?;
    ensure!(
        run.status.success(),
        "{}: {}",
        run.status,
        run.stderr
            .rfind("Error:")
            .map_or(run.stderr.trim_end(), |at| run.stderr[at..].trim_end())
    );
    let inputs = parse_inputs(&run.stdout);
    ensure!(!inputs.is_empty(), "the inputs child printed no input");
    Ok(inputs)
}

fn parse_inputs(stdout: &str) -> Inputs {
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix(INPUT)?.rsplit_once(' '))
        .map(|(name, tree)| (name.to_owned(), tree.to_owned()))
        .collect()
}

/// `cargo xtask native --host <host> --for all-gates` in the main checkout, run by its own xtask: the slot verify-main's check reads, built from main as it is now.
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
    let wt = find_worktree(&porcelain, branch)
        .with_context(|| format!("branch {branch} is not checked out in a worktree"))?;
    let ws = Layout::of(repo)?.workspace().to_path_buf();
    let note = clean_up(repo, &ws, &wt, branch)?;
    prune_slots_after_land(repo, &ws);
    Ok(note)
}

/// After a land: `slot prune --execute` over the pool's slots. The trees no slot owns (`repo/target`, EGL probes) are not pruned here: `slot prune --all` does that by hand. A busy slot is skipped; a failure is logged, never fatal, since the land is already pushed.
fn prune_slots_after_land(repo: &Path, ws: &Path) {
    let swept = slot::holders_of(repo).and_then(|holders| {
        let pool = slot::pool(&ws_config::load(ws)?, &holders)?;
        let slots: Vec<u32> = (1..=pool).collect();
        slot_prune::sweep(
            ws,
            &holders,
            &slots,
            false,
            slot_prune::PRUNE_AFTER_DAYS,
            true,
        )
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
        let plan = Plan::new(&env.repo, "land", true, &facts)
            .expect("main's binary must not parse the branch's tables");
        assert_eq!(*asked.borrow(), [(env.wt.clone(), "land".to_owned())]);
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
                inputs: &same_inputs,
                native: &no_native,
            },
        );
        assert_eq!(line, "land: ok (5 steps)");
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

    /// Every checkout reads the same inputs.
    fn same_inputs(_: &Path, _: &str) -> Result<Inputs> {
        Ok(Inputs::from([(".".to_owned(), "t".to_owned())]))
    }

    fn no_native(_: &Path, host: &str) -> Result<String> {
        panic!("verify-main rebuilt main's slot for {host}")
    }

    /// Main's inputs equal the worktree's: verify-main records Verify's verdict, no rebuild, no second check. Inputs read by this code in-process, as both children would.
    #[test]
    fn equal_inputs_skip_the_main_check() {
        let env = Env::new("skip");
        let plan = Plan::new(&env.repo, "land", true, &in_process).unwrap();
        let checked = std::cell::RefCell::new(Vec::new());
        let check_fn = |d: &Path| -> Result<String> {
            checked.borrow_mut().push(d.to_path_buf());
            Ok("check: ok (12 steps)".to_owned())
        };
        let fetch_fn = |_: &Path| -> Result<()> { Ok(()) };
        let cleanup_fn = |_: &Path, _: &str| -> Result<String> { Ok("cleaned".to_owned()) };
        let inputs_fn = |d: &Path, _: &str| check_inputs(d);
        let line = plan.execute(
            &plan.stages(),
            &Runner {
                edit: &no_edit,
                check: &check_fn,
                fetch: &fetch_fn,
                cleanup: &cleanup_fn,
                inputs: &inputs_fn,
                native: &no_native,
            },
        );
        assert_eq!(line, "land: ok (5 steps)");
        assert_eq!(
            *checked.borrow(),
            std::slice::from_ref(&env.wt),
            "main was checked"
        );
    }

    /// Main's inputs differ (an upstream fetch built otherwise than the task copy): verify-main rebuilds main's slot for the branch's host, then checks main; the note names what differed.
    #[test]
    fn differing_inputs_rebuild_and_check_main() {
        let env = Env::new("differ");
        let plan = Plan::new(&env.repo, "land", true, &in_process).unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        let check_fn = |d: &Path| -> Result<String> {
            calls.borrow_mut().push(format!("check {}", d.display()));
            Ok("check: ok (12 steps)".to_owned())
        };
        let native_fn = |d: &Path, host: &str| -> Result<String> {
            calls
                .borrow_mut()
                .push(format!("native {host} {}", d.display()));
            Ok("built".to_owned())
        };
        let inputs_fn = |d: &Path, _: &str| -> Result<Inputs> {
            let tree = if d == env.wt { "copy" } else { "fetched" };
            Ok(Inputs::from([
                (".".to_owned(), "t".to_owned()),
                ("aosp/art".to_owned(), tree.to_owned()),
            ]))
        };
        let fetch_fn = |_: &Path| -> Result<()> { Ok(()) };
        let cleanup_fn = |_: &Path, _: &str| -> Result<String> { Ok("cleaned".to_owned()) };
        let line = plan.execute(
            &plan.stages(),
            &Runner {
                edit: &no_edit,
                check: &check_fn,
                fetch: &fetch_fn,
                cleanup: &cleanup_fn,
                inputs: &inputs_fn,
                native: &native_fn,
            },
        );
        assert_eq!(line, "land: ok (5 steps)");
        assert_eq!(
            *calls.borrow(),
            [
                format!("check {}", env.wt.display()),
                format!("native {} {}", plan.host, env.repo.display()),
                format!("check {}", env.repo.display()),
            ]
        );
        assert_eq!(
            differing(
                &inputs_fn(&env.wt, "land").unwrap(),
                &Inputs::from([(".".to_owned(), "t".to_owned())])
            ),
            "aosp/art (1 in all)"
        );
        assert_eq!(
            parse_inputs("noise\nland-inputs: . abc\nland-inputs: aosp/a b c0\n"),
            Inputs::from([
                (".".to_owned(), "abc".to_owned()),
                ("aosp/a b".to_owned(), "c0".to_owned())
            ])
        );
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
        let plan = Plan::new(&env.repo, "land", false, &in_process).unwrap();
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
        let err = Plan::new(&env.repo, "land", true, &in_process)
            .err()
            .expect("a branch behind origin/main must be refused");
        assert!(format!("{err:#}").contains("not a fast-forward"), "{err:#}");
        assert_eq!(env.origin_main(), moved);
    }

    #[test]
    fn failing_check_stops_before_the_push() {
        let env = Env::new("failing");
        let before = env.origin_main();
        let plan = Plan::new(&env.repo, "land", true, &in_process).unwrap();
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
        let inputs_fn = |d: &Path, _: &str| -> Result<Inputs> {
            panic!("inputs of {} read after a failed check", d.display())
        };
        let line = plan.execute(
            &stages,
            &Runner {
                edit: &no_edit,
                check: &check_fn,
                fetch: &fetch_fn,
                cleanup: &cleanup_fn,
                inputs: &inputs_fn,
                native: &no_native,
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
        let plan = Plan::new(&env.repo, "land", true, &in_process).unwrap();
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
                inputs: &same_inputs,
                native: &no_native,
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
