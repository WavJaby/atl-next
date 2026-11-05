// SPDX-License-Identifier: MIT
//! Step `android-paths` of `xtask check`: an Android path literal (a string literal equal to or below one of `atl_android_paths::ROOTS`) lives only in crate atl-android-paths, so the guest layout has one home (plan S18) and a port to another host layout edits one file.
//! Scanned: every `.rs` file the next commit can contain (spdx.rs `candidate_files`), outside that crate and this file (its tests hold literals on purpose). A line is read up to `//` outside a string (comments, docs); a literal is `"..."` with `\` escapes, any prefix (`c`, `b`, `format!`). Raw strings (`r"..."`) and `/*` block comments are not handled.
//! `android-paths.toml` `[[allow]]` rows exempt one literal in one file, with the reason: a test's independently derived expected value, a fixture path no layout owns. A row that matches nothing fails (stale).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

pub const FILE: &str = "android-paths.toml";
/// The crate that owns the literals: not scanned.
const HOME: &str = "crates/atl-android-paths/";
/// This file: its tests hold the literals the scanner must find, so scanning it would only test the tests.
const SELF: &str = "xtask/src/android_paths.rs";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    #[serde(default)]
    allow: Vec<Allow>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Allow {
    /// Repo-relative file.
    file: String,
    /// The literal's content, as written between the quotes.
    literal: String,
    reason: String,
}

/// String literal contents on one source line, in order; none on a comment line.
fn literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            // a comment runs to the end of the line
            '/' if chars.as_str().starts_with('/') => break,
            // a char literal: skip it so a quote inside ('"') opens nothing; a lifetime ('a) is left as is
            '\'' => {
                let rest = chars.as_str();
                let mut it = rest.chars();
                let len = match (it.next(), it.next(), it.next()) {
                    (Some('\\'), Some(_), Some('\'')) => 3,
                    (Some(_), Some('\''), _) => 2,
                    _ => 0,
                };
                for _ in 0..len {
                    chars.next();
                }
            }
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            if let Some(e) = chars.next() {
                                s.push('\\');
                                s.push(e);
                            }
                        }
                        '"' => break,
                        _ => s.push(c),
                    }
                }
                out.push(s);
            }
            _ => {}
        }
    }
    out
}

/// The literal names an Android path: equal to a root, or a root followed by `/` (a path below it) or `{` (a format argument).
fn is_android_path(literal: &str) -> bool {
    atl_android_paths::ROOTS.iter().any(|root| {
        literal
            .strip_prefix(root)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('{'))
    })
}

/// `(file, line, literal)` of every Android path literal outside HOME.
fn scan(root: &Path, files: &[String]) -> Result<Vec<(String, usize, String)>> {
    let mut found = Vec::new();
    for f in files {
        if !f.ends_with(".rs") || f.starts_with(HOME) || f == SELF {
            continue;
        }
        let path = root.join(f);
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {f}"))?;
        for (i, line) in text.lines().enumerate() {
            for l in literals(line) {
                if is_android_path(&l) {
                    found.push((f.clone(), i + 1, l));
                }
            }
        }
    }
    Ok(found)
}

pub fn check(root: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(root.join(FILE)).with_context(|| format!("reading {FILE}"))?;
    let table: Table = toml::from_str(&text).with_context(|| format!("parsing {FILE}"))?;
    for a in &table.allow {
        ensure!(
            !a.reason.trim().is_empty(),
            "{FILE}: {} {:?}: empty reason",
            a.file,
            a.literal
        );
        ensure!(
            is_android_path(&a.literal),
            "{FILE}: {} {:?}: not an Android path literal",
            a.file,
            a.literal
        );
    }
    let found = scan(root, &crate::spdx::candidate_files(root)?)?;
    let mut used = BTreeSet::new();
    let mut fails = Vec::new();
    for (file, line, lit) in &found {
        match table
            .allow
            .iter()
            .position(|a| &a.file == file && &a.literal == lit)
        {
            Some(i) => {
                used.insert(i);
            }
            None => fails.push(format!(
                "{file}:{line}: {lit:?}: use atl-android-paths, or allow it in {FILE} with a reason"
            )),
        }
    }
    for (i, a) in table.allow.iter().enumerate() {
        if !used.contains(&i) {
            fails.push(format!(
                "{FILE}: {} {:?}: matches nothing (stale row)",
                a.file, a.literal
            ));
        }
    }
    if !fails.is_empty() {
        for f in &fails {
            eprintln!("android-paths: {f}");
        }
        bail!("{} Android path literal problems", fails.len());
    }
    eprintln!(
        "android-paths: ok ({} literals allowed, {} rows)",
        found.len(),
        table.allow.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_literals_not_comments() {
        assert_eq!(
            literals(r#"let a = format!("/data/{x}", "q\"r"); // "/system""#),
            ["/data/{x}", "q\\\"r"]
        );
        assert!(literals(r#"    /// "/system" in a doc"#).is_empty());
        assert_eq!(literals(r#"let q = '"'; let s = c"/apex/x";"#), ["/apex/x"]);
    }

    #[test]
    fn android_path_is_a_root_or_below_it() {
        for yes in [
            "/system",
            "/data/app/x",
            "/data/{app}",
            "/dev/__properties__",
        ] {
            assert!(is_android_path(yes), "{yes}");
        }
        // a different directory sharing a root's prefix, a host path, a relative path
        for no in [
            "/systemd",
            "/database",
            "/usr/lib",
            "system/framework",
            "--ro-bind /r/system /system",
        ] {
            assert!(!is_android_path(no), "{no}");
        }
    }
}
