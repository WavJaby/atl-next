// SPDX-License-Identifier: MIT
//! `xtask fetch`: upstream checkouts under `<workspace>/src` at the pins in sources.toml.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};

use crate::git::git;
use crate::sources::{self, Checkout, Pin};
use crate::workspace;

#[derive(clap::Args)]
pub struct Args {
    /// Clone missing checkouts and move clean detached ones to their pin, after checking on the remote
    /// that each tag still resolves to its pinned commit.
    #[arg(long, conflicts_with = "bump")]
    execute: bool,
    /// Resolve every tag on its remote and show where the pins would move (network, read-only).
    #[arg(long)]
    bump: bool,
    /// With --bump: write the resolved commits into sources.toml.
    #[arg(long, requires = "bump")]
    write: bool,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let all = sources::load(root)?.checkouts();
    if args.bump {
        return bump(root, &all, args.write);
    }
    let src = workspace::root_dir(root)?.join("src");
    let mut failed = 0;
    for c in &all {
        let dir = src.join(&c.dir);
        let result = (|| {
            let action = decide(&inspect(&dir)?, &c.commit);
            let shown = action.describe();
            if args.execute {
                apply(&action, c, &dir)?;
            }
            Ok::<_, anyhow::Error>(shown)
        })();
        match result {
            Ok(shown) => println!("fetch: {:<40} {shown}", c.dir.display()),
            Err(e) => {
                failed += 1;
                println!("fetch: {:<40} FAILED: {e:#}", c.dir.display());
            }
        }
    }
    if !args.execute {
        println!(
            "fetch: dry run ({} checkouts under {}); --execute applies",
            all.len(),
            src.display()
        );
    }
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

// ---- local state and the action it calls for -------------------------------------------------

#[derive(Debug)]
enum Local {
    Missing,
    Present {
        head: String,
        branch: Option<String>,
        dirty: bool,
    },
}

#[derive(Debug, PartialEq)]
enum Action {
    Clone,
    AtPin {
        dirty: bool,
    },
    MoveToPin,
    /// On a branch: `xtask patch` owns it.
    LeaveBranch(String),
    Blocked(&'static str),
}

impl Action {
    fn describe(&self) -> String {
        match self {
            Action::Clone => "missing -> clone".into(),
            Action::AtPin { dirty: false } => "at pin".into(),
            Action::AtPin { dirty: true } => "at pin, uncommitted changes".into(),
            Action::MoveToPin => "elsewhere -> move to pin".into(),
            Action::LeaveBranch(b) => format!("on branch {b} (xtask patch owns it), left alone"),
            Action::Blocked(why) => format!("blocked: {why}"),
        }
    }
}

fn decide(local: &Local, pin: &str) -> Action {
    match local {
        Local::Missing => Action::Clone,
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
    let head = git(Some(dir), &["rev-parse", "HEAD"]).context("checkout has no HEAD")?;
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
            git(Some(dir), &["remote", "add", "origin", &c.url])?;
            fetch_pin(c, dir)?;
            checkout_pin(c, dir)
        }
        Action::MoveToPin => {
            verify_tag(c)?;
            fetch_pin(c, dir)?;
            checkout_pin(c, dir)
        }
        Action::AtPin { .. } | Action::LeaveBranch(_) => Ok(()),
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
        &["fetch", "-q", "--depth", "1", "--no-tags", &c.url, &refspec],
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
    let now = remote_tag_commit(&c.url, &c.tag)?;
    ensure!(
        now == c.commit,
        "tag {} on {} now resolves to {now}, pin says {} (moved tag, or run --bump)",
        c.tag,
        c.url,
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

fn bump(root: &Path, all: &[Checkout], write: bool) -> Result<ExitCode> {
    let mut moved = Vec::new();
    let mut failed = 0;
    for c in all {
        match remote_tag_commit(&c.url, &c.tag) {
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
        let path = root.join(sources::FILE);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut doc: toml_edit::DocumentMut = text.parse().context("parsing sources.toml")?;
        for (c, now) in &moved {
            match &c.pin {
                Pin::Aosp(p) => {
                    doc["aosp"]["projects"][p.as_str()] = toml_edit::value(now.as_str())
                }
                Pin::Git(name) => doc[name.as_str()]["commit"] = toml_edit::value(now.as_str()),
            }
        }
        std::fs::write(&path, doc.to_string())
            .with_context(|| format!("writing {}", path.display()))?;
        sources::load(root).context("sources.toml after --write")?;
        println!("bump: wrote {} pins to {}", moved.len(), sources::FILE);
    } else if !moved.is_empty() {
        println!(
            "bump: {} pins would move; --write updates {}",
            moved.len(),
            sources::FILE
        );
    }
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
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
            decide(&at("p", Some("atl-patches"), true), "p"),
            Action::AtPin { dirty: true }
        );
        assert_eq!(decide(&at("x", None, false), "p"), Action::MoveToPin);
        assert_eq!(
            decide(&at("x", Some("atl-patches"), false), "p"),
            Action::LeaveBranch("atl-patches".into())
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
                url: format!("file://{}", self.dir.display()),
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
}
