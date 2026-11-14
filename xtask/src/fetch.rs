// SPDX-License-Identifier: MIT
//! `xtask fetch`: upstream checkouts at the pins in sources.toml, with `patches/<upstream>/` applied
//! on the checkout's patch branch -- each checkout where the layout resolves it: `src/<up>` (only
//! while this repo checkout's pin and patches are origin/main's, `Main`) or the task copy.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;

use crate::aosp::{self, Manifest};
use crate::git::{Ident, git, git_raw};
use crate::licenses::{self, Licenses};
use crate::sources::{self, Checkout, Pin, Sources};
use crate::{par, patch, tree};

/// The trunk as fetch compares against it: what `src/` holds.
pub const MAIN_REF: &str = "origin/main";

/// What `src/` holds: origin/main's pins and patches. Fetch (and `xtask patch --rebase`) change a shared checkout only for a repo checkout whose pin and patches/<up>/ equal these; a task's own go to its task copy (`xtask patch --edit`), so no worktree rewrites what the others build from.
#[derive(Default)]
pub struct Main {
    pins: BTreeMap<PathBuf, String>,
    patches: BTreeMap<PathBuf, Vec<(String, String)>>,
}

impl Main {
    pub fn load(root: &Path) -> Result<Self> {
        let show = |path: &str| git_raw(root, &["show", &format!("{MAIN_REF}:{path}")]);
        let text = show(sources::FILE).with_context(|| {
            format!(
                "reading {MAIN_REF}:{} (fetch compares against it)",
                sources::FILE
            )
        })?;
        let sources =
            sources::parse(&text).with_context(|| format!("in {MAIN_REF}:{}", sources::FILE))?;
        let mut pins: BTreeMap<PathBuf, String> = sources
            .checkouts(None)
            .into_iter()
            .map(|c| (c.dir, c.commit))
            .collect();
        let m = sources.manifest_checkout();
        pins.insert(m.dir, m.commit);
        let mut patches: BTreeMap<PathBuf, Vec<(String, String)>> = BTreeMap::new();
        let files = git(
            Some(root),
            &["ls-tree", "-r", "--name-only", MAIN_REF, "--", "patches"],
        )?;
        for file in files.lines().filter(|f| f.ends_with(".patch")) {
            let p = Path::new(file);
            if let (Some(dir), Some(name)) = (
                p.parent().and_then(|d| d.strip_prefix("patches").ok()),
                p.file_name(),
            ) {
                patches
                    .entry(dir.to_path_buf())
                    .or_default()
                    .push((name.to_string_lossy().into_owned(), show(file)?));
            }
        }
        for list in patches.values_mut() {
            list.sort();
        }
        Ok(Main { pins, patches })
    }

    /// Why the shared checkout of `c` must stay as it is for a repo checkout whose patches/<up>/ is `stored`: None when pin and patches are main's, or main has no such row (a new checkout nothing on main reads).
    pub fn refusal(&self, c: &Checkout, stored: &[(String, String)]) -> Option<String> {
        let pin = self.pins.get(&c.dir)?;
        let main = self.patches.get(&c.dir).map_or(&[][..], Vec::as_slice);
        let what = match (*pin != c.commit, stored != main) {
            (false, false) => return None,
            (true, false) => "pin",
            (false, true) => "patches/",
            (true, true) => "pin and patches/",
        };
        let how = match c.pin {
            Pin::Manifest => "merge the bump to main first".to_owned(),
            _ => format!("xtask patch --edit {} for a task copy", c.dir.display()),
        };
        Some(format!(
            "this checkout's {what} differ from {MAIN_REF}'s, and src/ holds main's: {how}"
        ))
    }

    /// Whether origin/main has a row for `c`. A row it lacks is one nothing on main reads, so `refusal` allows it.
    pub fn lists(&self, c: &Checkout) -> bool {
        self.pins.contains_key(&c.dir)
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// Clone missing checkouts and move clean detached ones to their pin, after checking on the remote
    /// that each tag still resolves to its pinned commit; put each upstream with patches on branch
    /// atl-patches = pin + patches/<upstream>/ (git am), refusing to drop commits patches/ lacks; then
    /// create the AOSP manifest's link/copy files.
    #[arg(long, conflicts_with = "bump")]
    execute: bool,
    /// Resolve every tag on its remote and show where the pins would move (network, read-only).
    #[arg(long)]
    bump: bool,
    /// With --bump: write the resolved commits into sources.toml.
    #[arg(long, requires = "bump")]
    write: bool,
    /// Pin AOSP projects (tree paths of the fetched manifest) at the commit the tag resolves to: adds
    /// their sources.toml rows, then goes on as without it. Already pinned paths are skipped.
    #[arg(long, value_name = "PATH", num_args = 1.., conflicts_with = "bump")]
    add: Vec<String>,
    /// Parallel git processes (default: the AOSP manifest's `sync-j`).
    #[arg(long)]
    jobs: Option<usize>,
    /// Move every checkout's in-tree `.git` dir to `src/.git-dirs/<dir>.git` and repair its task copies (one-time, per workspace; dry run unless --execute). Refuses while a build token is held,
    /// a process has its cwd under src/ or src-wt/ or is inside a task-copy namespace, or a git lock exists. Resumable: run it again after a crash.
    #[arg(long, conflicts_with_all = ["bump", "add"])]
    migrate_git_dirs: bool,
    /// Instead of the checkouts: every test APK of test-apks.toml into `<workspace>/cache/test-apks`, verified (size, sha256, signing certificate). Dry run unless --execute, which downloads what is missing or damaged.
    #[arg(long, conflicts_with_all = ["bump", "add", "migrate_git_dirs"])]
    test_apks: bool,
    /// With --test-apks: also the google-play rows, fetched by apkeep with the Play token (tier 1: Secret Service; tier 2: stdin). Needs --play-email. Without it those rows are listed as skipped.
    #[arg(long, requires_all = ["test_apks", "play_email"])]
    play: bool,
    /// With --test-apks --play: the Google account apkeep signs in with. This flag is the only way to give it.
    #[arg(long, value_name = "EMAIL", requires = "play")]
    play_email: Option<String>,
    /// With --test-apks: store the Play token read from stdin in the Secret Service (`secret-tool store`) and exit. Nothing is downloaded.
    #[arg(
        long,
        requires = "test_apks",
        conflicts_with_all = ["play", "execute"]
    )]
    store_token: bool,
    /// Delete and clone again the checkouts of these pinned AOSP projects (tree paths) whose keep-set differs from sources.toml [aosp.sparse] in a way fetch cannot apply in place (a shrink, sparse <-> full; adding dirs at the pin needs no flag). Only a checkout at its pin with no uncommitted changes; the remote must still resolve the pin. Dry run unless --execute.
    #[arg(long, value_name = "PATH", num_args = 1.., conflicts_with_all = ["bump", "add", "migrate_git_dirs", "test_apks"])]
    recreate: Vec<String>,
    /// Also fetch the sources.toml [aosp.needed_by] projects of these gates (`native --for` names, `all-gates`); with --execute, adds them to `<workspace>/workspace.toml` [fetch] for, which keeps them requested. Without either, fetch checks out core only.
    #[arg(long = "for", value_name = "GATE", num_args = 1.., value_delimiter = ',', conflicts_with_all = ["bump", "migrate_git_dirs", "test_apks"])]
    for_gates: Vec<String>,
    /// Delete these checkouts (dirs under src/, as fetch lists them): only one that is not pinned, or pinned but not requested by [fetch] for; refused while it has a task copy, uncommitted changes or a local branch. Dry run unless --execute.
    #[arg(long = "drop", value_name = "DIR", num_args = 1.., conflicts_with_all = ["bump", "add", "migrate_git_dirs", "test_apks", "recreate", "for_gates"])]
    drop_dirs: Vec<String>,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.test_apks {
        if args.store_token {
            crate::test_apks::store_token()?;
        } else {
            // clap requires --play and --play-email together, so the email is there exactly when Play is asked for
            let play = match (args.play, args.play_email.as_deref()) {
                (true, Some(email)) => Some(email),
                _ => None,
            };
            crate::test_apks::fetch(root, args.execute, play)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    if args.migrate_git_dirs {
        let layout = Layout::of(root)?;
        let sources = sources::load(root)?;
        let mut dirs: Vec<PathBuf> = sources.checkouts(None).into_iter().map(|c| c.dir).collect();
        dirs.insert(0, sources.manifest_checkout().dir);
        let scope = crate::git_dirs::Scope {
            ws: layout.workspace(),
            dirs: &dirs,
        };
        crate::git_dirs::migrate(&scope, args.execute)?;
        return Ok(ExitCode::SUCCESS);
    }
    if !args.drop_dirs.is_empty() {
        return drop(root, &args.drop_dirs, args.execute).map(|ok| {
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        });
    }
    Ok(if fetch(root, args)? {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `xtask fetch --add <paths> --execute`: pin AOSP projects and check them out.
pub fn add_and_fetch(root: &Path, paths: &[String]) -> Result<()> {
    let args = Args {
        execute: true,
        bump: false,
        write: false,
        add: paths.to_vec(),
        jobs: None,
        migrate_git_dirs: false,
        test_apks: false,
        play: false,
        play_email: None,
        store_token: false,
        recreate: Vec::new(),
        for_gates: Vec::new(),
        drop_dirs: Vec::new(),
    };
    ensure!(fetch(root, &args)?, "fetch failed (rows above)");
    Ok(())
}

/// False when a checkout failed (its row says why).
fn fetch(root: &Path, args: &Args) -> Result<bool> {
    let layout = Layout::of(root)?;
    let mut sources = sources::load(root)?;
    for p in &args.recreate {
        ensure!(
            sources.aosp.projects.contains_key(p),
            "--recreate {p}: not a pinned [aosp.projects] path"
        );
    }
    let ctx = Ctx {
        root,
        layout: &layout,
        main: if args.bump {
            Main::default()
        } else {
            Main::load(root)?
        },
        licenses: licenses::load(root, &sources.upstream_names())?,
        // only `git am` needs it, and only then is its absence an error
        ident: Ident::of_repo(root).ok(),
        execute: args.execute,
        recreate: &args.recreate,
    };
    let mut rows = Rows::default();
    // the manifest first: it names every other AOSP project's repository
    if !args.bump {
        let c = sources.manifest_checkout();
        rows.add(&c, sync(&ctx, &c));
    }
    let manifest = aosp::load(root, &sources.aosp)?;
    let jobs = args
        .jobs
        .unwrap_or(manifest.as_ref().map_or(aosp::DEFAULT_SYNC_J, |m| m.sync_j));
    if !args.add.is_empty() {
        let m = manifest
            .as_ref()
            .context("--add needs the AOSP manifest (xtask fetch --execute)")?;
        add(root, &sources, m, &args.add, jobs)?;
        sources = sources::load(root)?;
    }
    let names = manifest.as_ref().map(Manifest::names);
    let all = sources.checkouts(names.as_ref());
    if args.bump {
        let mut all = all;
        all.insert(0, sources.manifest_checkout());
        return bump(root, &all, args.write, jobs);
    }
    let wanted = requested(layout.workspace(), &args.for_gates, args.execute)?;
    // an unrequested checkout already present stays in step (drift checks every present one); only a missing one is skipped
    let (all, skipped): (Vec<Checkout>, Vec<Checkout>) = all.into_iter().partition(|c| {
        sources.unrequested(c, &wanted.list).is_none() || layout.src().join(&c.dir).exists()
    });
    let results = par::map(&all, jobs, |c| sync(&ctx, c));
    for (c, result) in all.iter().zip(results) {
        rows.add(c, result);
    }
    println!("fetch: {} checkouts at pin", rows.at_pin);
    println!("fetch: for {}", wanted.shown);
    for c in &skipped {
        let gates = sources.unrequested(c, &wanted.list).unwrap_or_default();
        println!(
            "fetch: {:<40} skipped: not requested (needed_by [{}]; `fetch --for <gate>`)",
            c.dir.display(),
            gates.join(", ")
        );
    }
    for d in droppable(&layout, &sources, &wanted.list)? {
        println!(
            "fetch: {:<40} {}: `fetch --drop {0}` removes it",
            d.dir.display(),
            d.why
        );
    }
    if args.execute
        && let Some(m) = &manifest
    {
        let made = aosp::apply_links(&tree::shared_tree(&layout), m)?;
        println!("fetch: {made} manifest link/copy files created");
    } else if !args.execute {
        println!(
            "fetch: dry run ({} checkouts under {}); --execute applies",
            all.len() + 1,
            layout.src().display()
        );
    }
    Ok(rows.failed == 0)
}

/// The gates a fetch serves: workspace.toml `fetch.for` plus `--for` (written to the file only with `execute`).
struct Requested {
    list: Vec<String>,
    shown: String,
}

fn requested(ws: &Path, extra: &[String], execute: bool) -> Result<Requested> {
    if execute && !extra.is_empty() {
        crate::ws_config::add_fetch_for(ws, extra)?;
    }
    let cfg = crate::ws_config::load(ws)?;
    let mut list = cfg.fetch_for.value.clone();
    for g in extra {
        ensure!(
            crate::gate_goals::is_for_value(g),
            "--for {g}: not a gate (gate_goals.rs GATES) or `{}`",
            crate::gate_goals::ALL
        );
        if !list.contains(g) {
            list.push(g.clone());
        }
    }
    let mut shown = cfg.fetch_for_shown();
    if !execute && list != cfg.fetch_for.value {
        shown = format!(
            "[{}] (dry run: --execute writes --for to the file)",
            list.join(", ")
        );
    }
    Ok(Requested { list, shown })
}

/// A present checkout this workspace has no use for.
struct Droppable {
    /// Under src/, as `Checkout::dir`.
    dir: PathBuf,
    why: String,
}

/// Checkouts present under src/ that are not pinned, or pinned but not requested by `wanted`: what `--drop` may remove. Found by their git dirs (`src/.git-dirs/<dir>.git`, `Layout::git_dir`).
fn droppable(layout: &Layout, sources: &Sources, wanted: &[String]) -> Result<Vec<Droppable>> {
    let mut pinned: BTreeMap<PathBuf, Checkout> = sources
        .checkouts(None)
        .into_iter()
        .map(|c| (c.dir.clone(), c))
        .collect();
    let m = sources.manifest_checkout();
    pinned.insert(m.dir.clone(), m);
    let mut out = Vec::new();
    // the store root: parent of a top-level dir's git dir (`atl_layout::git_store` names no root of its own)
    let any = layout.git_dir(Path::new("x"));
    let store = any.parent().context("git store has no parent")?;
    for dir in stored_checkouts(store)? {
        match pinned.get(&dir) {
            None => out.push(Droppable {
                dir,
                why: "present, not pinned in sources.toml".into(),
            }),
            Some(c) => {
                if let Some(gates) = sources.unrequested(c, wanted) {
                    out.push(Droppable {
                        dir,
                        why: format!("present, not requested (needed_by [{}])", gates.join(", ")),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Checkout dirs whose git dir is under `store` (`<dir>.git` holding a HEAD), relative to it.
fn stored_checkouts(store: &Path) -> Result<Vec<PathBuf>> {
    fn walk(store: &Path, at: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        let here = store.join(at);
        let entries = match std::fs::read_dir(&here) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", here.display())),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            match name.strip_suffix(".git") {
                Some(dir) if entry.path().join("HEAD").is_file() => out.push(at.join(dir)),
                _ => walk(store, &at.join(&name), out)?,
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(store, Path::new(""), &mut out)?;
    out.sort();
    Ok(out)
}

/// `--drop`: each dir must be droppable (not pinned, or not requested) and pass [`drop_refusal`]; all are judged before any is removed.
fn drop(root: &Path, dirs: &[String], execute: bool) -> Result<bool> {
    let layout = Layout::of(root)?;
    let sources = sources::load(root)?;
    let wanted = crate::ws_config::load(layout.workspace())?.fetch_for.value;
    drop_in(&layout, &sources, &wanted, dirs, execute)
}

fn drop_in(
    layout: &Layout,
    sources: &Sources,
    wanted: &[String],
    dirs: &[String],
    execute: bool,
) -> Result<bool> {
    let free = droppable(layout, sources, wanted)?;
    let pinned: Vec<PathBuf> = sources.checkouts(None).into_iter().map(|c| c.dir).collect();
    let mut ok = true;
    let mut go = Vec::new();
    for d in dirs {
        let dir = PathBuf::from(d.trim_end_matches('/'));
        let verdict = match free.iter().find(|f| f.dir == dir) {
            None => Err(format!(
                "not a present checkout that is unpinned or unrequested (fetch lists those); requested: [{}]",
                wanted.join(", ")
            )),
            Some(f) => match drop_refusal(layout, &dir, &pinned)? {
                Some(why) => Err(why),
                None => Ok(f.why.clone()),
            },
        };
        match verdict {
            Ok(why) => {
                println!(
                    "fetch: {:<40} {why} -> {}",
                    dir.display(),
                    if execute { "dropped" } else { "would drop" }
                );
                go.push(dir);
            }
            Err(why) => {
                ok = false;
                println!("fetch: {:<40} drop refused: {why}", dir.display());
            }
        }
    }
    if !ok {
        println!("fetch: nothing dropped (refusals above)");
        return Ok(false);
    }
    if execute {
        for dir in &go {
            remove_checkout(&layout.src().join(dir), &layout.git_dir(dir))?;
        }
    } else {
        println!("fetch: dry run; --execute drops {} checkouts", go.len());
    }
    Ok(true)
}

/// Why checkout `dir` (under src/) must not be deleted: a pinned checkout inside it, a task copy of it, uncommitted changes, or a local branch (its commits may be the only copy). `--recreate`'s guards, less the pin it would clone again.
fn drop_refusal(layout: &Layout, dir: &Path, pinned: &[PathBuf]) -> Result<Option<String>> {
    if let Some(inner) = pinned.iter().find(|p| *p != dir && p.starts_with(dir)) {
        return Ok(Some(format!(
            "pinned checkout {} is inside it",
            inner.display()
        )));
    }
    // a task copy is a linked worktree: an entry under `<git dir>/worktrees`, whose `gitdir` file names the copy's `.git`
    let linked = layout.git_dir(dir).join("worktrees");
    let mut copies = Vec::new();
    if linked.is_dir() {
        for entry in
            std::fs::read_dir(&linked).with_context(|| format!("reading {}", linked.display()))?
        {
            let entry = entry?.path();
            let at = std::fs::read_to_string(entry.join("gitdir"))
                .unwrap_or_else(|_| entry.display().to_string());
            copies.push(at.trim().trim_end_matches("/.git").to_owned());
        }
    }
    if !copies.is_empty() {
        return Ok(Some(format!(
            "task copy {} exists: `xtask patch --done` first",
            copies.join(", ")
        )));
    }
    let tree = layout.src().join(dir);
    Ok(match inspect(&tree)? {
        Local::Missing => None,
        Local::Unborn { files: true } => {
            Some("git repository without commits holding files".into())
        }
        Local::Unborn { files: false } => None,
        Local::Present { dirty: true, .. } => Some("uncommitted changes".into()),
        Local::Present {
            branch: Some(b), ..
        } => Some(format!(
            "on local branch {b}: its commits may exist nowhere else"
        )),
        Local::Present { .. } => None,
    })
}

/// What every checkout's sync shares.
struct Ctx<'a> {
    root: &'a Path,
    layout: &'a Layout,
    main: Main,
    licenses: Licenses,
    ident: Option<Ident>,
    execute: bool,
    /// `--recreate` projects: their keep-set mismatch may be fixed by deleting the checkout.
    recreate: &'a [String],
}

/// One checkout's sync: what moving it to its pin needs, then what applying its patches needs.
struct Row {
    action: Action,
    patches: Option<String>,
}

/// What the checkout needs; with `execute`, also do it.
fn sync(ctx: &Ctx, c: &Checkout) -> Result<Row> {
    let u = patch::Up::new(c, ctx.layout, ctx.root);
    patch::shared_allowed(&u, &ctx.main)?;
    let dir = u.checkout();
    let local = inspect(dir)?;
    let mut action = decide(&local, &c.commit, u.branch());
    if let Some(why) = keep_mismatch(&local, &c.sparse) {
        action = if let Some(dirs) = keep_growth(&local, &c.sparse, &c.commit) {
            Action::Grow(dirs)
        } else if recreates(ctx, c, &local) {
            Action::Recreate(why)
        } else {
            Action::Blocked(why)
        };
    }
    if ctx.execute {
        apply(&action, c, dir, &ctx.layout.git_dir(&c.dir))?;
    }
    let patches = match action {
        Action::LeaveBranch(_) | Action::Blocked(_) => None,
        _ => patch::settle(&u, &ctx.licenses, ctx.ident.as_ref(), ctx.execute)?,
    };
    Ok(Row { action, patches })
}

/// Checkout rows: a clean one at its pin + patches is only counted, every other one is printed.
#[derive(Default)]
struct Rows {
    at_pin: usize,
    failed: usize,
}

impl Rows {
    fn add(&mut self, c: &Checkout, result: Result<Row>) {
        match result {
            Ok(Row {
                action: Action::AtPin { dirty: false } | Action::PatchBranch,
                patches: None,
            }) => self.at_pin += 1,
            Ok(Row { action, patches }) => println!(
                "fetch: {:<40} {}{}",
                c.dir.display(),
                action.describe(),
                patches.map(|p| format!("; {p}")).unwrap_or_default()
            ),
            Err(e) => {
                self.failed += 1;
                println!("fetch: {:<40} FAILED: {e:#}", c.dir.display());
            }
        }
    }
}

/// sources.toml rows for `paths` at the commit the tag resolves to in each; all or nothing.
fn add(
    root: &Path,
    sources: &Sources,
    manifest: &Manifest,
    paths: &[String],
    jobs: usize,
) -> Result<()> {
    let unique: BTreeSet<&String> = paths.iter().collect();
    let new: Vec<&String> = unique
        .iter()
        .filter(|p| !sources.aosp.projects.contains_key(**p))
        .copied()
        .collect();
    if new.is_empty() {
        println!("fetch: all {} paths already pinned", unique.len());
        return Ok(());
    }
    let resolved = par::map(&new, jobs, |path| {
        let project = manifest
            .projects
            .get(*path)
            .with_context(|| format!("{path}: not in the AOSP manifest"))?;
        remote_ref_commit(
            &sources.aosp_url(&project.name),
            &format!("refs/tags/{}", sources.aosp.tag),
        )
        .with_context(|| path.to_string())
    });
    let commits = resolved.into_iter().collect::<Result<Vec<_>>>()?;
    edit_sources(root, |doc| {
        let table = doc["aosp"]["projects"]
            .as_table_mut()
            .context("[aosp.projects] is not a table")?;
        for (path, commit) in new.iter().zip(&commits) {
            table.insert(path, toml_edit::value(commit.as_str()));
        }
        table.sort_values();
        Ok(())
    })?;
    println!(
        "fetch: pinned {} new AOSP projects ({} already pinned)",
        new.len(),
        unique.len() - new.len()
    );
    Ok(())
}

/// Rewrite sources.toml in place (comments and order kept), then re-validate it.
fn edit_sources(
    root: &Path,
    f: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
) -> Result<()> {
    let path = root.join(sources::FILE);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = text.parse().context("parsing sources.toml")?;
    f(&mut doc)?;
    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    sources::load(root).context("sources.toml after rewrite")?;
    Ok(())
}

// ---- local state and the action it calls for -------------------------------------------------

#[derive(Debug)]
enum Local {
    Missing,
    /// `.git` without a HEAD: a clone interrupted before its checkout (multi-GB prebuilts). `files` =
    /// anything beside `.git`, which then is not ours to delete.
    Unborn {
        files: bool,
    },
    Present {
        head: String,
        branch: Option<String>,
        dirty: bool,
        /// Cone-mode keep-set the checkout was created with; empty = full tree.
        sparse: Vec<String>,
    },
}

#[derive(Debug, PartialEq)]
enum Action {
    Clone,
    /// git cannot resume a cut-off fetch (it leaves `shallow.lock` and a partial pack): start over.
    Reclone,
    AtPin {
        dirty: bool,
    },
    MoveToPin,
    /// On the checkout's patch branch: `patch::settle` decides, wherever HEAD is.
    PatchBranch,
    /// On a branch that is not ours.
    LeaveBranch(String),
    /// Keep-set differs and `--recreate` names the project: delete the checkout, clone it again (the why).
    Recreate(String),
    /// The table only adds cone dirs to a clean checkout at its pin: `git sparse-checkout add` them in place (these dirs); existing files untouched, the new dirs' blobs fetched from the promisor remote.
    Grow(Vec<String>),
    Blocked(String),
}

impl Action {
    fn describe(&self) -> String {
        match self {
            Action::Clone => "missing -> clone".into(),
            Action::Reclone => "interrupted clone -> delete, clone again".into(),
            Action::AtPin { dirty: false } => "at pin".into(),
            Action::AtPin { dirty: true } => "at pin, uncommitted changes".into(),
            Action::MoveToPin => "elsewhere -> move to pin".into(),
            Action::PatchBranch => "on its patch branch".into(),
            Action::LeaveBranch(b) => format!("on branch {b}, left alone"),
            Action::Recreate(why) => format!("{why} -> delete, clone again"),
            Action::Grow(dirs) => {
                format!("keep-set grows by {} -> added in place", dirs.join(", "))
            }
            Action::Blocked(why) => format!("blocked: {why}"),
        }
    }
}

/// `patch_branch`: the checkout's own (`atl-patches`, or `atl-patches-<task>` in a task copy).
fn decide(local: &Local, pin: &str, patch_branch: &str) -> Action {
    match local {
        Local::Missing => Action::Clone,
        Local::Unborn { files: false } => Action::Reclone,
        Local::Unborn { files: true } => Action::Blocked(
            "git repository without commits holding files; remove it by hand".into(),
        ),
        Local::Present {
            branch: Some(b), ..
        } if b == patch_branch => Action::PatchBranch,
        Local::Present { head, dirty, .. } if head == pin => Action::AtPin { dirty: *dirty },
        Local::Present {
            branch: Some(b), ..
        } => Action::LeaveBranch(b.clone()),
        Local::Present { dirty: true, .. } => {
            Action::Blocked("not at pin and has uncommitted changes".into())
        }
        Local::Present { .. } => Action::MoveToPin,
    }
}

fn inspect(dir: &Path) -> Result<Local> {
    if !dir.exists() {
        return Ok(Local::Missing);
    }
    ensure!(
        dir.join(".git").exists(),
        "{} exists but is not a git checkout",
        dir.display()
    );
    ensure!(
        !dir.join(".git").is_dir(),
        "{}: its git dir is still inside the tree (a task copy's worktree admin dir would be hidden by the namespace bind): cargo xtask fetch --migrate-git-dirs",
        dir.display()
    );
    let Ok(head) = git(Some(dir), &["rev-parse", "-q", "--verify", "HEAD"]) else {
        let files = std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .any(|e| e.file_name() != ".git");
        return Ok(Local::Unborn { files });
    };
    let branch = git(Some(dir), &["symbolic-ref", "-q", "--short", "HEAD"]).ok();
    let dirty = !git(
        Some(dir),
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty();
    let sparse = if git(Some(dir), &["config", "--bool", "core.sparseCheckout"])
        .is_ok_and(|v| v == "true")
    {
        git(Some(dir), &["sparse-checkout", "list"])?
            .lines()
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    Ok(Local::Present {
        head,
        branch,
        dirty,
        sparse,
    })
}

/// A present checkout's keep-set must equal sources.toml's: a table change that the tree ignores would leave both disagreeing without a word. Growth at the pin is fixed in place ([`keep_growth`]); a shrink, a sparse <-> full change or a checkout off its pin is not.
fn keep_mismatch(local: &Local, want: &[String]) -> Option<String> {
    let Local::Present { sparse, .. } = local else {
        return None;
    };
    let show = |v: &[String]| {
        if v.is_empty() {
            "a full tree".to_owned()
        } else {
            v.join(", ")
        }
    };
    let (mut have, mut want) = (sparse.clone(), want.to_vec());
    have.sort();
    want.sort();
    (have != want).then(|| {
        format!(
            "keep-set differs: checkout holds {}, sources.toml [aosp.sparse] wants {}; only adding dirs at the pin is done in place: delete the checkout at its pin and fetch again",
            show(&have),
            show(&want)
        )
    })
}

/// The dirs to add when the table only grows a sparse checkout's keep-set and the checkout is clean at `pin`, else None. Growing a cone in place leaves every checked-out file alone (readers of the shared checkout unaffected);
/// removing a dir would delete files under live readers, and leaving the pin is MoveToPin's business, so both keep the delete-and-clone rule.
fn keep_growth(local: &Local, want: &[String], pin: &str) -> Option<Vec<String>> {
    let Local::Present {
        head,
        dirty: false,
        sparse,
        ..
    } = local
    else {
        return None;
    };
    if head != pin || sparse.is_empty() || !sparse.iter().all(|d| want.contains(d)) {
        return None;
    }
    let mut add: Vec<String> = want
        .iter()
        .filter(|d| !sparse.contains(d))
        .cloned()
        .collect();
    add.sort();
    (!add.is_empty()).then_some(add)
}

/// `git sparse-checkout add dirs` at `commit` in `dir`, each dir checked to exist there first (cone mode silently checks out nothing for a missing one).
fn grow_checkout(dir: &Path, commit: &str, dirs: &[String]) -> Result<()> {
    for d in dirs {
        git(Some(dir), &["cat-file", "-e", &format!("{commit}:{d}")])
            .with_context(|| format!("keep-set dir `{d}` is not in the tree at {commit}"))?;
    }
    let mut args = vec!["sparse-checkout", "add"];
    args.extend(dirs.iter().map(String::as_str));
    git(Some(dir), &args)?;
    Ok(())
}

/// A keep-set mismatch that `--recreate` names and the checkout can lose: at its pin, nothing uncommitted.
fn recreates(ctx: &Ctx, c: &Checkout, local: &Local) -> bool {
    let named = matches!(&c.pin, Pin::Aosp(p) if ctx.recreate.contains(p));
    named && matches!(local, Local::Present { head, dirty: false, .. } if *head == c.commit)
}

/// Deletes a checkout and its git dir (outside the tree); the caller clones it again.
fn remove_checkout(dir: &Path, git_dir: &Path) -> Result<()> {
    for d in [dir, git_dir] {
        match std::fs::remove_dir_all(d) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", d.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// `git_dir`: where a new checkout's git dir goes (`Layout::git_dir`); Clone and Reclone only happen to shared checkouts.
fn apply(action: &Action, c: &Checkout, dir: &Path, git_dir: &Path) -> Result<()> {
    match action {
        Action::Clone => {
            verify_ref(c)?;
            let sparse = if c.sparse.is_empty() {
                Sparse::Full
            } else {
                Sparse::Cone(&c.sparse)
            };
            create_checkout(dir, git_dir, c.url()?, &c.rev, &sparse, Some(&c.commit))?;
            Ok(())
        }
        Action::Reclone => {
            remove_checkout(dir, git_dir)?;
            apply(&Action::Clone, c, dir, git_dir)
        }
        Action::Recreate(_) => {
            // the remote must still resolve the pin before the only copy of the checkout is deleted
            verify_ref(c)?;
            remove_checkout(dir, git_dir)?;
            apply(&Action::Clone, c, dir, git_dir)
        }
        Action::Grow(dirs) => grow_checkout(dir, &c.commit, dirs),
        Action::MoveToPin => {
            verify_ref(c)?;
            fetch_pin(c, dir)?;
            checkout_pin(c, dir)
        }
        Action::AtPin { .. } | Action::PatchBranch | Action::LeaveBranch(_) => Ok(()),
        Action::Blocked(why) => bail!("{why}"),
    }
}

fn checkout_pin(c: &Checkout, dir: &Path) -> Result<()> {
    git(
        Some(dir),
        &[
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "-q",
            "--detach",
            &c.commit,
        ],
    )?;
    Ok(())
}

/// Shallow-fetch the pinned ref into `dir` and check it is the pinned commit. A sparse checkout is
/// a partial clone: blobs outside the cone are never fetched.
pub fn fetch_pin(c: &Checkout, dir: &Path) -> Result<()> {
    let refspec = format!("+{0}:{0}", c.rev);
    let mut args = vec!["fetch", "-q", "--depth", "1", "--no-tags"];
    if !c.sparse.is_empty() {
        args.push("--filter=blob:none");
    }
    args.extend([c.url()?, &refspec]);
    git(Some(dir), &args)?;
    let fetched = git(Some(dir), &["rev-parse", &format!("{}^{{commit}}", c.rev)])?;
    ensure!(
        fetched == c.commit,
        "{} is {fetched}, pin says {}",
        c.rev,
        c.commit
    );
    Ok(())
}

/// Which files a new checkout holds.
pub enum Sparse<'a> {
    Full,
    /// Cone mode: these directories (sources.toml `sparse`).
    Cone(&'a [String]),
    /// Non-cone patterns (the AOSP blueprint index: `*.bp`).
    Patterns(&'a [&'a str]),
}

/// The one way a checkout is created: a shallow checkout of `refname` from `url` at `dir`, detached, its git dir at `git_dir` (outside the tree, `dir/.git` a gitfile -- atl_layout::git_store).
/// A sparse one is a blobless promisor partial clone, so blobs outside it are never fetched. `expect`: the pinned commit, checked before checkout. Re-running on an interrupted one reuses its git dir. Returns the commit.
pub fn create_checkout(
    dir: &Path,
    git_dir: &Path,
    url: &str,
    refname: &str,
    sparse: &Sparse,
    expect: Option<&str>,
) -> Result<String> {
    for d in [Some(dir), git_dir.parent()].into_iter().flatten() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let g = |args: &[&str]| git(Some(dir), args);
    g(&[
        "init",
        "-q",
        "--separate-git-dir",
        &git_dir.display().to_string(),
    ])?;
    crate::git_dirs::write_gitfile(dir, git_dir)?;
    g(&["remote", "add", "origin", url]).or_else(|_| g(&["remote", "set-url", "origin", url]))?;
    let mut fetch = vec!["fetch", "-q", "--depth", "1", "--no-tags"];
    let (mode, set): (&str, Vec<&str>) = match sparse {
        Sparse::Full => ("", Vec::new()),
        Sparse::Cone(dirs) => ("--cone", dirs.iter().map(String::as_str).collect()),
        Sparse::Patterns(p) => ("--no-cone", p.to_vec()),
    };
    if !mode.is_empty() {
        // lazy blob fetch on checkout needs the remote marked as a partial-clone promisor
        g(&["config", "remote.origin.promisor", "true"])?;
        g(&["config", "remote.origin.partialclonefilter", "blob:none"])?;
        let mut args = vec!["sparse-checkout", "set", mode];
        args.extend(set);
        g(&args)?;
        fetch.push("--filter=blob:none");
    }
    let refspec = format!("+{refname}:{refname}");
    fetch.extend(["origin", &refspec]);
    g(&fetch)?;
    let commit = g(&["rev-parse", &format!("{refname}^{{commit}}")])?;
    if let Some(want) = expect {
        ensure!(commit == want, "{refname} is {commit}, pin says {want}");
    }
    // cone mode silently checks out nothing for a missing dir: name it before any file is written
    if let Sparse::Cone(dirs) = sparse {
        for d in dirs.iter() {
            g(&["cat-file", "-e", &format!("{commit}:{d}")])
                .with_context(|| format!("keep-set dir `{d}` is not in the tree at {commit}"))?;
        }
    }
    g(&[
        "-c",
        "advice.detachedHead=false",
        "checkout",
        "-q",
        "--detach",
        &commit,
    ])?;
    Ok(commit)
}

/// The remote's current commit for the ref must be the pin: a moved tag (or advanced branch) fails here.
pub fn verify_ref(c: &Checkout) -> Result<()> {
    let url = c.url()?;
    let now = remote_ref_commit(url, &c.rev)?;
    ensure!(
        now == c.commit,
        "{} on {url} now resolves to {now}, pin says {} (moved ref, or run --bump)",
        c.rev,
        c.commit
    );
    Ok(())
}

fn remote_ref_commit(url: &str, refname: &str) -> Result<String> {
    let out = git(
        None,
        &["ls-remote", url, refname, &format!("{refname}^{{}}")],
    )?;
    peeled_commit(&out, refname).with_context(|| format!("no {refname} on {url}"))
}

/// From `git ls-remote` output: the commit an annotated tag points to (`^{}` line), else the
/// ref's own commit.
fn peeled_commit(ls_remote: &str, refname: &str) -> Option<String> {
    let line = |suffix: &str| {
        let want = format!("{refname}{suffix}");
        ls_remote.lines().find_map(|l| {
            let (sha, name) = l.split_once('\t')?;
            (name == want).then(|| sha.to_owned())
        })
    };
    line("^{}").or_else(|| line(""))
}

// ---- --bump ------------------------------------------------------------------------------------

fn bump(root: &Path, all: &[Checkout], write: bool, jobs: usize) -> Result<bool> {
    let mut moved = Vec::new();
    let mut failed = 0;
    let resolved = par::map(all, jobs, |c| remote_ref_commit(c.url()?, &c.rev));
    for (c, result) in all.iter().zip(resolved) {
        match result {
            Ok(now) if now == c.commit => {
                println!("bump: {:<40} {} unchanged", c.dir.display(), c.rev)
            }
            Ok(now) => {
                println!(
                    "bump: {:<40} {} {} -> {now}",
                    c.dir.display(),
                    c.rev,
                    &c.commit[..12]
                );
                moved.push((c, now));
            }
            Err(e) => {
                failed += 1;
                println!("bump: {:<40} FAILED: {e:#}", c.dir.display());
            }
        }
    }
    if write && !moved.is_empty() {
        edit_sources(root, |doc| {
            for (c, now) in &moved {
                let now = toml_edit::value(now.as_str());
                match &c.pin {
                    Pin::Aosp(p) => doc["aosp"]["projects"][p.as_str()] = now,
                    Pin::Manifest => doc["aosp"]["manifest"] = now,
                    Pin::Git(name) => doc[name.as_str()]["commit"] = now,
                }
            }
            Ok(())
        })?;
        println!("bump: wrote {} pins to {}", moved.len(), sources::FILE);
    } else if !moved.is_empty() {
        println!(
            "bump: {} pins would move; --write updates {}",
            moved.len(),
            sources::FILE
        );
    }
    Ok(failed == 0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// `decide` for a shared checkout (shadows the glob import).
    fn decide(local: &Local, pin: &str) -> Action {
        super::decide(local, pin, patch::BRANCH)
    }

    #[test]
    fn keep_set_must_match_the_table() {
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        let present = |sparse: &[&str]| Local::Present {
            head: "p".into(),
            branch: None,
            dirty: false,
            sparse: strs(sparse),
        };
        assert!(keep_mismatch(&present(&[]), &strs(&[])).is_none());
        assert!(keep_mismatch(&present(&["b", "a"]), &strs(&["a", "b"])).is_none());
        assert!(keep_mismatch(&Local::Missing, &strs(&["a"])).is_none());
        let full = keep_mismatch(&present(&[]), &strs(&["a"])).unwrap();
        assert!(
            full.contains("holds a full tree") && full.contains("wants a"),
            "{full}"
        );
        let other = keep_mismatch(&present(&["a"]), &strs(&["a", "b"])).unwrap();
        assert!(
            other.contains("holds a,") && other.contains("wants a, b"),
            "{other}"
        );
    }

    /// Growth at the pin is in place; a shrink, a swap, a dirty or off-pin checkout and a full tree are not.
    #[test]
    fn keep_set_grows_only_by_adding_at_the_pin() {
        let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        let at = |head: &str, dirty: bool, sparse: &[&str]| Local::Present {
            head: head.into(),
            branch: None,
            dirty,
            sparse: strs(sparse),
        };
        let clean = at("p", false, &["28", "36"]);
        assert_eq!(
            keep_growth(&clean, &strs(&["36", "24", "28"]), "p"),
            Some(strs(&["24"]))
        );
        assert_eq!(keep_growth(&clean, &strs(&["28"]), "p"), None, "shrink");
        assert_eq!(
            keep_growth(&clean, &strs(&["24", "28"]), "p"),
            None,
            "swap drops 36"
        );
        assert_eq!(
            keep_growth(&clean, &strs(&["28", "36"]), "p"),
            None,
            "equal"
        );
        assert_eq!(
            keep_growth(&clean, &strs(&["24", "28", "36"]), "q"),
            None,
            "off pin"
        );
        assert_eq!(
            keep_growth(&at("p", true, &["28"]), &strs(&["24", "28"]), "p"),
            None,
            "dirty"
        );
        assert_eq!(
            keep_growth(&at("p", false, &[]), &strs(&["24"]), "p"),
            None,
            "full tree"
        );
    }

    /// A real promisor partial clone (file:// remote with filters allowed), created as fetch creates one with keep-set [a], grown to [a, b]:
    /// b's file appears, a's file keeps its bytes and its inode (not rewritten). Break: `grow_checkout` with a dir absent at the pin fails and adds nothing.
    #[test]
    fn grow_in_place_keeps_existing_files() {
        use std::os::unix::fs::MetadataExt;
        let tmp = std::env::temp_dir().join(format!("xtask-grow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (src, dir, gd) = (tmp.join("src"), tmp.join("co"), tmp.join("git/co"));
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::create_dir_all(src.join("b")).unwrap();
        std::fs::write(src.join("a/f"), "alpha\n").unwrap();
        std::fs::write(src.join("b/g"), "beta\n").unwrap();
        let g = |at: &Path, args: &[&str]| {
            let o = std::process::Command::new("git")
                .arg("-C")
                .arg(at)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            String::from_utf8(o.stdout).unwrap().trim().to_owned()
        };
        g(&src, &["init", "-q", "-b", "rel"]);
        g(&src, &["config", "uploadpack.allowFilter", "true"]);
        g(&src, &["add", "."]);
        g(&src, &["commit", "-q", "-m", "a"]);
        let pin = g(&src, &["rev-parse", "HEAD"]);
        let url = format!("file://{}", src.display());
        let keep = vec!["a".to_owned()];
        create_checkout(
            &dir,
            &gd,
            &url,
            "refs/heads/rel",
            &Sparse::Cone(&keep),
            Some(&pin),
        )
        .unwrap();
        let before = std::fs::metadata(dir.join("a/f")).unwrap();
        assert!(!dir.join("b").exists());
        let local = inspect(&dir).unwrap();
        let want = vec!["a".to_owned(), "b".to_owned()];
        assert!(grow_checkout(&dir, &pin, &["nope".to_owned()]).is_err());
        let add = keep_growth(&local, &want, &pin).expect("growth");
        grow_checkout(&dir, &pin, &add).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("b/g")).unwrap(), "beta\n");
        let after = std::fs::metadata(dir.join("a/f")).unwrap();
        assert_eq!(std::fs::read(dir.join("a/f")).unwrap(), b"alpha\n");
        assert_eq!(
            (before.ino(), before.mtime_nsec()),
            (after.ino(), after.mtime_nsec())
        );
        assert!(keep_mismatch(&inspect(&dir).unwrap(), &want).is_none());
        // the shrink back is still refused in place
        assert_eq!(keep_growth(&inspect(&dir).unwrap(), &keep, &pin), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn peeled_commit_prefers_annotated_target() {
        // `git ls-remote` format: <sha>\t<ref>; an annotated tag also lists its target as `^{}`
        let out = "aaaa\trefs/tags/v1\nbbbb\trefs/tags/v1^{}\ncccc\trefs/tags/v10\n";
        assert_eq!(peeled_commit(out, "refs/tags/v1").as_deref(), Some("bbbb"));
        assert_eq!(peeled_commit(out, "refs/tags/v10").as_deref(), Some("cccc"));
        assert_eq!(peeled_commit(out, "refs/tags/v2"), None);
    }

    /// A branch pin (sources.toml `branch` + `commit`) passes while the remote branch is at the pin and fails once the branch advances: a real git remote, so the ls-remote query is the one `verify_ref` sends.
    #[test]
    fn advanced_branch_pin_fails() {
        let tmp = std::env::temp_dir().join(format!("xtask-branch-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let g = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(&tmp)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(st.status.success(), "git {args:?}");
            String::from_utf8(st.stdout).unwrap().trim().to_owned()
        };
        g(&["init", "-q", "-b", "rel"]);
        g(&["commit", "-q", "--allow-empty", "-m", "a"]);
        let pinned = g(&["rev-parse", "HEAD"]);
        let c = Checkout {
            dir: PathBuf::from("llvm"),
            url: Some(tmp.display().to_string()),
            rev: "refs/heads/rel".into(),
            commit: pinned,
            sparse: vec![],
            pin: crate::sources::Pin::Git("llvm".into()),
            licence_key: "llvm".into(),
        };
        verify_ref(&c).unwrap();
        g(&["commit", "-q", "--allow-empty", "-m", "b"]);
        let err = format!("{:#}", verify_ref(&c).unwrap_err());
        assert!(err.contains("moved ref"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn decisions() {
        let at = |head: &str, branch: Option<&str>, dirty| Local::Present {
            head: head.into(),
            branch: branch.map(Into::into),
            dirty,
            sparse: Vec::new(),
        };
        assert_eq!(decide(&Local::Missing, "p"), Action::Clone);
        assert_eq!(
            decide(&at("p", None, false), "p"),
            Action::AtPin { dirty: false }
        );
        assert_eq!(
            decide(&at("p", Some("mine"), true), "p"),
            Action::AtPin { dirty: true }
        );
        assert_eq!(decide(&at("x", None, false), "p"), Action::MoveToPin);
        assert_eq!(
            decide(&at("x", Some("mine"), false), "p"),
            Action::LeaveBranch("mine".into())
        );
        // atl-patches is settled against patches/, wherever it points
        assert_eq!(
            decide(&at("p", Some(patch::BRANCH), true), "p"),
            Action::PatchBranch
        );
        assert_eq!(
            decide(&at("x", Some(patch::BRANCH), false), "p"),
            Action::PatchBranch
        );
        // a task copy's patch branch is its own; the shared name there is someone else's branch
        let task = "atl-patches-t";
        assert_eq!(
            super::decide(&at("x", Some(task), false), "p", task),
            Action::PatchBranch
        );
        assert_eq!(
            super::decide(&at("x", Some(patch::BRANCH), false), "p", task),
            Action::LeaveBranch(patch::BRANCH.into())
        );
        assert!(matches!(
            decide(&at("x", None, true), "p"),
            Action::Blocked(_)
        ));
    }

    /// A throwaway local "remote": one commit, an annotated tag `v1`, then a second commit.
    struct Remote {
        dir: PathBuf,
        first: String,
        second: String,
    }

    impl Remote {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("xtask-fetch-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let g = |args: &[&str]| git(Some(&dir), args).unwrap();
            g(&["init", "-q"]);
            let commit = |msg: &str| {
                g(&[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    msg,
                ]);
                g(&["rev-parse", "HEAD"])
            };
            let first = commit("one");
            g(&[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "tag",
                "-a",
                "v1",
                "-m",
                "v1",
            ]);
            let second = commit("two");
            Remote { dir, first, second }
        }

        fn checkout(&self, commit: &str) -> Checkout {
            Checkout {
                dir: PathBuf::from("up"),
                url: Some(format!("file://{}", self.dir.display())),
                rev: "refs/tags/v1".into(),
                commit: commit.into(),
                sparse: Vec::new(),
                pin: Pin::Git("up".into()),
                licence_key: "up".into(),
            }
        }
    }

    impl Drop for Remote {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn clone_then_at_pin() {
        let remote = Remote::new("clone");
        let src = remote.dir.with_extension("src");
        let _ = std::fs::remove_dir_all(&src);
        let c = remote.checkout(&remote.first);
        let dir = src.join(&c.dir);
        let store = atl_layout::git_store(&src, &c.dir);
        apply(
            &decide(&inspect(&dir).unwrap(), &c.commit),
            &c,
            &dir,
            &store,
        )
        .unwrap();
        assert_eq!(
            git(Some(&dir), &["rev-parse", "HEAD"]).unwrap(),
            remote.first
        );
        // the git dir is in the store, the tree holds a relative gitfile
        assert_eq!(
            std::fs::read_to_string(dir.join(".git")).unwrap(),
            "gitdir: ../.git-dirs/up.git\n"
        );
        assert!(store.join("HEAD").is_file());
        assert_eq!(
            decide(&inspect(&dir).unwrap(), &c.commit),
            Action::AtPin { dirty: false }
        );
        // an in-tree git dir (not yet migrated) is refused, naming the migration
        let old = src.join("old");
        std::fs::create_dir_all(&old).unwrap();
        git(Some(&old), &["init", "-q"]).unwrap();
        let e = format!("{:#}", inspect(&old).unwrap_err());
        assert!(e.contains("--migrate-git-dirs"), "{e}");
        std::fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn interrupted_clone_is_recloned() {
        let remote = Remote::new("reclone");
        let src = remote.dir.with_extension("src");
        let _ = std::fs::remove_dir_all(&src);
        let c = remote.checkout(&remote.first);
        let dir = src.join(&c.dir);
        // what a killed `git fetch --depth 1` leaves (measured on prebuilts/clang): a stale lock that
        // makes every later fetch fail, and a partial pack
        let store = atl_layout::git_store(&src, &c.dir);
        let init = |dir: &Path, store: &Path| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::create_dir_all(store.parent().unwrap()).unwrap();
            git(
                Some(dir),
                &[
                    "init",
                    "-q",
                    "--separate-git-dir",
                    &store.display().to_string(),
                ],
            )
            .unwrap();
        };
        init(&dir, &store);
        std::fs::write(store.join("shallow.lock"), "").unwrap();
        std::fs::write(store.join("objects/pack/tmp_pack_x"), "partial").unwrap();
        let local = inspect(&dir).unwrap();
        assert_eq!(decide(&local, &c.commit), Action::Reclone);
        apply(&Action::Reclone, &c, &dir, &store).unwrap();
        assert_eq!(
            decide(&inspect(&dir).unwrap(), &c.commit),
            Action::AtPin { dirty: false }
        );
        // the stale store went with the tree
        assert!(!store.join("shallow.lock").exists());
        // a commit-less repository with someone's files in it is never deleted
        let other = src.join("other");
        init(&other, &atl_layout::git_store(&src, Path::new("other")));
        std::fs::write(other.join("notes.txt"), "mine").unwrap();
        assert!(matches!(
            decide(&inspect(&other).unwrap(), &c.commit),
            Action::Blocked(_)
        ));
        std::fs::remove_dir_all(&src).unwrap();
    }

    /// `--drop`: an unpinned or unrequested checkout goes (dry run first); a requested one, a dirty one, one on a local branch and one with a task copy are refused, and one refusal drops nothing.
    #[test]
    fn drop_guards() {
        let remote = Remote::new("drop");
        let ws = remote.dir.with_extension("ws");
        let _ = std::fs::remove_dir_all(&ws);
        let layout = Layout::new(&ws, None);
        let url = format!("file://{}", remote.dir.display());
        let make = |dir: &str| {
            let d = Path::new(dir);
            create_checkout(
                &layout.src().join(d),
                &layout.git_dir(d),
                &url,
                "refs/tags/v1",
                &Sparse::Full,
                None,
            )
            .unwrap();
            layout.src().join(d)
        };
        let sha = &remote.first;
        let sources = sources::parse(&format!(
            "[aosp]\nremote = \"https://h/\"\ntag = \"t\"\nmanifest = \"{sha}\"\n[aosp.projects]\ncts = \"{sha}\"\ncore = \"{sha}\"\n[aosp.needed_by]\ncts = [\"cts\"]\n"
        ))
        .unwrap();
        for d in [
            "aosp/cts",
            "aosp/core",
            "aosp/gone",
            "aosp/dirty",
            "aosp/branch",
            "aosp/copied",
        ] {
            make(d);
        }
        std::fs::write(layout.src().join("aosp/dirty/x"), "").unwrap();
        git(Some(&layout.src().join("aosp/dirty")), &["add", "x"]).unwrap();
        git(
            Some(&layout.src().join("aosp/branch")),
            &["checkout", "-q", "-b", "mine"],
        )
        .unwrap();
        let copy = ws.join("src-wt/t/aosp/copied").display().to_string();
        git(
            Some(&layout.src().join("aosp/copied")),
            &["worktree", "add", "-q", "--detach", &copy],
        )
        .unwrap();
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let listed: Vec<String> = droppable(&layout, &sources, &[])
            .unwrap()
            .iter()
            .map(|d| format!("{} {}", d.dir.display(), d.why))
            .collect();
        assert_eq!(
            listed,
            [
                "aosp/branch present, not pinned in sources.toml",
                "aosp/copied present, not pinned in sources.toml",
                "aosp/cts present, not requested (needed_by [cts])",
                "aosp/dirty present, not pinned in sources.toml",
                "aosp/gone present, not pinned in sources.toml",
            ]
        );
        // requested: cts is kept, core always
        assert!(
            droppable(&layout, &sources, &v(&["all-gates"]))
                .unwrap()
                .iter()
                .all(|d| d.dir != Path::new("aosp/cts"))
        );
        let refused = |dir: &str| !drop_in(&layout, &sources, &[], &v(&[dir]), true).unwrap();
        for d in ["aosp/core", "aosp/dirty", "aosp/branch", "aosp/copied"] {
            assert!(refused(d), "{d} dropped");
            assert!(layout.src().join(d).is_dir(), "{d} removed");
        }
        assert!(!drop_in(&layout, &sources, &v(&["cts"]), &v(&["aosp/cts"]), true).unwrap());
        // one refusal: nothing goes
        assert!(
            !drop_in(
                &layout,
                &sources,
                &[],
                &v(&["aosp/gone", "aosp/dirty"]),
                true
            )
            .unwrap()
        );
        assert!(layout.src().join("aosp/gone").is_dir());
        // dry run changes nothing; execute removes tree and git dir
        assert!(
            drop_in(
                &layout,
                &sources,
                &[],
                &v(&["aosp/gone", "aosp/cts"]),
                false
            )
            .unwrap()
        );
        assert!(layout.src().join("aosp/gone").is_dir());
        assert!(
            drop_in(
                &layout,
                &sources,
                &[],
                &v(&["aosp/gone", "aosp/cts/"]),
                true
            )
            .unwrap()
        );
        for d in ["aosp/gone", "aosp/cts"] {
            assert!(
                !layout.src().join(d).exists() && !layout.git_dir(Path::new(d)).exists(),
                "{d} kept"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn moved_tag_fails() {
        let remote = Remote::new("moved");
        // pin claims v1 is the second commit: the remote disagrees
        let c = remote.checkout(&remote.second);
        let e = format!("{:#}", verify_ref(&c).unwrap_err());
        assert!(
            e.contains(&format!("now resolves to {}", remote.first)),
            "{e}"
        );
    }

    // ---- patches/ applied by fetch -------------------------------------------------------------

    use crate::patch::fixture::*;

    /// Fetch in `layout` with a main that has no row of the fixture's upstream: nothing refused.
    fn ctx<'a>(f: &'a Fixture, layout: &'a Layout, execute: bool) -> Ctx<'a> {
        with_main(f, layout, execute, Main::default())
    }

    fn with_main<'a>(f: &'a Fixture, layout: &'a Layout, execute: bool, main: Main) -> Ctx<'a> {
        Ctx {
            root: &f.root,
            layout,
            main,
            licenses: licenses(),
            ident: Some(ident()),
            execute,
            recreate: &[],
        }
    }

    fn sync_err(ctx: &Ctx, c: &Checkout) -> String {
        match sync(ctx, c) {
            Ok(row) => panic!("sync succeeded: {:?} {:?}", row.action, row.patches),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn fetch_applies_stored_patches() {
        let f = Fixture::new("fetch-apply");
        let c = f.checkout("v1");
        let dev = f.fetched(&c, "dev");
        patch_commit(&dev, TRAILED);
        export_ok(&dev);
        let ws = f.layout("ws", None);

        let row = sync(&ctx(&f, &ws, false), &c).unwrap();
        assert_eq!(row.action, Action::Clone);
        assert_eq!(row.patches.as_deref(), Some("then apply 1 patches"));
        let row = sync(&ctx(&f, &ws, true), &c).unwrap();
        assert_eq!(row.patches.as_deref(), Some("applied 1 patches"));
        // the developer's tree, rebuilt from patches/ alone
        let u = f.at(&c, "ws");
        assert!(on_patch_branch(&u));
        assert_eq!(tree(&u), tree(&dev));
        assert_eq!(drift_of(&u), Vec::<String>::new());
        // in step: nothing to report
        let row = sync(&ctx(&f, &ws, true), &c).unwrap();
        assert_eq!((row.action, row.patches), (Action::PatchBranch, None));
    }

    /// src/ keeps main's pins and patches: another checkout's go to its task copy, never into src/.
    #[test]
    fn fetch_keeps_src_on_main() {
        let f = Fixture::new("fetch-main");
        let c = f.checkout("v1");
        let shared = f.fetched(&c, "ws");
        // this checkout's patches/ holds a patch main lacks
        let dev = f.fetched(&c, "dev");
        patch_commit(&dev, TRAILED);
        export_ok(&dev);
        let stored = dev.stored().unwrap();
        let main = |commit: &str, patches: Vec<(String, String)>| Main {
            pins: BTreeMap::from([(PathBuf::from("up"), commit.to_owned())]),
            patches: BTreeMap::from([(PathBuf::from("up"), patches)]),
        };
        let on_main = f.layout("ws", None);
        let before = head(&shared);
        for execute in [false, true] {
            let e = sync_err(
                &with_main(&f, &on_main, execute, main(&c.commit, vec![])),
                &c,
            );
            assert!(
                e.contains("this checkout's patches/ differ from origin/main's")
                    && e.contains("xtask patch --edit up"),
                "{e}"
            );
        }
        let other = "0".repeat(40);
        let e = sync_err(&with_main(&f, &on_main, true, main(&other, vec![])), &c);
        assert!(e.contains("pin and patches/ differ"), "{e}");
        assert_eq!(head(&shared), before);
        assert!(!on_patch_branch(&shared));

        // a task copy is the task's own: fetch settles it, src/ stays as it was
        let copy = f.root.join("ws/src-wt/t/up");
        git(
            Some(shared.checkout()),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                &copy.display().to_string(),
                &c.commit,
            ],
        )
        .unwrap();
        let on_task = f.layout("ws", Some("t"));
        let row = sync(&with_main(&f, &on_task, true, main(&c.commit, vec![])), &c).unwrap();
        assert_eq!(row.patches.as_deref(), Some("applied 1 patches"));
        let task = f.on(&c, "ws", "t");
        assert_eq!(task.checkout(), copy);
        assert!(on_patch_branch(&task));
        assert_eq!(drift_of(&task), Vec::<String>::new());
        assert_eq!(head(&shared), before);

        // once main holds the patch, src/ follows it
        let row = sync(&with_main(&f, &on_main, true, main(&c.commit, stored)), &c).unwrap();
        assert_eq!(row.patches.as_deref(), Some("applied 1 patches"));
        assert!(on_patch_branch(&shared));
    }

    /// Main read from a repository's origin/main: pins, and patch texts byte for byte.
    #[test]
    fn main_from_origin() {
        let f = Fixture::new("fetch-origin");
        let repo = f.root.join("repo");
        std::fs::create_dir_all(repo.join("patches/up")).unwrap();
        let commit = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(
            repo.join(sources::FILE),
            format!(
                "[aosp]\nremote = \"https://h/\"\ntag = \"t\"\nmanifest = \"{commit}\"\n[aosp.projects]\n\n[up]\ngit = \"file:///x\"\ntag = \"v1\"\ncommit = \"{commit}\"\n"
            ),
        )
        .unwrap();
        let text = "Subject: x\n---\ndiff\n\n";
        std::fs::write(repo.join("patches/up/0001-x.patch"), text).unwrap();
        std::fs::write(repo.join("patches/up/LICENSE"), "l\n").unwrap();
        let g = |args: &[&str]| git(Some(&repo), args).unwrap();
        g(&["init", "-q"]);
        g(&["add", "-A"]);
        g(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "m",
        ]);
        let e = format!("{:#}", Main::load(&repo).err().expect("no origin/main yet"));
        assert!(e.contains("reading origin/main:sources.toml"), "{e}");
        g(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let main = Main::load(&repo).unwrap();
        assert_eq!(main.pins[Path::new("up")], commit);
        assert_eq!(main.pins[Path::new("aosp-manifest")], commit);
        assert_eq!(
            main.patches[Path::new("up")],
            [("0001-x.patch".to_owned(), text.to_owned())]
        );
    }

    #[test]
    fn fetch_refuses_to_drop_unexported_commits() {
        let f = Fixture::new("fetch-refuse");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "ws");
        let ws = f.layout("ws", None);
        patch_commit(&u, TRAILED);
        export_ok(&u);
        patch_commit(
            &u,
            "fix b\n\nUpstream-Status: Pending\nPatch-Reason: test\n",
        );
        let before = head(&u);
        for execute in [false, true] {
            let e = sync_err(&ctx(&f, &ws, execute), &c);
            assert!(
                e.contains("atl-patches holds 1 commits not in patches/up")
                    && e.contains("xtask patch --export up"),
                "{e}"
            );
        }
        assert_eq!(head(&u), before);
        // the same branch while HEAD is detached: rebuilding it would reset the branch, so refused too
        git(
            Some(u.checkout()),
            &["checkout", "-q", "--detach", &c.commit],
        )
        .unwrap();
        let e = sync_err(&ctx(&f, &ws, true), &c);
        assert!(e.contains("xtask patch --export up"), "{e}");
        assert_eq!(
            git(Some(u.checkout()), &["rev-parse", patch::BRANCH]).unwrap(),
            before
        );
    }

    /// A patch merged, then deleted in the repo's history (as on origin/main): its commit on src/'s
    /// patch branch is no lost work, so drift names fetch and fetch puts the checkout back at the pin.
    #[test]
    fn fetch_drops_patches_retired_on_main() {
        let f = Fixture::new("fetch-retire");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "ws");
        let ws = f.layout("ws", None);
        patch_commit(&u, TRAILED);
        export_ok(&u);
        f.commit_repo("patches", "add the patch");
        std::fs::remove_dir_all(f.root.join("patches/up")).unwrap();
        f.commit_repo("patches", "retire the patch");
        assert!(
            drift_of(&u)
                .iter()
                .any(|p| p.contains("holds patches retired from patches/up: cargo xtask fetch")),
            "{:?}",
            drift_of(&u)
        );
        let row = sync(&ctx(&f, &ws, false), &c).unwrap();
        assert_eq!(
            row.patches.as_deref(),
            Some("drop the retired patches: back to the pin")
        );
        let row = sync(&ctx(&f, &ws, true), &c).unwrap();
        assert_eq!(
            row.patches.as_deref(),
            Some("dropped the retired patches; at the pin")
        );
        assert_eq!(head(&u), c.commit);
        assert!(!on_patch_branch(&u));
        assert_eq!(drift_of(&u), Vec::<String>::new());
        // a commit nothing retired is still refused (fetch_refuses_to_drop_unexported_commits)
        patch_commit(
            &u,
            "fix b\n\nUpstream-Status: Pending\nPatch-Reason: test\n",
        );
        let e = sync_err(&ctx(&f, &ws, true), &c);
        assert!(e.contains("xtask patch --export up"), "{e}");
    }

    /// A land rewrote a patch under the same file name (2026-11-14 fid-nbridge, ART 0004): the old text is the repo history's, so src/'s branch holding it is
    /// no lost work and fetch rebuilds the branch from patches/ instead of refusing it.
    #[test]
    fn fetch_replaces_patches_rewritten_on_main() {
        let f = Fixture::new("fetch-rewrite");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "ws");
        let ws = f.layout("ws", None);
        let names = || -> Vec<std::ffi::OsString> {
            let mut n: Vec<_> = std::fs::read_dir(f.root.join("patches/up"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            n.sort();
            n
        };
        patch_commit(&u, TRAILED);
        export_ok(&u);
        let before = names();
        f.commit_repo("patches", "add the patch");
        // the same subject, another body: export writes the same file name, so the history records a rewrite (M), not a delete
        let dev = f.fetched(&c, "dev");
        patch_commit(&dev, TRAILED);
        std::fs::write(
            dev.checkout().join("a.c"),
            "// SPDX-License-Identifier: GPL-2.0-or-later\nint a;\nint other;\n",
        )
        .unwrap();
        git(
            Some(dev.checkout()),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-a",
                "--amend",
                "--no-edit",
            ],
        )
        .unwrap();
        export_ok(&dev);
        assert_eq!(names(), before, "the rewrite must keep the file name");
        f.commit_repo("patches", "rewrite the patch");
        let row = sync(&ctx(&f, &ws, true), &c).unwrap();
        assert_eq!(row.patches.as_deref(), Some("applied 1 patches"));
        assert_eq!(tree(&u), tree(&dev));
        assert_eq!(drift_of(&u), Vec::<String>::new());
    }

    #[test]
    fn pin_bump_reapplies_or_restores() {
        let f = Fixture::new("fetch-bump");
        let c1 = f.checkout("v1");
        let u1 = f.fetched(&c1, "ws");
        let ws = f.layout("ws", None);
        patch_commit(&u1, TRAILED);
        export_ok(&u1);
        // in step; fetch records the base a later bump needs
        assert_eq!(sync(&ctx(&f, &ws, true), &c1).unwrap().patches, None);
        assert_eq!(base_ref(&u1).as_deref(), Some(c1.commit.as_str()));

        // v2 leaves a.c alone: patches/ applies on the new pin, byte-identical
        f.write("b.c", "int b;\n");
        f.release("two", "v2");
        let c2 = f.checkout("v2");
        let row = sync(&ctx(&f, &ws, true), &c2).unwrap();
        assert_eq!(row.patches.as_deref(), Some("applied 1 patches"));
        let u2 = f.at(&c2, "ws");
        assert_eq!(base_ref(&u2).as_deref(), Some(c2.commit.as_str()));
        assert!(u2.checkout().join("b.c").exists());
        assert_eq!(drift_of(&u2), Vec::<String>::new());

        // v3 rewrites the line the patch's context needs: git am fails, the checkout is put back
        f.write(
            "a.c",
            "// SPDX-License-Identifier: GPL-2.0-or-later\nint changed;\n",
        );
        f.release("three", "v3");
        let c3 = f.checkout("v3");
        let before = head(&u2);
        let e = sync_err(&ctx(&f, &ws, true), &c3);
        assert!(
            e.contains("does not apply at the pin; checkout restored: xtask patch --rebase up"),
            "{e}"
        );
        assert_eq!(head(&u2), before);
        assert!(on_patch_branch(&u2));
        assert_eq!(base_ref(&u2).as_deref(), Some(c2.commit.as_str()));
    }
}
