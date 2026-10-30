// SPDX-License-Identifier: MIT
//! The AOSP side of fetching: the pinned manifest (project git names, the tree's link/copy files,
//! `sync-j`), and the index of every project's `Android.bp` modules used to find which projects a
//! build needs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::git::git;
use crate::sources::{self, AOSP_MANIFEST_DIR, Aosp, Sources};
use crate::{par, workspace};

/// The manifest file inside the manifest checkout.
const MANIFEST_FILE: &str = "default.xml";

pub struct Manifest {
    /// Tree path -> project.
    pub projects: BTreeMap<String, Project>,
    /// `<default revision>`: must be the tag sources.toml pins.
    pub revision: String,
    /// The manifest's own parallelism hint (`<default sync-j>`), used for fetch workers.
    pub sync_j: usize,
}

pub struct Project {
    pub name: String,
    pub links: Vec<Link>,
}

/// A `<linkfile>` (symlink) or `<copyfile>`: `src` relative to the project, `dest` to the tree root.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub src: String,
    pub dest: String,
    pub copy: bool,
}

impl Manifest {
    /// Tree path -> git name, for checkout URLs.
    pub fn names(&self) -> BTreeMap<String, String> {
        self.projects
            .iter()
            .map(|(p, pr)| (p.clone(), pr.name.clone()))
            .collect()
    }
}

/// When the parallelism hint is absent: repo's own default.
pub const DEFAULT_SYNC_J: usize = 4;

/// Only the shape the pinned AOSP manifest has: one remote at `..` (project URL = sources.toml
/// `remote` + git name), one default revision, no per-project remote/revision, no includes.
pub fn parse(xml: &str) -> Result<Manifest> {
    let doc = roxmltree::Document::parse(xml).context("manifest XML")?;
    let mut projects = BTreeMap::new();
    let mut revision = None;
    let mut sync_j = DEFAULT_SYNC_J;
    for node in doc.root_element().children().filter(|n| n.is_element()) {
        match node.tag_name().name() {
            "remote" => ensure!(
                node.attribute("fetch") == Some(".."),
                "remote fetch must be `..` (URL = remote + name)"
            ),
            "default" => {
                revision = node.attribute("revision").map(str::to_owned);
                if let Some(j) = node.attribute("sync-j") {
                    sync_j = j.parse().with_context(|| format!("sync-j `{j}`"))?;
                }
            }
            tag @ ("include" | "remove-project" | "extend-project") => {
                bail!("manifest <{tag}> is not supported")
            }
            "project" => {
                let name = node.attribute("name").context("<project> without name")?;
                let path = node.attribute("path").unwrap_or(name);
                ensure!(
                    node.attribute("remote").is_none() && node.attribute("revision").is_none(),
                    "project {path}: own remote/revision is not supported"
                );
                let links = node
                    .children()
                    .filter(|c| c.has_tag_name("linkfile") || c.has_tag_name("copyfile"))
                    .map(|c| {
                        Ok(Link {
                            src: c.attribute("src").context("link without src")?.to_owned(),
                            dest: c.attribute("dest").context("link without dest")?.to_owned(),
                            copy: c.has_tag_name("copyfile"),
                        })
                    })
                    .collect::<Result<_>>()?;
                let previous = projects.insert(
                    path.to_owned(),
                    Project {
                        name: name.to_owned(),
                        links,
                    },
                );
                ensure!(previous.is_none(), "manifest lists path {path} twice");
            }
            _ => {}
        }
    }
    let revision = revision.context("manifest has no <default revision>")?;
    Ok(Manifest {
        projects,
        revision,
        sync_j,
    })
}

/// The fetched manifest, or None while its checkout does not exist. Its default revision must be the
/// pinned tag: a manifest checkout left at another tag would name the wrong projects.
pub fn load(root: &Path, aosp: &Aosp) -> Result<Option<Manifest>> {
    let file = workspace::root_dir(root)?
        .join("src")
        .join(AOSP_MANIFEST_DIR)
        .join(MANIFEST_FILE);
    if !file.exists() {
        return Ok(None);
    }
    let xml =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let manifest = parse(&xml).with_context(|| file.display().to_string())?;
    let want = format!("refs/tags/{}", aosp.tag);
    ensure!(
        manifest.revision == want,
        "{}: default revision {} is not {want} (xtask fetch --execute)",
        file.display(),
        manifest.revision
    );
    Ok(Some(manifest))
}

/// Create the manifest's links for every project checked out under `tree`. A link already in place is
/// kept; anything else at a link's destination is an error, never overwritten.
pub fn apply_links(tree: &Path, manifest: &Manifest) -> Result<usize> {
    let mut made = 0;
    for (path, project) in &manifest.projects {
        if !tree.join(path).is_dir() {
            continue;
        }
        for link in &project.links {
            let dest = tree.join(&link.dest);
            let src = tree.join(path).join(&link.src);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if link.copy {
                let want =
                    std::fs::read(&src).with_context(|| format!("copyfile {}", src.display()))?;
                if std::fs::read(&dest).ok().as_ref() == Some(&want) {
                    continue;
                }
                ensure!(
                    !dest.exists(),
                    "{} exists with other content",
                    dest.display()
                );
                std::fs::write(&dest, want)?;
            } else {
                let target = relative(&dest, &src);
                if std::fs::read_link(&dest).ok().as_deref() == Some(target.as_path()) {
                    continue;
                }
                ensure!(
                    dest.symlink_metadata().is_err(),
                    "{} exists and is not this link",
                    dest.display()
                );
                std::os::unix::fs::symlink(&target, &dest)?;
            }
            made += 1;
        }
    }
    Ok(made)
}

/// Path of `to` relative to the directory holding `from` (both under the same root).
fn relative(from: &Path, to: &Path) -> PathBuf {
    let from_dir: Vec<_> = from
        .parent()
        .map(|p| p.components().collect())
        .unwrap_or_default();
    let to: Vec<_> = to.components().collect();
    let common = from_dir.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from_dir.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c);
    }
    out
}

// ---- the Android.bp module index -----------------------------------------------------------------

#[derive(clap::Args)]
pub struct Args {
    /// Fetch every manifest project's `Android.bp` files (shallow, sparse) and rebuild the module index.
    #[arg(long)]
    index: bool,
    /// Print the project(s) defining these modules.
    #[arg(long, value_name = "MODULE", num_args = 1..)]
    which: Vec<String>,
    /// Parallel git processes (default: the manifest's `sync-j`).
    #[arg(long)]
    jobs: Option<usize>,
}

/// The index of one tag, beside its sparse checkouts under `<workspace>/cache/aosp-bp/<tag>/`:
/// module name -> every definition.
const INDEX_FILE: &str = "index.json";

#[derive(Serialize, Deserialize)]
struct Definition {
    /// e.g. `cc_library`; tells a source module from a prebuilt or an `ndk_library` stub of one name.
    module_type: String,
    /// Tree path of the defining project.
    project: String,
    /// Tree path of the `Android.bp`.
    file: String,
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let sources = sources::load(root)?;
    let manifest =
        load(root, &sources.aosp)?.context("AOSP manifest not fetched: xtask fetch --execute")?;
    let base = workspace::root_dir(root)?
        .join("cache")
        .join("aosp-bp")
        .join(&sources.aosp.tag);
    if args.index {
        build_index(
            &sources,
            &manifest,
            &base,
            args.jobs.unwrap_or(manifest.sync_j),
        )?;
    }
    if !args.which.is_empty() {
        let index: BTreeMap<String, Vec<Definition>> = serde_json::from_str(
            &std::fs::read_to_string(base.join(INDEX_FILE))
                .context("no index: xtask aosp --index")?,
        )?;
        for m in &args.which {
            match index.get(m) {
                Some(defs) => {
                    for d in defs {
                        println!(
                            "aosp: {m:<32} {:<28} {:<40} {}",
                            d.module_type, d.project, d.file
                        );
                    }
                }
                None => println!("aosp: {m:<32} (not in any Android.bp)"),
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn build_index(sources: &Sources, manifest: &Manifest, base: &Path, jobs: usize) -> Result<()> {
    let tag = &sources.aosp.tag;
    let projects: Vec<(&String, &Project)> = manifest.projects.iter().collect();
    let results = par::map(&projects, jobs, |(path, project)| {
        sparse_bp_checkout(&base.join(path), &sources.aosp_url(&project.name), tag)
            .with_context(|| path.to_string())
    });
    let failed: Vec<String> = results
        .into_iter()
        .filter_map(|r| r.err().map(|e| format!("{e:#}")))
        .collect();
    for f in &failed {
        println!("aosp: FAILED {f}");
    }
    ensure!(
        failed.is_empty(),
        "{} of {} projects failed",
        failed.len(),
        projects.len()
    );

    let mut modules: BTreeMap<String, Vec<Definition>> = BTreeMap::new();
    let mut files = 0;
    for (path, _) in &projects {
        let mut bps = Vec::new();
        find_bp(&base.join(path), &mut bps)?;
        for bp in bps {
            files += 1;
            let text = String::from_utf8_lossy(&std::fs::read(&bp)?).into_owned();
            let file = bp.strip_prefix(base).unwrap_or(&bp).display().to_string();
            for (module_type, name) in bp_modules(&text) {
                modules.entry(name).or_default().push(Definition {
                    module_type,
                    project: (*path).clone(),
                    file: file.clone(),
                });
            }
        }
    }
    std::fs::write(
        base.join(INDEX_FILE),
        serde_json::to_string_pretty(&modules)? + "\n",
    )?;
    println!(
        "aosp: index of {} modules from {files} Android.bp in {} projects",
        modules.len(),
        projects.len()
    );
    Ok(())
}

/// A shallow, blobless checkout of `tag` holding only `Android.bp` files; kept once checked out.
fn sparse_bp_checkout(dir: &Path, url: &str, tag: &str) -> Result<()> {
    let tag_ref = format!("refs/tags/{tag}");
    if dir.join(".git").exists() && git(Some(dir), &["rev-parse", "-q", "--verify", "HEAD"]).is_ok()
    {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let g = |args: &[&str]| git(Some(dir), args);
    g(&["init", "-q"])?;
    g(&["remote", "add", "origin", url]).or_else(|_| g(&["remote", "set-url", "origin", url]))?;
    // lazy blob fetch on checkout needs the remote marked as a partial-clone promisor
    g(&["config", "remote.origin.promisor", "true"])?;
    g(&["config", "remote.origin.partialclonefilter", "blob:none"])?;
    g(&["config", "core.sparseCheckout", "true"])?;
    std::fs::write(dir.join(".git/info/sparse-checkout"), "Android.bp\n")?;
    g(&[
        "fetch",
        "-q",
        "--depth",
        "1",
        "--filter=blob:none",
        "origin",
        &format!("+{tag_ref}:{tag_ref}"),
    ])?;
    g(&[
        "-c",
        "advice.detachedHead=false",
        "checkout",
        "-q",
        "--detach",
        &tag_ref,
    ])?;
    Ok(())
}

/// `Android.bp` files of the project at `dir`; a subdirectory holding `.git` is a nested project, its own.
fn find_bp(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_symlink() {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if !path.join(".git").exists() && path.file_name().is_some_and(|n| n != ".git") {
                find_bp(&path, out)?;
            }
        } else if path.file_name().is_some_and(|n| n == "Android.bp") {
            out.push(path);
        }
    }
    Ok(())
}

// ---- Blueprint: module names only ------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Punct(char),
}

fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '/' if chars.peek() == Some(&'/') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
            }
            '"' | '`' => {
                let mut s = String::new();
                while let Some(d) = chars.next() {
                    if d == '\\' && c == '"' {
                        if let Some(e) = chars.next() {
                            s.push(e);
                        }
                    } else if d == c {
                        break;
                    } else {
                        s.push(d);
                    }
                }
                out.push(Token::Str(s));
            }
            c if c.is_alphanumeric() || c == '_' => {
                let mut s = c.to_string();
                while let Some(&d) = chars.peek() {
                    if d.is_alphanumeric() || d == '_' {
                        s.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(Token::Ident(s));
            }
            other => out.push(Token::Punct(other)),
        }
    }
    out
}

/// `(module type, name)` of top-level modules (`type { name: "x", … }`); variable assignments, nested
/// maps and comments are skipped.
fn bp_modules(text: &str) -> Vec<(String, String)> {
    let toks = tokens(text);
    let mut out = Vec::new();
    let mut depth = 0usize;
    // type of the module whose body is open at depth 1; None inside `x = { … }` or a list
    let mut module_type: Option<&str> = None;
    for (i, t) in toks.iter().enumerate() {
        match t {
            Token::Punct(open @ ('{' | '[' | '(')) => {
                if depth == 0 {
                    module_type = match i.checked_sub(1).map(|j| &toks[j]) {
                        Some(Token::Ident(ty)) if *open == '{' => Some(ty),
                        _ => None,
                    };
                }
                depth += 1;
            }
            Token::Punct('}' | ']' | ')') => depth = depth.saturating_sub(1),
            Token::Ident(id) if depth == 1 && id == "name" => {
                // a plain string literal only: `"a" + v` is not a name this index can know
                if let (Some(ty), Some(Token::Punct(':')), Some(Token::Str(s))) =
                    (module_type, toks.get(i + 1), toks.get(i + 2))
                    && toks.get(i + 3) != Some(&Token::Punct('+'))
                {
                    out.push((ty.to_owned(), s.clone()));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_projects_links_and_defaults() {
        // shape of AOSP's default.xml (repo manifest format)
        let xml = r#"<manifest>
  <remote name="aosp" fetch=".." />
  <default revision="refs/tags/t" remote="aosp" sync-j="4" />
  <project path="build/make" name="platform/build">
    <linkfile src="CleanSpec.mk" dest="build/CleanSpec.mk" />
    <copyfile src="core/root.mk" dest="Makefile" />
  </project>
  <project name="platform/art" path="art" />
  <project name="tools/repohooks" />
</manifest>"#;
        let m = parse(xml).unwrap();
        assert_eq!(m.sync_j, 4);
        assert_eq!(m.projects["build/make"].name, "platform/build");
        assert_eq!(m.projects["tools/repohooks"].name, "tools/repohooks");
        assert_eq!(
            m.projects["build/make"].links,
            vec![
                Link {
                    src: "CleanSpec.mk".into(),
                    dest: "build/CleanSpec.mk".into(),
                    copy: false
                },
                Link {
                    src: "core/root.mk".into(),
                    dest: "Makefile".into(),
                    copy: true
                },
            ]
        );
        assert_eq!(m.revision, "refs/tags/t");
        assert!(parse("<manifest><include name=\"x.xml\"/></manifest>").is_err());
        let own_revision =
            r#"<manifest><default revision="r"/><project name="a" revision="x"/></manifest>"#;
        assert!(parse(own_revision).is_err());
    }

    #[test]
    fn links_applied_once_and_never_clobber() {
        let tree = std::env::temp_dir().join(format!("xtask-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tree);
        std::fs::create_dir_all(tree.join("build/make/core")).unwrap();
        std::fs::write(tree.join("build/make/core/root.mk"), "x").unwrap();
        std::fs::write(tree.join("build/make/CleanSpec.mk"), "y").unwrap();
        let m = parse(r#"<manifest><default revision="r"/><project path="build/make" name="b"><linkfile src="CleanSpec.mk" dest="build/CleanSpec.mk"/><copyfile src="core/root.mk" dest="Makefile"/></project></manifest>"#).unwrap();
        assert_eq!(apply_links(&tree, &m).unwrap(), 2);
        assert_eq!(
            std::fs::read_to_string(tree.join("build/CleanSpec.mk")).unwrap(),
            "y"
        );
        assert_eq!(
            std::fs::read_link(tree.join("build/CleanSpec.mk")).unwrap(),
            PathBuf::from("make/CleanSpec.mk")
        );
        assert_eq!(apply_links(&tree, &m).unwrap(), 0);
        std::fs::write(tree.join("Makefile"), "changed").unwrap();
        assert!(apply_links(&tree, &m).is_err());
        std::fs::remove_dir_all(&tree).unwrap();
    }

    #[test]
    fn blueprint_module_names() {
        let bp = r#"
// comment { name: "not_this" }
cc_defaults = ["x"]
vars = { name: "not_a_module" }
cc_library {
    name: "libc",
    srcs: ["a.c", "b{c}.c"], /* name: "nor_this" */
    arch: { arm: { name: "nested_not" } },
}
genrule {
    cmd: "echo \"}\"",
    name: "gen",
}
cc_test { name: "pre" + suffix }
soong_namespace {}
"#;
        let pair = |t: &str, n: &str| (t.to_owned(), n.to_owned());
        assert_eq!(
            bp_modules(bp),
            vec![pair("cc_library", "libc"), pair("genrule", "gen")]
        );
    }
}
