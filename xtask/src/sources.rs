// SPDX-License-Identifier: MIT
//! `sources.toml`: the upstream manifest (fetch pins only). Column meanings live in that file's header.

use std::collections::BTreeMap;
use std::path::Path;

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
    /// AOSP tree path -> pinned commit.
    pub projects: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitUpstream {
    pub git: String,
    pub tag: String,
    pub commit: String,
}

pub fn load(root: &Path) -> Result<Sources> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    parse(&text).with_context(|| format!("in {FILE}"))
}

fn parse(text: &str) -> Result<Sources> {
    let sources: Sources = toml::from_str(text)?;
    validate(&sources)?;
    Ok(sources)
}

fn validate(sources: &Sources) -> Result<()> {
    let aosp = &sources.aosp;
    ensure!(aosp.remote.ends_with('/'), "aosp.remote must end with `/`");
    ensure!(!aosp.tag.is_empty(), "aosp.tag is empty");
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
        ensure!(
            !up.git.is_empty() && !up.tag.is_empty(),
            "upstream `{name}`: empty git or tag"
        );
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
fn is_tree_path(s: &str) -> bool {
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

    const AOSP: &str = "[aosp]\nremote = \"https://h/platform/\"\ntag = \"t\"\n[aosp.projects]\n";
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
        assert_eq!(sources.git["qemu"].tag, "v1");
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
