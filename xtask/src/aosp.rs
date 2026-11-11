// SPDX-License-Identifier: MIT
//! The AOSP side of fetching: the pinned manifest (project git names, the tree's link/copy files,
//! `sync-j`), and the index of every project's `Android.bp` modules used to find which projects a
//! build needs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use atl_layout::Layout;

use crate::git::git;
use crate::par;
use crate::sources::{self, AOSP_MANIFEST_DIR, Aosp, Sources};

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

    /// The project a tree path belongs to: the longest project path that is it or a parent of it.
    pub fn project_of(&self, path: &str) -> Option<&str> {
        let mut at = path;
        loop {
            if let Some((p, _)) = self.projects.get_key_value(at) {
                return Some(p);
            }
            at = &at[..at.rfind('/')?];
        }
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
    let file = Layout::of(root)?
        .locate(&Path::new(AOSP_MANIFEST_DIR).join(MANIFEST_FILE))
        .dir;
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
pub fn relative(from: &Path, to: &Path) -> PathBuf {
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

/// The index of one tag, beside its sparse checkouts under `<workspace>/cache/aosp-bp/<tag>/`.
const INDEX_FILE: &str = "index.json";

/// Whether a prebuilt definition may answer a reference: Soong can use one when no source module of
/// the name exists; our converter never builds prebuilts (binary VNDK snapshots would be fetched for
/// nothing).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prebuilts {
    Fallback,
    Never,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Index {
    /// Module name -> every definition.
    modules: BTreeMap<String, Vec<Definition>>,
    /// Module type -> projects whose `soong_build` plugin registers it (outside build/soong, a type
    /// only exists when its plugin's project is in the tree).
    module_types: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Definition {
    /// e.g. `cc_library`; tells a source module from a prebuilt or an `ndk_library` stub of one name.
    module_type: String,
    /// Tree path of the defining project.
    project: String,
    /// Tree path of the `Android.bp`.
    file: String,
    /// Directory of the enclosing `soong_namespace`; absent = the root namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    /// Its `defaults` list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    defaults: Vec<String>,
    /// A `license` module's `license_kinds`, empty if it lists none; None for other modules (and in an index built before kinds were recorded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    license_kinds: Option<Vec<String>>,
}

impl Definition {
    /// A binary drop rather than a build from source (Soong prefers the source module of a name).
    fn is_prebuilt(&self) -> bool {
        let t = &self.module_type;
        t.contains("prebuilt")
            || t.contains("_import")
            || t.contains("snapshot")
            || self.project.starts_with("prebuilts/")
    }
}

fn index_dir(root: &Path, aosp: &Aosp) -> Result<PathBuf> {
    Ok(Layout::of(root)?
        .workspace()
        .join("cache")
        .join("aosp-bp")
        .join(&aosp.tag))
}

pub fn index_file(root: &Path, aosp: &Aosp) -> Result<PathBuf> {
    Ok(index_dir(root, aosp)?.join(INDEX_FILE))
}

impl Index {
    pub fn load(root: &Path, aosp: &Aosp) -> Result<Index> {
        Index::read(&index_file(root, aosp)?)
    }

    /// An index of `license` modules only: (name, tree path of its blueprint file, kinds).
    #[cfg(test)]
    pub(crate) fn of_licenses(defs: &[(&str, &str, &[&str])]) -> Index {
        let mut modules: BTreeMap<String, Vec<Definition>> = BTreeMap::new();
        for (name, file, kinds) in defs {
            modules
                .entry((*name).to_owned())
                .or_default()
                .push(Definition {
                    module_type: "license".into(),
                    project: parent_dir(file).into(),
                    file: (*file).into(),
                    namespace: None,
                    defaults: vec![],
                    license_kinds: Some(kinds.iter().map(|k| (*k).to_owned()).collect()),
                });
        }
        Index {
            modules,
            module_types: BTreeMap::new(),
        }
    }

    /// An index of plain modules: (name, module type, tree path of its blueprint file).
    #[cfg(test)]
    pub(crate) fn of_modules(defs: &[(&str, &str, &str)]) -> Index {
        let mut modules: BTreeMap<String, Vec<Definition>> = BTreeMap::new();
        for (name, module_type, file) in defs {
            modules
                .entry((*name).to_owned())
                .or_default()
                .push(Definition {
                    module_type: (*module_type).into(),
                    project: parent_dir(file).into(),
                    file: (*file).into(),
                    namespace: None,
                    defaults: vec![],
                    license_kinds: None,
                });
        }
        Index {
            modules,
            module_types: BTreeMap::new(),
        }
    }

    pub fn read(file: &Path) -> Result<Index> {
        let text = std::fs::read_to_string(file)
            .with_context(|| format!("no index {}: xtask aosp --index", file.display()))?;
        serde_json::from_str(&text).with_context(|| file.display().to_string())
    }

    /// Every definition of `name` as (module type, tree path of its `Android.bp`).
    pub fn definitions_of(&self, name: &str) -> Vec<(&str, &str)> {
        self.modules.get(name).map_or_else(Vec::new, |defs| {
            defs.iter()
                .map(|d| (d.module_type.as_str(), d.file.as_str()))
                .collect()
        })
    }

    /// The project to fetch for a dependency reference as Soong prints it (`name`, `:name`,
    /// `name{.tag}`, `//dir:name`). `//dir:name` names a soong_namespace or the package directory of a
    /// root-namespace module (`//external/llvm-libc:llvmlibc`). A bare name prefers the root namespace,
    /// then a source module over a prebuilt; a name still left in several projects is ambiguous, never
    /// guessed.
    pub fn project_for(&self, reference: &str) -> Result<&str, String> {
        self.resolve(reference, Prebuilts::Fallback)
    }

    fn resolve(&self, reference: &str, prebuilts: Prebuilts) -> Result<&str, String> {
        let reference = reference.trim_start_matches(':');
        let reference = reference.split('{').next().unwrap_or(reference);
        let (dir, name) = match reference.strip_prefix("//") {
            Some(r) => match r.split_once(':') {
                Some((dir, name)) => (Some(dir), name),
                None => return Err(format!("{reference}: not a module reference")),
            },
            None => (None, reference),
        };
        // a module a generator creates during analysis (`java_sdk_library` X makes `X.stubs`,
        // `X.stubs.source.api.contribution`, …) is in no Android.bp: its longest dotted prefix is.
        // The modules of an aidl_interface (`foo-V2-cpp`), a hidl_interface (`a.b@1.0_genc++`) and a
        // sysprop_library (`libFoo`) come first: their base names are themselves dotted
        let owned_by = |base: Option<&str>, module_type: &str| {
            base.and_then(|b| self.modules.get(b))
                .filter(|defs| defs.iter().any(|d| d.module_type == module_type))
        };
        let generated = owned_by(aidl_interface_of(name), "aidl_interface")
            .or_else(|| owned_by(hidl_interface_of(name), "hidl_interface"))
            .or_else(|| owned_by(name.strip_prefix("lib"), "sysprop_library"));
        let defs = generated
            .or_else(|| {
                std::iter::successors(Some(name), |n| n.rsplit_once('.').map(|(head, _)| head))
                    .find_map(|n| self.modules.get(n))
            })
            .ok_or("not in any Android.bp")?;
        let mut cands: Vec<&Definition> = match dir {
            Some(dir) => defs
                .iter()
                .filter(|d| d.namespace.as_deref() == Some(dir) || parent_dir(&d.file) == dir)
                .collect(),
            None => {
                let root: Vec<_> = defs.iter().filter(|d| d.namespace.is_none()).collect();
                if root.is_empty() {
                    defs.iter().collect()
                } else {
                    root
                }
            }
        };
        if prebuilts == Prebuilts::Never || cands.iter().any(|d| !d.is_prebuilt()) {
            cands.retain(|d| !d.is_prebuilt());
        }
        let projects: BTreeSet<&str> = cands.iter().map(|d| d.project.as_str()).collect();
        match projects.len() {
            1 => Ok(projects.into_iter().next().unwrap_or_default()),
            0 => Err(format!(
                "no definition in namespace or package //{}",
                dir.unwrap_or_default()
            )),
            _ => Err(format!(
                "ambiguous: {}",
                projects.into_iter().collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// Unpinned project -> why it is wanted, for module references that are missing from the tree
    /// (`(reference, why)` pairs, `why` echoed back per project: first reference wins); plus what could
    /// not be mapped to a project to fetch. A reference defined in a pinned project is a different
    /// problem (a lookup rule of the consumer, not an absent checkout), so it is unresolved, not wanted.
    pub fn unpinned_projects<'a>(
        &self,
        refs: impl IntoIterator<Item = (&'a str, String)>,
        pinned: &BTreeSet<&str>,
        prebuilts: Prebuilts,
    ) -> (BTreeMap<String, String>, Vec<String>) {
        let mut want = BTreeMap::new();
        let mut unresolved = Vec::new();
        for (module, why) in refs {
            match self.resolve(module, prebuilts) {
                Ok(p) if !pinned.contains(p) => {
                    want.entry(p.to_owned()).or_insert(why);
                }
                Ok(p) => unresolved.push(format!("{module}: defined in pinned {p}, yet missing")),
                Err(e) => unresolved.push(format!("{module}: {e}")),
            }
        }
        (want, unresolved)
    }

    /// `license_kinds` of the `license` module a `licenses` entry in blueprint directory `from` (tree path) names (`license_of`).
    pub fn license_kinds_of(&self, reference: &str, from: &str) -> Result<&[String], String> {
        let def = self.license_of(reference, from)?;
        match &def.license_kinds {
            None => Err(
                "the AOSP index predates licence kinds (xtask aosp --index rebuilds it)".to_owned(),
            ),
            // e.g. harfbuzz_ng's `unused.HPND-sell-variant`, its one kind commented out: its licence is unknown, not absent
            Some(k) if k.is_empty() => Err(format!("{} lists no licence kind", def.file)),
            Some(k) => Ok(k),
        }
    }

    /// The `license` module a `licenses` entry in blueprint directory `from` (tree path) names, found as Soong's licensesPropertyGatherer does: `//dir:name` as written; else `name` in the nearest of `from` and its ancestors defining it; else the one definition anywhere.
    fn license_of(&self, reference: &str, from: &str) -> Result<&Definition, String> {
        let (dir, name) = split_reference(reference)?;
        let defs: Vec<&Definition> = self
            .modules
            .get(name)
            .into_iter()
            .flatten()
            .filter(|d| d.module_type == "license")
            .collect();
        let within = |d: &Definition| {
            let at = parent_dir(&d.file);
            at.is_empty() || from == at || from.starts_with(&format!("{at}/"))
        };
        match dir {
            Some(dir) => defs.iter().find(|d| parent_dir(&d.file) == dir),
            None => defs
                .iter()
                .filter(|d| within(d))
                .max_by_key(|d| parent_dir(&d.file).len())
                .or(match defs.as_slice() {
                    [one] => Some(one),
                    _ => None,
                }),
        }
        .copied()
        .ok_or_else(|| match defs.len() {
            0 => "no license module of that name".to_owned(),
            n => format!("{n} license modules of that name, none enclosing it"),
        })
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let sources = sources::load(root)?;
    let manifest =
        load(root, &sources.aosp)?.context("AOSP manifest not fetched: xtask fetch --execute")?;
    if args.index {
        build_index(
            &sources,
            &manifest,
            &index_dir(root, &sources.aosp)?,
            args.jobs.unwrap_or(manifest.sync_j),
        )?;
    }
    if !args.which.is_empty() {
        let index = Index::load(root, &sources.aosp)?;
        for m in &args.which {
            let chosen = match index.project_for(m) {
                Ok(p) => format!("-> {p}"),
                Err(e) => format!("-> none ({e})"),
            };
            println!("aosp: {m:<32} {chosen}");
            for d in index.modules.get(m).into_iter().flatten() {
                let ns = d
                    .namespace
                    .as_deref()
                    .map(|n| format!(" (//{n})"))
                    .unwrap_or_default();
                println!(
                    "aosp:   {:<28} {:<40} {}{ns}",
                    d.module_type, d.project, d.file
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Sparse checkouts of `projects` holding (at least) files matching `patterns`; all or an error.
fn sparse_checkouts(
    sources: &Sources,
    base: &Path,
    projects: &[(&String, &Project)],
    patterns: &[&str],
    jobs: usize,
) -> Result<()> {
    let results = par::map(projects, jobs, |(path, project)| {
        let url = sources.aosp_url(&project.name);
        sparse_checkout(
            base,
            Path::new(path.as_str()),
            &url,
            &sources.aosp.tag,
            patterns,
        )
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
    Ok(())
}

fn build_index(sources: &Sources, manifest: &Manifest, base: &Path, jobs: usize) -> Result<()> {
    let projects: Vec<(&String, &Project)> = manifest.projects.iter().collect();
    sparse_checkouts(sources, base, &projects, &[BLUEPRINT_FILES], jobs)?;

    // (project, tree path of the blueprint file, its modules): every Android.bp, plus the files its
    // top-level `build = [...]` pulls in (e.g. `Android.gen.bp`), the set Soong reads
    let mut files = Vec::new();
    for (path, _) in &projects {
        let mut bps = Vec::new();
        find_files(&base.join(path), &|n| n == "Android.bp", &mut bps)?;
        while let Some(bp) = bps.pop() {
            let text = String::from_utf8_lossy(&std::fs::read(&bp)?).into_owned();
            let parsed = parse_bp(&text);
            if let Some(dir) = bp.parent() {
                bps.extend(
                    parsed
                        .build
                        .iter()
                        .map(|f| dir.join(f))
                        .filter(|f| f.is_file()),
                );
            }
            let file = bp.strip_prefix(base).unwrap_or(&bp).display().to_string();
            files.push((*path, file, parsed.modules));
        }
    }
    // a `soong_namespace` makes its directory and everything below it (up to a nested one) a namespace
    let namespaces: BTreeSet<&str> = files
        .iter()
        .filter(|(_, _, mods)| mods.iter().any(|m| m.module_type == "soong_namespace"))
        .map(|(_, file, _)| parent_dir(file))
        .collect();
    let mut modules: BTreeMap<String, Vec<Definition>> = BTreeMap::new();
    for (project, file, mods) in &files {
        let namespace = enclosing(&namespaces, parent_dir(file)).map(str::to_owned);
        for m in mods {
            if let Some(name) = &m.name {
                modules.entry(name.clone()).or_default().push(Definition {
                    module_type: m.module_type.clone(),
                    project: (*project).clone(),
                    file: file.clone(),
                    namespace: namespace.clone(),
                    defaults: m.defaults.clone(),
                    license_kinds: (m.module_type == "license").then(|| m.license_kinds.clone()),
                });
            }
        }
    }
    // module types: registered by the Go code of projects holding a soong_build plugin package
    // (project, Go file compiled into a soong_build plugin package)
    let plugin_srcs: Vec<(&String, String)> = files
        .iter()
        .flat_map(|(project, file, mods)| {
            mods.iter()
                .filter(|m| m.is_soong_plugin())
                .flat_map(move |m| {
                    m.srcs
                        .iter()
                        .map(move |s| (*project, format!("{}/{s}", parent_dir(file))))
                })
        })
        .collect();
    let plugin_projects: BTreeSet<&String> = plugin_srcs.iter().map(|(p, _)| *p).collect();
    let plugins: Vec<(&String, &Project)> = projects
        .iter()
        .filter(|(p, _)| plugin_projects.contains(p))
        .copied()
        .collect();
    sparse_checkouts(sources, base, &plugins, &[BLUEPRINT_FILES, GO_FILES], jobs)?;
    let mut module_types: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (project, src) in &plugin_srcs {
        // a glob or generated source is not a file here: it registers nothing this index can see
        let Ok(bytes) = std::fs::read(base.join(src)) else {
            continue;
        };
        for t in registered_types(&String::from_utf8_lossy(&bytes)) {
            module_types
                .entry(t)
                .or_default()
                .insert((*project).clone());
        }
    }
    let index = Index {
        modules,
        module_types,
    };
    std::fs::write(
        base.join(INDEX_FILE),
        serde_json::to_string_pretty(&index)? + "\n",
    )?;
    println!(
        "aosp: index from {} blueprint files ({} namespaces) in {} projects; {} module types from {} plugin projects",
        files.len(),
        namespaces.len(),
        projects.len(),
        index.module_types.len(),
        plugins.len()
    );
    Ok(())
}

/// A Soong package and the licences its blueprint files declare, references unresolved.
struct Package {
    /// Tree path of the package directory.
    tree: PathBuf,
    /// Its own `license` modules.
    own: Vec<BpModule>,
    /// `licenses` / `default_applicable_licenses` entries of its modules: `license` modules defined wherever (`build/soong/licenses`' `Android-Apache-2.0`).
    refs: BTreeSet<String>,
}

/// The Soong package holding directory `at` (inside the checkout `dir` of AOSP project `project`, a tree path): the nearest of `at` and its ancestors up to `dir` whose `Android.bp` (+ its `build = [...]` files) declares a licence -- a `license` module with kinds, or a licence reference.
/// Soong gives a module its package's default, and one project may hold hundreds of packages under different licences (external/rust/android-crates-io: a crate each). Inner `Err`: no package declares a licence.
/// `seen` gets what the answer depends on: each blueprint read, and the directory of each one absent (its entries say when one appears).
fn package(
    dir: &Path,
    project: &Path,
    at: &Path,
    seen: &mut dyn FnMut(&Path),
) -> Result<Result<Package, String>> {
    let mut pkg = dir.join(at);
    loop {
        let mut bps = vec![pkg.join("Android.bp")];
        let mut own = Vec::new();
        let mut refs = BTreeSet::new();
        while let Some(bp) = bps.pop() {
            let Ok(bytes) = std::fs::read(&bp) else {
                seen(bp.parent().unwrap_or(&pkg));
                continue;
            };
            seen(&bp);
            let parsed = parse_bp(&String::from_utf8_lossy(&bytes));
            bps.extend(
                parsed
                    .build
                    .iter()
                    .map(|f| pkg.join(f))
                    .filter(|f| f.is_file() && !f.ends_with("Android.bp")),
            );
            for mut m in parsed.modules {
                refs.extend(std::mem::take(&mut m.licenses));
                if m.module_type == "license" {
                    own.push(m);
                }
            }
        }
        let tree = match pkg.strip_prefix(dir) {
            Ok(rel) if !rel.as_os_str().is_empty() => project.join(rel),
            _ => project.to_path_buf(),
        };
        if own.iter().any(|m| !m.license_kinds.is_empty()) || !refs.is_empty() {
            return Ok(Ok(Package { tree, own, refs }));
        }
        if pkg == dir || !pkg.pop() || !pkg.starts_with(dir) {
            return Ok(Err(format!(
                "no Android.bp from {} up to {} declares a licence",
                lexical(&project.join(at)).display(),
                project.display()
            )));
        }
    }
}

/// The Soong package holding directory `at` (`package`) and its licence kinds: those of its `license` modules plus those of every `license` module it references (`Index::license_of`).
/// Returns the package's tree path; inner `Err`: no package declares a licence, or a reference the index cannot resolve to one `license` module with kinds. `seen`: as `package`'s.
pub fn package_kinds(
    dir: &Path,
    project: &Path,
    at: &Path,
    index: &Index,
    seen: &mut dyn FnMut(&Path),
) -> Result<Result<(PathBuf, BTreeSet<String>), String>> {
    let p = match package(dir, project, at, seen)? {
        Ok(p) => p,
        Err(why) => return Ok(Err(why)),
    };
    let mut kinds: BTreeSet<String> = p
        .own
        .iter()
        .flat_map(|m| m.license_kinds.iter().cloned())
        .collect();
    let from = p.tree.display().to_string();
    for name in &p.refs {
        match index.license_kinds_of(name, &from) {
            Ok(k) => kinds.extend(k.iter().cloned()),
            Err(e) => return Ok(Err(format!("licence {name} (from {from}): {e}"))),
        }
    }
    Ok(Ok((p.tree, kinds)))
}

/// The licence texts of the Soong package holding directory `at` (`package`): the `license_text` files of every `license` module applying to it, own and referenced (resolved as `package_kinds` resolves them), by tree path.
/// `read` returns a file of the AOSP tree by tree path, None when absent: a referenced module's blueprint and texts may sit in another project (`Android-Apache-2.0` -> build/soong/licenses/LICENSE). A referenced module's `license_text` is read from its blueprint, not the index, which records only kinds.
/// Inner `Err`: no package declares a licence, a reference does not resolve, a `license` module lists no `license_text` (its text would be missing from the copy) or names a module, or a listed file is absent.
pub fn package_texts(
    dir: &Path,
    project: &Path,
    at: &Path,
    index: &Index,
    read: &dyn Fn(&Path) -> Result<Option<String>>,
) -> Result<Result<BTreeMap<PathBuf, String>, String>> {
    let p = match package(dir, project, at, &mut |_| {})? {
        Ok(p) => p,
        Err(why) => return Ok(Err(why)),
    };
    let from = p.tree.display().to_string();
    // (directory its texts are relative to, `<name> in <blueprint dir>` for messages, its license_text)
    let mut modules: Vec<(PathBuf, String, Vec<String>)> = p
        .own
        .into_iter()
        .map(|m| {
            let what = format!("{} in {from}", m.name.as_deref().unwrap_or("(unnamed)"));
            (p.tree.clone(), what, m.license_text)
        })
        .collect();
    for reference in &p.refs {
        let def = match index.license_of(reference, &from) {
            Ok(d) => d,
            Err(e) => return Ok(Err(format!("licence {reference} (from {from}): {e}"))),
        };
        let what = format!("{reference} in {}", def.file);
        let Some(bp) = read(Path::new(&def.file))? else {
            return Ok(Err(format!("license module {what}: blueprint absent")));
        };
        let name = split_reference(reference).map_or(reference.as_str(), |(_, n)| n);
        let Some(m) = parse_bp(&bp)
            .modules
            .into_iter()
            .find(|m| m.module_type == "license" && m.name.as_deref() == Some(name))
        else {
            return Ok(Err(format!("license module {what}: not in that blueprint")));
        };
        modules.push((PathBuf::from(parent_dir(&def.file)), what, m.license_text));
    }
    let mut texts = BTreeMap::new();
    for (base, what, list) in modules {
        if list.is_empty() {
            return Ok(Err(format!("license module {what} lists no license_text")));
        }
        for t in list {
            if t.starts_with(':') {
                return Ok(Err(format!(
                    "license module {what}: license_text {t} names a module, not a file"
                )));
            }
            let path = lexical(&base.join(&t));
            match read(&path)? {
                Some(text) => texts.insert(path, text),
                None => {
                    return Ok(Err(format!(
                        "license module {what}: license_text {} absent",
                        path.display()
                    )));
                }
            };
        }
    }
    Ok(Ok(texts))
}

/// A `licenses` entry as (`//dir` part, module name): `//dir:name` or `name`.
fn split_reference(reference: &str) -> Result<(Option<&str>, &str), String> {
    match reference.strip_prefix("//") {
        Some(r) => r
            .split_once(':')
            .map(|(d, n)| (Some(d), n))
            .ok_or_else(|| "not a module reference".to_owned()),
        None => Ok((None, reference)),
    }
}

/// `p` with `.` and `dir/..` folded away, without touching the filesystem (a tree path, not a host path).
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// TARGET_RELEASE of the pinned tag: AOSP release builds use the release config named by the build
/// ID's first field (`BP2A.250605.031.A2` -> `bp2a`). Kati rejects a name build/release lacks.
pub fn target_release(tree: &Path) -> Result<String> {
    let file = tree.join("build/make/core/build_id.mk");
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let id = text
        .lines()
        .find_map(|l| l.strip_prefix("BUILD_ID="))
        .with_context(|| format!("no BUILD_ID= in {}", file.display()))?;
    let field = id.split('.').next().unwrap_or(id);
    Ok(field.to_lowercase())
}

/// Module types a Go file registers: `Register…ModuleType("name", …)` with a literal name.
fn registered_types(go: &str) -> Vec<String> {
    const CALL: &str = "ModuleType(\"";
    let mut out = Vec::new();
    let mut rest = go;
    while let Some(i) = rest.find(CALL) {
        let before = &rest[..i];
        let word = &before[before
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |p| p + 1)..];
        rest = &rest[i + CALL.len()..];
        if word.starts_with("Register")
            && let Some(end) = rest.find('"')
        {
            out.push(rest[..end].to_owned());
        }
    }
    out
}

fn parent_dir(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Backend suffixes of an aidl_interface (system/tools/aidl/build/aidl_interface.go `lang*`, longest first).
const AIDL_BACKENDS: [&str; 6] = [
    "-cpp-analyzer",
    "-ndk_platform",
    "-cpp",
    "-ndk",
    "-java",
    "-rust",
];

/// `foo` of a backend name `foo[-V<n>]-<backend>` an aidl_interface `foo` creates (the caller checks it is one).
fn aidl_interface_of(name: &str) -> Option<&str> {
    let base = AIDL_BACKENDS
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))?;
    let digits = base.len() - base.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (head, _) = base.split_at(base.len() - digits);
    Some(match head.strip_suffix("-V") {
        Some(iface) if digits > 0 => iface,
        _ => base,
    })
}

/// Suffixes of the modules a hidl_interface `<pkg>@<ver>` creates that the converter builds (system/tools/hidl/build/fqName.go `sourcesName`,
/// `headersName`, `fileGroupName`; longest first). The cc library has the interface's own name.
const HIDL_GENERATED: [&str; 3] = ["_genc++_headers", "_genc++", "_hal"];

/// `pkg@ver` of a name `pkg@ver_genc++` & co. a hidl_interface creates (the caller checks it is one).
fn hidl_interface_of(name: &str) -> Option<&str> {
    let base = HIDL_GENERATED
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))?;
    base.contains('@').then_some(base)
}

/// The innermost namespace directory that is `dir` or one of its parents.
fn enclosing<'a>(namespaces: &BTreeSet<&'a str>, dir: &str) -> Option<&'a str> {
    let mut at = dir;
    loop {
        if let Some(ns) = namespaces.get(at) {
            return Some(ns);
        }
        at = &at[..at.rfind('/')?];
    }
}

/// Non-cone sparse patterns: every blueprint file (`Android.bp` and what its `build = [...]` names);
/// Go sources, only for projects holding a soong_build plugin.
const BLUEPRINT_FILES: &str = "*.bp";
const GO_FILES: &str = "*.go";

/// A shallow, blobless checkout `<base>/<path>` of `tag` holding only files matching `patterns`, its git dir in `<base>`'s store (atl_layout::git_store, as `src/`). An existing one is
/// only widened, so a later narrower request never drops what another pass added.
fn sparse_checkout(
    base: &Path,
    path: &Path,
    url: &str,
    tag: &str,
    patterns: &[&str],
) -> Result<()> {
    let dir = base.join(path);
    let g = |args: &[&str]| git(Some(&dir), args);
    if dir.join(".git").exists() && g(&["rev-parse", "-q", "--verify", "HEAD"]).is_ok() {
        let sparse_file = g(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/sparse-checkout",
        ])?;
        let current = std::fs::read_to_string(&sparse_file).unwrap_or_default();
        let mut set: BTreeSet<&str> = current.lines().collect();
        if !patterns.iter().all(|p| set.contains(p)) {
            set.extend(patterns);
            let text = set.iter().map(|p| format!("{p}\n")).collect::<String>();
            std::fs::write(&sparse_file, text)?;
            g(&["sparse-checkout", "reapply"])?;
        }
        return Ok(());
    }
    crate::fetch::create_checkout(
        &dir,
        &atl_layout::git_store(base, path),
        url,
        &format!("refs/tags/{tag}"),
        &crate::fetch::Sparse::Patterns(patterns),
        None,
    )?;
    Ok(())
}

/// Files of the project at `dir` whose name is `wanted`; a subdirectory holding `.git` is a nested
/// project, its own.
fn find_files(dir: &Path, wanted: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) -> Result<()> {
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
                find_files(&path, wanted, out)?;
            }
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(wanted)
        {
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

#[derive(Debug, PartialEq, Default)]
struct BpModule {
    module_type: String,
    /// A plain string literal only: `"a" + v` is not a name this index can know.
    name: Option<String>,
    /// `pluginFor: [...]` of a `bootstrap_go_package`: `soong_build` = its Go code registers module types.
    plugin_for: Vec<String>,
    /// `srcs: [...]` as literal strings (for a Go package: the files compiled into it).
    srcs: Vec<String>,
    /// `defaults: [...]`: modules whose properties this one inherits.
    defaults: Vec<String>,
    /// `license_kinds: [...]` of a `license` module.
    license_kinds: Vec<String>,
    /// `license_text: [...]` of a `license` module: its licence text files, relative to its directory.
    license_text: Vec<String>,
    /// `licenses: [...]` of a module, `default_applicable_licenses: [...]` of `package`: the `license` modules that apply.
    licenses: Vec<String>,
}

impl BpModule {
    fn is_soong_plugin(&self) -> bool {
        self.module_type == "bootstrap_go_package"
            && self.plugin_for.iter().any(|p| p == "soong_build")
    }
}

/// The string literals of a `[ ... ]` list starting at `toks[at]`, if it is one.
fn string_list(toks: &[Token], at: usize) -> Option<Vec<String>> {
    if toks.get(at) != Some(&Token::Punct('[')) {
        return None;
    }
    Some(
        toks[at + 1..]
            .iter()
            .take_while(|t| **t != Token::Punct(']'))
            .filter_map(|t| match t {
                Token::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
    )
}

#[derive(Debug, PartialEq)]
struct BpFile {
    /// Top-level modules (`type { name: "x", … }`).
    modules: Vec<BpModule>,
    /// Top-level `build = ["a.bp", …]`: more blueprint files of this directory Soong reads.
    build: Vec<String>,
}

/// Modules and `build` files of a blueprint file; other assignments, nested maps and comments are
/// skipped.
fn parse_bp(text: &str) -> BpFile {
    let toks = tokens(text);
    let mut build = Vec::new();
    let mut out: Vec<BpModule> = Vec::new();
    let mut depth = 0usize;
    // whether the block open at depth 1 is a module body, not `x = { … }` or a list
    let mut in_module = false;
    for (i, t) in toks.iter().enumerate() {
        match t {
            Token::Punct(open @ ('{' | '[' | '(')) => {
                if depth == 0 {
                    in_module = match i.checked_sub(1).map(|j| &toks[j]) {
                        Some(Token::Ident(ty)) if *open == '{' => {
                            out.push(BpModule {
                                module_type: ty.clone(),
                                ..BpModule::default()
                            });
                            true
                        }
                        _ => false,
                    };
                }
                depth += 1;
            }
            Token::Punct('}' | ']' | ')') => depth = depth.saturating_sub(1),
            Token::Ident(id) if depth == 1 && in_module && id == "name" => {
                if let (Some(Token::Punct(':')), Some(Token::Str(s))) =
                    (toks.get(i + 1), toks.get(i + 2))
                    && toks.get(i + 3) != Some(&Token::Punct('+'))
                    && let Some(m) = out.last_mut()
                {
                    m.name.get_or_insert_with(|| s.clone());
                }
            }
            Token::Ident(id)
                if depth == 1
                    && in_module
                    && matches!(
                        id.as_str(),
                        "pluginFor"
                            | "srcs"
                            | "defaults"
                            | "license_kinds"
                            | "license_text"
                            | "licenses"
                            | "default_applicable_licenses"
                    ) =>
            {
                if toks.get(i + 1) == Some(&Token::Punct(':'))
                    && let Some(list) = string_list(&toks, i + 2)
                    && let Some(m) = out.last_mut()
                {
                    match id.as_str() {
                        "srcs" => m.srcs = list,
                        "defaults" => m.defaults = list,
                        "license_kinds" => m.license_kinds = list,
                        "license_text" => m.license_text = list,
                        "licenses" | "default_applicable_licenses" => m.licenses.extend(list),
                        _ => m.plugin_for = list,
                    }
                }
            }
            // `build = [...]` or `build += [...]`
            Token::Ident(id) if depth == 0 && id == "build" => {
                let mut j = i + 1;
                if toks.get(j) == Some(&Token::Punct('+')) {
                    j += 1;
                }
                if toks.get(j) == Some(&Token::Punct('='))
                    && let Some(list) = string_list(&toks, j + 1)
                {
                    build.extend(list);
                }
            }
            _ => {}
        }
    }
    BpFile {
        modules: out,
        build,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn licence_references_resolve_as_soong_does() {
        let parsed = parse_bp(
            "package {\n    default_applicable_licenses: [\"a\"],\n}\ncc_library { name: \"x\", licenses: [\"b\", \"c\"] }\n",
        );
        assert_eq!(parsed.modules[0].licenses, ["a"]);
        assert_eq!(parsed.modules[1].licenses, ["b", "c"]);
        let apache: &[&str] = &["SPDX-license-identifier-Apache-2.0"];
        let mit: &[&str] = &["SPDX-license-identifier-MIT"];
        let index = Index::of_licenses(&[
            ("l", "external/x/Android.bp", mit),
            ("l", "external/y/Android.bp", apache),
            ("shared", "build/soong/licenses/Android.bp", apache),
            ("empty", "external/z/Android.bp", &[]),
        ]);
        let of = |r: &str, from: &str| index.license_kinds_of(r, from);
        assert_eq!(of("l", "external/x/sub").unwrap(), mit);
        assert_eq!(of("//external/y:l", "external/x").unwrap(), apache);
        assert_eq!(of("shared", "external/x").unwrap(), apache);
        // `external/xx` is not inside `external/x`
        assert!(
            of("l", "external/xx")
                .unwrap_err()
                .contains("none enclosing")
        );
        assert!(
            of("nope", "external/x")
                .unwrap_err()
                .contains("no license module")
        );
        // harfbuzz_ng's `unused.HPND-sell-variant` shape: kinds unknown, never read as none
        assert!(
            of("empty", "external/z")
                .unwrap_err()
                .contains("lists no licence kind")
        );
        // an index built before kinds were recorded
        let mut stale = index;
        for d in stale.modules.values_mut().flatten() {
            d.license_kinds = None;
        }
        assert!(
            stale
                .license_kinds_of("l", "external/x")
                .unwrap_err()
                .contains("rebuilds it")
        );
    }

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
build = ["Android.gen.bp"]
build += ["more.bp", "x.bp"]
"#;
        let m = |t: &str, n: Option<&str>| BpModule {
            module_type: t.to_owned(),
            name: n.map(str::to_owned),
            ..BpModule::default()
        };
        let parsed = parse_bp(bp);
        assert_eq!(parsed.build, ["Android.gen.bp", "more.bp", "x.bp"]);
        assert_eq!(
            parsed.modules,
            vec![
                BpModule {
                    srcs: vec!["a.c".into(), "b{c}.c".into()],
                    ..m("cc_library", Some("libc"))
                },
                m("genrule", Some("gen")),
                m("cc_test", None),
                m("soong_namespace", None)
            ]
        );
        // a plugin source may end in `_test.go` without being a Go test (test/app_compat/csuite)
        let plugin = parse_bp(
            "bootstrap_go_package {\n name: \"soong-csuite\",\n srcs: [\"csuite_test.go\"],\n testSrcs: [\"csuite_test_test.go\"],\n pluginFor: [\"soong_build\"],\n}\n",
        );
        assert!(plugin.modules[0].is_soong_plugin());
        assert_eq!(plugin.modules[0].srcs, ["csuite_test.go"]);
    }

    #[test]
    fn go_type_registrations() {
        let go = r#"
func init() {
	android.RegisterModuleType("aidl_interface", AidlInterfaceFactory)
	ctx.RegisterSingletonModuleType("all_apis", f)
	ctx.RegisterParallelSingletonModuleType("x_singleton", f)
	ctx.RegisterModuleType(name, f)
	notRegister.ModuleType("nope")
}"#;
        assert_eq!(
            registered_types(go),
            ["aidl_interface", "all_apis", "x_singleton"]
        );
    }

    #[test]
    fn project_of_path() {
        let m = parse(r#"<manifest><default revision="r"/><project path="build/make" name="a"/><project path="build/make2" name="b"/></manifest>"#).unwrap();
        assert_eq!(m.project_of("build/make/core/x.mk"), Some("build/make"));
        assert_eq!(m.project_of("build/make"), Some("build/make"));
        assert_eq!(m.project_of("build/make2/y"), Some("build/make2"));
        assert_eq!(m.project_of("build/other"), None);
        assert_eq!(m.project_of("Android.bp"), None);
    }

    #[test]
    fn namespace_is_innermost_parent() {
        let ns: BTreeSet<&str> = ["a", "a/b/c"].into();
        assert_eq!(enclosing(&ns, "a/b"), Some("a"));
        assert_eq!(enclosing(&ns, "a/b/c/d"), Some("a/b/c"));
        assert_eq!(enclosing(&ns, "ab"), None);
        assert_eq!(enclosing(&ns, ""), None);
    }

    #[test]
    fn reference_resolution() {
        let def = |t: &str, p: &str, ns: Option<&str>| Definition {
            module_type: t.into(),
            project: p.into(),
            file: format!("{p}/Android.bp"),
            namespace: ns.map(Into::into),
            defaults: vec![],
            license_kinds: None,
        };
        let modules = BTreeMap::from([
            (
                "libc".to_owned(),
                vec![
                    def("cc_library", "bionic", None),
                    def("ndk_library", "bionic", None),
                    def("cc_prebuilt_library_shared", "prebuilts/runtime", None),
                ],
            ),
            (
                "libz".to_owned(),
                vec![
                    def("cc_library", "external/zlib", None),
                    def("cc_library", "device/x", Some("device/x")),
                ],
            ),
            (
                "tool".to_owned(),
                vec![def("cc_prebuilt_binary", "prebuilts/t", None)],
            ),
            (
                "dup".to_owned(),
                vec![def("cc_library", "a", None), def("cc_library", "b", None)],
            ),
        ]);
        let module_types = BTreeMap::from([(
            "aidl_interface".to_owned(),
            BTreeSet::from(["system/tools/aidl".to_owned()]),
        )]);
        let index = Index {
            modules,
            module_types,
        };
        // source over prebuilt; one project despite two definitions
        assert_eq!(index.project_for("libc"), Ok("bionic"));
        assert_eq!(index.project_for(":libc{.tag}"), Ok("bionic"));
        // bare name -> root namespace; explicit namespace -> only it
        assert_eq!(index.project_for("libz"), Ok("external/zlib"));
        assert_eq!(index.project_for("//device/x:libz"), Ok("device/x"));
        // `//dir:name` also names a root-namespace module by its package directory
        assert_eq!(
            index.project_for("//external/zlib:libz"),
            Ok("external/zlib")
        );
        assert!(index.project_for("//elsewhere:libz").is_err());
        // a prebuilt is taken when it is all there is, unless the consumer never builds prebuilts
        assert_eq!(index.project_for("tool"), Ok("prebuilts/t"));
        assert!(index.resolve("tool", Prebuilts::Never).is_err());
        assert_eq!(index.resolve("libc", Prebuilts::Never), Ok("bionic"));
        assert!(
            index
                .project_for("dup")
                .unwrap_err()
                .starts_with("ambiguous")
        );
        assert!(index.project_for("nope").is_err());
        // generated by a module type during analysis: longest defined dotted prefix
        assert_eq!(index.project_for("libc.stubs.source"), Ok("bionic"));
        assert!(index.project_for("nope.stubs").is_err());
    }

    #[test]
    fn aidl_backend_names() {
        for (name, iface) in [
            ("foo-cpp", Some("foo")),
            ("foo-V2-ndk", Some("foo")),
            (
                "android.hardware.common.fmq-V1-cpp",
                Some("android.hardware.common.fmq"),
            ),
            ("foo-V12-java", Some("foo")),
            ("foo-cpp-analyzer", Some("foo")),
            ("foo-V-cpp", Some("foo-V")),
            ("foo-ndk_platform", Some("foo")),
            ("foo", None),
            ("foo-V2", None),
        ] {
            assert_eq!(aidl_interface_of(name), iface, "{name}");
        }
        let def = |t: &str, p: &str| Definition {
            module_type: t.into(),
            project: p.into(),
            file: format!("{p}/Android.bp"),
            namespace: None,
            defaults: vec![],
            license_kinds: None,
        };
        let index = Index {
            modules: BTreeMap::from([
                ("a.b".to_owned(), vec![def("aidl_interface", "hw/a")]),
                ("a".to_owned(), vec![def("cc_library", "other")]),
                ("libx".to_owned(), vec![def("cc_library", "x")]),
            ]),
            module_types: BTreeMap::new(),
        };
        // the interface's project, not the project of its dotted prefix `a`
        assert_eq!(index.project_for("a.b-V3-cpp"), Ok("hw/a"));
        assert_eq!(index.project_for("a.b-ndk"), Ok("hw/a"));
        // a `-cpp` name of something that is no aidl_interface stays unresolved
        assert!(index.project_for("libx-cpp").is_err());
    }

    #[test]
    fn hidl_and_sysprop_names() {
        for (name, iface) in [
            (
                "android.hidl.base@1.0_genc++",
                Some("android.hidl.base@1.0"),
            ),
            (
                "android.hidl.base@1.0_genc++_headers",
                Some("android.hidl.base@1.0"),
            ),
            ("a.b@2.13_hal", Some("a.b@2.13")),
            ("a.b@2.13", None),
            ("libhidl_genc++", None),
        ] {
            assert_eq!(hidl_interface_of(name), iface, "{name}");
        }
        let def = |t: &str, p: &str| Definition {
            module_type: t.into(),
            project: p.into(),
            file: format!("{p}/Android.bp"),
            namespace: None,
            defaults: vec![],
            license_kinds: None,
        };
        let index = Index {
            modules: BTreeMap::from([
                ("a.b@1.0".to_owned(), vec![def("hidl_interface", "hw/ab")]),
                // the dotted prefix `a.b` is another project's module
                ("a.b".to_owned(), vec![def("hidl_package_root", "other")]),
                (
                    "com.x.Props".to_owned(),
                    vec![def("sysprop_library", "sys/x")],
                ),
                ("com".to_owned(), vec![def("cc_library", "wrong")]),
                ("libplain".to_owned(), vec![def("cc_library", "plain")]),
                ("plain".to_owned(), vec![def("cc_library", "wrong")]),
            ]),
            module_types: BTreeMap::new(),
        };
        assert_eq!(index.project_for("a.b@1.0_genc++"), Ok("hw/ab"));
        assert_eq!(index.project_for("a.b@1.0_genc++_headers"), Ok("hw/ab"));
        assert_eq!(index.project_for(":a.b@1.0_hal"), Ok("hw/ab"));
        assert_eq!(index.project_for("a.b@1.0"), Ok("hw/ab"));
        // a version no Android.bp has stays unresolved: neither the package root `a.b` nor a prefix is guessed
        assert!(index.project_for("a.b@9.9_genc++").is_err());
        // `lib<name>` of a sysprop_library; a library that exists keeps its own project; `lib<name>` of a non-sysprop module is not mapped
        assert_eq!(index.project_for("libcom.x.Props"), Ok("sys/x"));
        assert_eq!(index.project_for("libplain"), Ok("plain"));
        assert!(index.project_for("libcom.y.Props").is_err());
    }
}
