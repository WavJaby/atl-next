// SPDX-License-Identifier: MIT
//! `sources.toml`: the upstream manifest (fetch pins only). Column meanings live in that file's header.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

pub const FILE: &str = "sources.toml";

#[derive(Debug, Deserialize)]
pub struct Sources {
    pub aosp: Aosp,
    /// Every other top-level table is a git upstream, so a stray or misspelt table fails to parse as one.
    #[serde(flatten)]
    pub git: BTreeMap<String, GitUpstream>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Aosp {
    pub remote: String,
    pub tag: String,
    /// `platform/manifest` commit the tag resolves to: project git names and the tree's link/copy files.
    pub manifest: String,
    /// AOSP tree path -> pinned commit.
    pub projects: BTreeMap<String, String>,
}

/// Directory of the manifest checkout under `src/`: outside the AOSP tree, which the converter scans.
pub const AOSP_MANIFEST_DIR: &str = "aosp-manifest";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitUpstream {
    pub git: String,
    /// Exactly one of `tag` / `branch` / `change`: the ref the pin is read from (a branch only where upstream publishes no tag for the wanted build; `change` = Gerrit `<nn>/<change>/<patchset>` under `refs/changes/`, only where the commit is reachable from no published branch or tag).
    pub tag: Option<String>,
    pub branch: Option<String>,
    pub change: Option<String>,
    pub commit: String,
    /// Cone-mode sparse directories; empty = whole tree. Set => partial (blob:none) clone.
    #[serde(default)]
    pub sparse: Vec<String>,
}

impl GitUpstream {
    /// Full ref name the pin is read from (`refs/tags/..`, `refs/heads/..` or `refs/changes/..`).
    pub fn refname(&self) -> String {
        match (&self.tag, &self.branch, &self.change) {
            (Some(t), _, _) => format!("refs/tags/{t}"),
            (None, Some(b), _) => format!("refs/heads/{b}"),
            (None, None, Some(c)) => format!("refs/changes/{c}"),
            (None, None, None) => String::new(),
        }
    }
}

/// One upstream checkout: where it lives under `<workspace>/src` (and under `patches/`), and its pin.
pub struct Checkout {
    /// `aosp/<path>` or `<name>`; the same relative path under `src/` and `patches/`.
    pub dir: PathBuf,
    /// None for an AOSP project while the manifest (which holds its git name) is not fetched.
    pub url: Option<String>,
    /// Full ref name the commit is pinned from (`refs/tags/..` or `refs/heads/..`).
    pub rev: String,
    pub commit: String,
    /// Cone-mode sparse directories; empty = full checkout.
    pub sparse: Vec<String>,
    pub pin: Pin,
    /// Key of the upstream's `[upstream.<key>]` row in licenses.toml (`aosp` for every AOSP project).
    pub licence_key: String,
}

/// Where a checkout's commit lives in sources.toml.
pub enum Pin {
    Aosp(String),
    Manifest,
    Git(String),
}

impl Sources {
    /// Upstream names as other tables reference them: each git upstream, plus `aosp`.
    pub fn upstream_names(&self) -> std::collections::BTreeSet<String> {
        self.git
            .keys()
            .cloned()
            .chain(["aosp".to_owned()])
            .collect()
    }

    /// URL of the AOSP repository with this git name (the manifest's `name`, not the tree path).
    pub fn aosp_url(&self, name: &str) -> String {
        format!("{}{name}", self.aosp.remote)
    }

    /// The AOSP manifest's own checkout; its URL needs nothing from the manifest.
    pub fn manifest_checkout(&self) -> Checkout {
        Checkout {
            dir: PathBuf::from(AOSP_MANIFEST_DIR),
            url: Some(self.aosp_url("platform/manifest")),
            rev: format!("refs/tags/{}", self.aosp.tag),
            commit: self.aosp.manifest.clone(),
            sparse: Vec::new(),
            pin: Pin::Manifest,
            licence_key: "aosp".to_owned(),
        }
    }

    /// Every pinned upstream checkout (the manifest is fetch's own concern, not one of them).
    /// `names` (manifest path -> git name) gives AOSP project URLs.
    pub fn checkouts(&self, names: Option<&BTreeMap<String, String>>) -> Vec<Checkout> {
        let aosp = self.aosp.projects.iter().map(|(path, commit)| Checkout {
            dir: Path::new("aosp").join(path),
            url: names
                .and_then(|n| n.get(path))
                .map(|name| self.aosp_url(name)),
            rev: format!("refs/tags/{}", self.aosp.tag),
            commit: commit.clone(),
            sparse: Vec::new(),
            pin: Pin::Aosp(path.clone()),
            licence_key: "aosp".to_owned(),
        });
        let git = self.git.iter().map(|(name, up)| Checkout {
            dir: PathBuf::from(name),
            url: Some(up.git.clone()),
            rev: up.refname(),
            commit: up.commit.clone(),
            sparse: up.sparse.clone(),
            pin: Pin::Git(name.clone()),
            licence_key: name.clone(),
        });
        aosp.chain(git).collect()
    }
}

impl Checkout {
    /// Where the checkout's files sit under its licence key's root: an AOSP project's tree path (licenses.toml `paths` of `aosp` are tree-relative), empty otherwise.
    pub fn licence_prefix(&self) -> PathBuf {
        match &self.pin {
            Pin::Aosp(path) => PathBuf::from(path),
            Pin::Manifest | Pin::Git(_) => PathBuf::new(),
        }
    }

    pub fn url(&self) -> Result<&str> {
        self.url
            .as_deref()
            .context("AOSP manifest not fetched or lacks this path (xtask fetch --execute)")
    }
}

pub fn load(root: &Path) -> Result<Sources> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text).with_context(|| format!("in {FILE}"))
}

pub fn parse(text: &str) -> Result<Sources> {
    let sources: Sources = toml::from_str(text)?;
    validate(&sources)?;
    Ok(sources)
}

fn validate(sources: &Sources) -> Result<()> {
    let aosp = &sources.aosp;
    ensure!(aosp.remote.ends_with('/'), "aosp.remote must end with `/`");
    ensure!(!aosp.tag.is_empty(), "aosp.tag is empty");
    ensure!(
        is_commit(&aosp.manifest),
        "aosp.manifest `{}` is not a full commit hash",
        aosp.manifest
    );
    for (path, commit) in &aosp.projects {
        ensure!(
            is_tree_path(path),
            "aosp project `{path}`: not a relative tree path"
        );
        ensure!(
            is_commit(commit),
            "aosp project `{path}`: `{commit}` is not a full commit hash"
        );
    }
    for (name, up) in &sources.git {
        ensure!(
            is_name(name),
            "upstream `{name}`: name must be [a-z0-9-], starting alphanumeric"
        );
        ensure!(!up.git.is_empty(), "upstream `{name}`: empty git");
        ensure!(
            matches!(
                (&up.tag, &up.branch, &up.change),
                (Some(r), None, None) | (None, Some(r), None) | (None, None, Some(r)) if !r.is_empty()
            ),
            "upstream `{name}`: needs exactly one non-empty `tag`, `branch` or `change`"
        );
        if let Some(c) = &up.change {
            let parts: Vec<&str> = c.split('/').collect();
            ensure!(
                parts.len() == 3
                    && parts
                        .iter()
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                    && parts[1].ends_with(parts[0]),
                "upstream `{name}`: change `{c}` is not a Gerrit `<last two digits>/<change>/<patchset>`"
            );
        }
        for d in &up.sparse {
            ensure!(
                is_tree_path(d),
                "upstream `{name}`: sparse `{d}` is not a relative tree path"
            );
        }
        ensure!(
            is_commit(&up.commit),
            "upstream `{name}`: `{}` is not a full commit hash",
            up.commit
        );
    }
    Ok(())
}

/// Full SHA-1 object name, lowercase (all pinned upstreams are SHA-1 repositories).
fn is_commit(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `a/b/c`: no empty, `.` or `..` component, no leading or trailing `/`.
pub fn is_tree_path(s: &str) -> bool {
    s.split('/').all(|c| {
        !c.is_empty()
            && c != "."
            && c != ".."
            && c.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

/// Row name of sources.toml and deps.toml: `[a-z0-9-]`, starting alphanumeric.
pub fn is_name(s: &str) -> bool {
    s.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::parse;

    const AOSP: &str = "[aosp]\nremote = \"https://h/\"\ntag = \"t\"\nmanifest = \"0123456789abcdef0123456789abcdef01234567\"\n[aosp.projects]\n";
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn err(text: &str) -> String {
        format!("{:#}", parse(text).expect_err("should be rejected"))
    }

    #[test]
    fn aosp_project_and_git_upstream() {
        let text = format!(
            "{AOSP}\"external/icu\" = \"{SHA}\"\n[qemu]\ngit = \"g\"\ntag = \"v1\"\ncommit = \"{SHA}\"\n"
        );
        let sources = parse(&text).unwrap();
        assert_eq!(sources.aosp.projects["external/icu"], SHA);
        assert_eq!(sources.git["qemu"].refname(), "refs/tags/v1");
    }

    #[test]
    fn branch_pin_with_sparse_dirs() {
        let ok = format!(
            "{AOSP}[llvm]\ngit = \"g\"\nbranch = \"rel\"\ncommit = \"{SHA}\"\nsparse = [\"libcxx\", \"llvm/cmake\"]\n"
        );
        let sources = parse(&ok).unwrap();
        assert_eq!(sources.git["llvm"].refname(), "refs/heads/rel");
        let both = format!(
            "{AOSP}[llvm]\ngit = \"g\"\ntag = \"t\"\nbranch = \"rel\"\ncommit = \"{SHA}\"\n"
        );
        assert!(err(&both).contains("exactly one"));
        let bad = format!(
            "{AOSP}[llvm]\ngit = \"g\"\ntag = \"t\"\ncommit = \"{SHA}\"\nsparse = [\"../x\"]\n"
        );
        assert!(err(&bad).contains("not a relative tree path"));
    }

    #[test]
    fn gerrit_change_pin() {
        // Gerrit shards change 3568025 under its last two digits: refs/changes/25/3568025/<patchset>.
        let ok = format!("{AOSP}[x]\ngit = \"g\"\nchange = \"25/3568025/1\"\ncommit = \"{SHA}\"\n");
        assert_eq!(
            parse(&ok).unwrap().git["x"].refname(),
            "refs/changes/25/3568025/1"
        );
        let shard =
            format!("{AOSP}[x]\ngit = \"g\"\nchange = \"24/3568025/1\"\ncommit = \"{SHA}\"\n");
        assert!(err(&shard).contains("not a Gerrit"));
        let both = format!(
            "{AOSP}[x]\ngit = \"g\"\ntag = \"t\"\nchange = \"25/3568025/1\"\ncommit = \"{SHA}\"\n"
        );
        assert!(err(&both).contains("exactly one"));
    }

    #[test]
    fn short_or_uppercase_commit() {
        assert!(err(&format!("{AOSP}art = \"0123abc\"\n")).contains("not a full commit hash"));
        let upper = SHA.to_uppercase();
        assert!(err(&format!("{AOSP}art = \"{upper}\"\n")).contains("not a full commit hash"));
    }

    #[test]
    fn escaping_tree_path() {
        assert!(
            err(&format!("{AOSP}\"../art\" = \"{SHA}\"\n")).contains("not a relative tree path")
        );
        assert!(err(&format!("{AOSP}\"/art\" = \"{SHA}\"\n")).contains("not a relative tree path"));
    }

    #[test]
    fn misspelt_aosp_table() {
        let e = err(&AOSP.replace("aosp", "aops"));
        assert!(e.contains("missing field `aosp`"), "{e}");
    }

    #[test]
    fn unknown_upstream_column() {
        let text = format!(
            "{AOSP}[qemu]\ngit = \"g\"\ntag = \"v1\"\ncommit = \"{SHA}\"\nsha256 = \"x\"\n"
        );
        assert!(err(&text).contains("unknown field `sha256`"));
    }

    #[test]
    fn bad_upstream_name() {
        let text = format!("{AOSP}[Qemu]\ngit = \"g\"\ntag = \"v1\"\ncommit = \"{SHA}\"\n");
        assert!(err(&text).contains("name must be"));
    }
}
