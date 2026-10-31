// SPDX-License-Identifier: MIT
//! `xtask fetch`: upstream checkouts under `<workspace>/src` at the pins in sources.toml, with
//! `patches/<upstream>/` applied on branch `atl-patches`.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};

use crate::aosp::{self, Manifest};
use crate::git::{Ident, git};
use crate::licenses::{self, Licenses};
use crate::sources::{self, Checkout, Pin, Sources};
use crate::{par, patch, workspace};

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
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
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
    };
    ensure!(fetch(root, &args)?, "fetch failed (rows above)");
    Ok(())
}

/// False when a checkout failed (its row says why).
fn fetch(root: &Path, args: &Args) -> Result<bool> {
    let src = workspace::root_dir(root)?.join("src");
    let mut sources = sources::load(root)?;
    let ctx = Ctx {
        root,
        src: &src,
        licenses: licenses::load(root, &sources.upstream_names())?,
        // only `git am` needs it, and only then is its absence an error
        ident: Ident::of_repo(root).ok(),
        execute: args.execute,
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
    let results = par::map(&all, jobs, |c| sync(&ctx, c));
    for (c, result) in all.iter().zip(results) {
        rows.add(c, result);
    }
    println!("fetch: {} checkouts at pin", rows.at_pin);
    if args.execute
        && let Some(m) = &manifest
    {
        let made = aosp::apply_links(&src.join("aosp"), m)?;
        println!("fetch: {made} manifest link/copy files created");
    } else if !args.execute {
        println!(
            "fetch: dry run ({} checkouts under {}); --execute applies",
            all.len() + 1,
            src.display()
        );
    }
    Ok(rows.failed == 0)
}

/// What every checkout's sync shares.
struct Ctx<'a> {
    root: &'a Path,
    src: &'a Path,
    licenses: Licenses,
    ident: Option<Ident>,
    execute: bool,
}

/// One checkout's sync: what moving it to its pin needs, then what applying its patches needs.
struct Row {
    action: Action,
    patches: Option<String>,
}

/// What the checkout needs; with `execute`, also do it.
fn sync(ctx: &Ctx, c: &Checkout) -> Result<Row> {
    let u = patch::Up::new(c, ctx.src, ctx.root);
    let dir = ctx.src.join(&c.dir);
    let action = decide(&inspect(&dir)?, &c.commit);
    if ctx.execute {
        apply(&action, c, &dir)?;
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
        remote_tag_commit(&sources.aosp_url(&project.name), &sources.aosp.tag)
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
    /// On atl-patches: `patch::settle` decides, wherever HEAD is.
    PatchBranch,
    /// On a branch that is not ours.
    LeaveBranch(String),
    Blocked(&'static str),
}

impl Action {
    fn describe(&self) -> String {
        match self {
            Action::Clone => "missing -> clone".into(),
            Action::Reclone => "interrupted clone -> delete, clone again".into(),
            Action::AtPin { dirty: false } => "at pin".into(),
            Action::AtPin { dirty: true } => "at pin, uncommitted changes".into(),
            Action::MoveToPin => "elsewhere -> move to pin".into(),
            Action::PatchBranch => format!("on {}", patch::BRANCH),
            Action::LeaveBranch(b) => format!("on branch {b}, left alone"),
            Action::Blocked(why) => format!("blocked: {why}"),
        }
    }
}

fn decide(local: &Local, pin: &str) -> Action {
    match local {
        Local::Missing => Action::Clone,
        Local::Unborn { files: false } => Action::Reclone,
        Local::Unborn { files: true } => {
            Action::Blocked("git repository without commits holding files; remove it by hand")
        }
        Local::Present {
            branch: Some(b), ..
        } if b == patch::BRANCH => Action::PatchBranch,
        Local::Present { head, dirty, .. } if head == pin => Action::AtPin { dirty: *dirty },
        Local::Present {
            branch: Some(b), ..
        } => Action::LeaveBranch(b.clone()),
        Local::Present { dirty: true, .. } => {
            Action::Blocked("not at pin and has uncommitted changes")
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
    Ok(Local::Present {
        head,
        branch,
        dirty,
    })
}

fn apply(action: &Action, c: &Checkout, dir: &Path) -> Result<()> {
    match action {
        Action::Clone => {
            verify_tag(c)?;
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            git(Some(dir), &["init", "-q"])?;
            git(Some(dir), &["remote", "add", "origin", c.url()?])?;
            fetch_pin(c, dir)?;
            checkout_pin(c, dir)
        }
        Action::Reclone => {
            std::fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))?;
            apply(&Action::Clone, c, dir)
        }
        Action::MoveToPin => {
            verify_tag(c)?;
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

/// Shallow-fetch the pinned tag into `dir` and check it is the pinned commit.
pub fn fetch_pin(c: &Checkout, dir: &Path) -> Result<()> {
    let refspec = format!("+refs/tags/{0}:refs/tags/{0}", c.tag);
    git(
        Some(dir),
        &[
            "fetch",
            "-q",
            "--depth",
            "1",
            "--no-tags",
            c.url()?,
            &refspec,
        ],
    )?;
    let fetched = git(
        Some(dir),
        &["rev-parse", &format!("refs/tags/{}^{{commit}}", c.tag)],
    )?;
    ensure!(
        fetched == c.commit,
        "tag {} is {fetched}, pin says {}",
        c.tag,
        c.commit
    );
    Ok(())
}

/// The remote's current commit for the tag must be the pin: a moved tag fails here.
pub fn verify_tag(c: &Checkout) -> Result<()> {
    let url = c.url()?;
    let now = remote_tag_commit(url, &c.tag)?;
    ensure!(
        now == c.commit,
        "tag {} on {url} now resolves to {now}, pin says {} (moved tag, or run --bump)",
        c.tag,
        c.commit
    );
    Ok(())
}

fn remote_tag_commit(url: &str, tag: &str) -> Result<String> {
    let tag_ref = format!("refs/tags/{tag}");
    let out = git(
        None,
        &[
            "ls-remote",
            "--tags",
            url,
            &tag_ref,
            &format!("{tag_ref}^{{}}"),
        ],
    )?;
    peeled_commit(&out, tag).with_context(|| format!("no tag {tag} on {url}"))
}

/// From `git ls-remote` output: the commit an annotated tag points to (`^{}` line), else the
/// lightweight tag's own commit.
fn peeled_commit(ls_remote: &str, tag: &str) -> Option<String> {
    let line = |suffix: &str| {
        let want = format!("refs/tags/{tag}{suffix}");
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
    let resolved = par::map(all, jobs, |c| remote_tag_commit(c.url()?, &c.tag));
    for (c, result) in all.iter().zip(resolved) {
        match result {
            Ok(now) if now == c.commit => {
                println!("bump: {:<40} {} unchanged", c.dir.display(), c.tag)
            }
            Ok(now) => {
                println!(
                    "bump: {:<40} {} {} -> {now}",
                    c.dir.display(),
                    c.tag,
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

    #[test]
    fn peeled_commit_prefers_annotated_target() {
        // `git ls-remote` format: <sha>\t<ref>; an annotated tag also lists its target as `^{}`
        let out = "aaaa\trefs/tags/v1\nbbbb\trefs/tags/v1^{}\ncccc\trefs/tags/v10\n";
        assert_eq!(peeled_commit(out, "v1").as_deref(), Some("bbbb"));
        assert_eq!(peeled_commit(out, "v10").as_deref(), Some("cccc"));
        assert_eq!(peeled_commit(out, "v2"), None);
    }

    #[test]
    fn decisions() {
        let at = |head: &str, branch: Option<&str>, dirty| Local::Present {
            head: head.into(),
            branch: branch.map(Into::into),
            dirty,
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
                tag: "v1".into(),
                commit: commit.into(),
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
        apply(&decide(&inspect(&dir).unwrap(), &c.commit), &c, &dir).unwrap();
        assert_eq!(
            git(Some(&dir), &["rev-parse", "HEAD"]).unwrap(),
            remote.first
        );
        assert_eq!(
            decide(&inspect(&dir).unwrap(), &c.commit),
            Action::AtPin { dirty: false }
        );
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
        std::fs::create_dir_all(&dir).unwrap();
        git(Some(&dir), &["init", "-q"]).unwrap();
        std::fs::write(dir.join(".git/shallow.lock"), "").unwrap();
        std::fs::write(dir.join(".git/objects/pack/tmp_pack_x"), "partial").unwrap();
        let local = inspect(&dir).unwrap();
        assert_eq!(decide(&local, &c.commit), Action::Reclone);
        apply(&Action::Reclone, &c, &dir).unwrap();
        assert_eq!(
            decide(&inspect(&dir).unwrap(), &c.commit),
            Action::AtPin { dirty: false }
        );
        // a commit-less repository with someone's files in it is never deleted
        let other = src.join("other");
        std::fs::create_dir_all(&other).unwrap();
        git(Some(&other), &["init", "-q"]).unwrap();
        std::fs::write(other.join("notes.txt"), "mine").unwrap();
        assert!(matches!(
            decide(&inspect(&other).unwrap(), &c.commit),
            Action::Blocked(_)
        ));
        std::fs::remove_dir_all(&src).unwrap();
    }

    #[test]
    fn moved_tag_fails() {
        let remote = Remote::new("moved");
        // pin claims v1 is the second commit: the remote disagrees
        let c = remote.checkout(&remote.second);
        let e = format!("{:#}", verify_tag(&c).unwrap_err());
        assert!(
            e.contains(&format!("now resolves to {}", remote.first)),
            "{e}"
        );
    }

    // ---- patches/ applied by fetch -------------------------------------------------------------

    use crate::patch::fixture::*;

    fn ctx<'a>(f: &'a Fixture, src: &'a Path, execute: bool) -> Ctx<'a> {
        Ctx {
            root: &f.root,
            src,
            licenses: licenses(),
            ident: Some(ident()),
            execute,
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
        let src = f.src("src");

        let row = sync(&ctx(&f, &src, false), &c).unwrap();
        assert_eq!(row.action, Action::Clone);
        assert_eq!(row.patches.as_deref(), Some("then apply 1 patches"));
        let row = sync(&ctx(&f, &src, true), &c).unwrap();
        assert_eq!(row.patches.as_deref(), Some("applied 1 patches"));
        // the developer's tree, rebuilt from patches/ alone
        let u = f.at(&c, "src");
        assert!(on_patch_branch(&u));
        assert_eq!(tree(&u), tree(&dev));
        assert_eq!(drift_of(&u), Vec::<String>::new());
        // in step: nothing to report
        let row = sync(&ctx(&f, &src, true), &c).unwrap();
        assert_eq!((row.action, row.patches), (Action::PatchBranch, None));
    }

    #[test]
    fn fetch_refuses_to_drop_unexported_commits() {
        let f = Fixture::new("fetch-refuse");
        let c = f.checkout("v1");
        let u = f.fetched(&c, "src");
        let src = f.src("src");
        patch_commit(&u, TRAILED);
        export_ok(&u);
        patch_commit(
            &u,
            "fix b\n\nUpstream-Status: Pending\nPatch-Reason: test\n",
        );
        let before = head(&u);
        for execute in [false, true] {
            let e = sync_err(&ctx(&f, &src, execute), &c);
            assert!(
                e.contains("atl-patches holds 1 commits not in patches/up")
                    && e.contains("xtask patch --export up"),
                "{e}"
            );
        }
        assert_eq!(head(&u), before);
        // the same branch while HEAD is detached: rebuilding it would reset the branch, so refused too
        git(
            Some(&src.join("up")),
            &["checkout", "-q", "--detach", &c.commit],
        )
        .unwrap();
        let e = sync_err(&ctx(&f, &src, true), &c);
        assert!(e.contains("xtask patch --export up"), "{e}");
        assert_eq!(
            git(Some(&src.join("up")), &["rev-parse", patch::BRANCH]).unwrap(),
            before
        );
    }

    #[test]
    fn pin_bump_reapplies_or_restores() {
        let f = Fixture::new("fetch-bump");
        let c1 = f.checkout("v1");
        let u1 = f.fetched(&c1, "src");
        let src = f.src("src");
        patch_commit(&u1, TRAILED);
        export_ok(&u1);
        // in step; fetch records the base a later bump needs
        assert_eq!(sync(&ctx(&f, &src, true), &c1).unwrap().patches, None);
        assert_eq!(base_ref(&u1).as_deref(), Some(c1.commit.as_str()));

        // v2 leaves a.c alone: patches/ applies on the new pin, byte-identical
        f.write("b.c", "int b;\n");
        f.release("two", "v2");
        let c2 = f.checkout("v2");
        let row = sync(&ctx(&f, &src, true), &c2).unwrap();
        assert_eq!(row.patches.as_deref(), Some("applied 1 patches"));
        let u2 = f.at(&c2, "src");
        assert_eq!(base_ref(&u2).as_deref(), Some(c2.commit.as_str()));
        assert!(src.join("up/b.c").exists());
        assert_eq!(drift_of(&u2), Vec::<String>::new());

        // v3 rewrites the line the patch's context needs: git am fails, the checkout is put back
        f.write(
            "a.c",
            "// SPDX-License-Identifier: GPL-2.0-or-later\nint changed;\n",
        );
        f.release("three", "v3");
        let c3 = f.checkout("v3");
        let before = head(&u2);
        let e = sync_err(&ctx(&f, &src, true), &c3);
        assert!(
            e.contains("does not apply at the pin; checkout restored: xtask patch --rebase up"),
            "{e}"
        );
        assert_eq!(head(&u2), before);
        assert!(on_patch_branch(&u2));
        assert_eq!(base_ref(&u2).as_deref(), Some(c2.commit.as_str()));
    }
}
