// SPDX-License-Identifier: MIT
//! Step `bwrap-flags` of `xtask check`: bwrap flag literals live only in crate atl-bwrap (the argv builder), so every sandbox argv is built through it and a flag's meaning has one home.
//! Flags = the `--name` literals of bwrap.rs itself (derived, not listed here). A finding = a string literal elsewhere whose first whitespace-separated token is one of them (a flag inside another command's text, `mount --bind`, is not bwrap's), in any `.rs` file the next commit can contain (spdx.rs `candidate_files`); literals are read as android_paths.rs reads them.
//! Exempt: this file and the functions in [`EXEMPT`], each with its reason -- chiefly the golden argv tests, expected values written from the sandbox spec that must not be built by the code under test. A row whose function holds no flag literal fails (stale).

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::android_paths::literals;

const BUILDER: &str = "crates/atl-bwrap/src/lib.rs";
const SELF: &str = "xtask/src/bwrap_flags.rs";
/// (file, function, reason) allowed to hold a flag literal.
const EXEMPT: &[(&str, &str, &str)] = &[
    (
        "crates/atl-broker/src/lib.rs",
        "sandbox_shape",
        "golden argv",
    ),
    (
        "crates/atl-broker/src/lib.rs",
        "binderd_shape",
        "golden argv",
    ),
    (
        "crates/atl-broker/src/lib.rs",
        "installer_shape",
        "golden argv of the installer profile",
    ),
    (
        "crates/atl-broker/src/lib.rs",
        "links_follow_the_mounts",
        "golden argv tail",
    ),
    (
        "crates/atl-broker/src/lib.rs",
        "nested_mount_keeps_the_root_dir",
        "golden argv: a mount below a root dir",
    ),
    ("xtask/src/tree.rs", "namespace_argv", "golden argv"),
    (
        "xtask/src/android_paths.rs",
        "android_path_is_a_root_or_below_it",
        "a non-path literal fixture of that gate's test",
    ),
    (
        "xtask-harness/src/services.rs",
        "stage",
        "atld's own `app --uid` argument, not bwrap's",
    ),
    (
        "xtask-harness/src/atld_run.rs",
        "build",
        "aapt2 compile's `--dir`, not bwrap's",
    ),
];

/// The `--name` literals of `text` (bwrap.rs).
fn flags(text: &str) -> BTreeSet<String> {
    text.lines()
        .flat_map(literals)
        .filter(|l| l.len() > 2 && l.starts_with("--") && !l.contains(char::is_whitespace))
        .collect()
}

/// The name of the last `fn` opened at or above each line: the function a line belongs to, close enough for test functions (no nested fns).
fn function_of(line: &str, current: &mut String) {
    if let Some(rest) = line.trim_start().split("fn ").nth(1)
        && (line.trim_start().starts_with("fn ") || line.contains(" fn "))
    {
        *current = rest
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("")
            .to_owned();
    }
}

/// `(file, line, function, literal)` of every literal outside the builder holding a flag.
fn scan(
    text_of: impl Fn(&str) -> Option<String>,
    files: &[String],
    flags: &BTreeSet<String>,
) -> Vec<(String, usize, String, String)> {
    let mut found = Vec::new();
    for f in files {
        if !f.ends_with(".rs") || f == BUILDER || f == SELF {
            continue;
        }
        let Some(text) = text_of(f) else { continue };
        let mut func = String::new();
        for (i, line) in text.lines().enumerate() {
            function_of(line, &mut func);
            for l in literals(line) {
                if l.split_whitespace()
                    .next()
                    .is_some_and(|t| flags.contains(t))
                {
                    found.push((f.clone(), i + 1, func.clone(), l));
                }
            }
        }
    }
    found
}

/// Findings outside EXEMPT, then EXEMPT rows nothing matched.
fn judge(found: &[(String, usize, String, String)], exempt: &[(&str, &str, &str)]) -> Vec<String> {
    let mut fails = Vec::new();
    let mut used = BTreeSet::new();
    for (f, line, func, l) in found {
        match exempt.iter().find(|(gf, gfn, _)| gf == f && gfn == func) {
            Some(g) => {
                used.insert(*g);
            }
            None => fails.push(format!("{f}:{line}: bwrap flag literal {l:?} outside {BUILDER}: build the argv with atl_bwrap::Args")),
        }
    }
    for g in exempt {
        if !used.contains(g) {
            fails.push(format!(
                "{SELF}: EXEMPT row {}::{} matches no flag literal (stale)",
                g.0, g.1
            ));
        }
    }
    fails
}

pub fn check(root: &Path) -> Result<()> {
    let builder = std::fs::read_to_string(root.join(BUILDER))
        .with_context(|| format!("reading {BUILDER}"))?;
    let flags = flags(&builder);
    if flags.is_empty() {
        bail!("{BUILDER}: no `--flag` literals found: the probe would pass anything");
    }
    let files = crate::spdx::candidate_files(root)?;
    let found = scan(
        |f| std::fs::read_to_string(root.join(f)).ok(),
        &files,
        &flags,
    );
    let fails = judge(&found, EXEMPT);
    if !fails.is_empty() {
        bail!("{}", fails.join("\n"));
    }
    println!(
        "bwrap-flags: ok ({} flags, {} exempt literal(s) in {} function(s))",
        flags.len(),
        found.len(),
        EXEMPT.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn flags_from_builder() {
        let text =
            "self.word(\"--tmpfs\").word(at)\nself.0.push(\"--\".into());\nlet x = \"a --b\";";
        assert_eq!(flags(text), set(&["--tmpfs"]));
    }

    /// Deliberate break: a flag literal in a non-exempt function is found and fails; inside an exempt one it passes; another command's text (`mount --bind`) is not bwrap's; an exempt row with no literal is stale.
    #[test]
    fn finds_outside_exempt() {
        const EX: &[(&str, &str, &str)] = &[
            ("crates/atl-broker/src/lib.rs", "sandbox_shape", "t"),
            ("crates/atl-broker/src/lib.rs", "binderd_shape", "t"),
            ("xtask/src/tree.rs", "namespace_argv", "t"),
        ];
        let flags = set(&["--ro-bind", "--tmpfs", "--bind"]);
        let text = |f: &str| {
            Some(match f {
                "crates/atl-broker/src/lib.rs" => "fn sandbox_shape() {\n let a = \"--tmpfs / --ro-bind\";\n}\nfn binderd_shape() {\n \"--ro-bind\";\n}\n".to_owned(),
                "xtask/src/tree.rs" => "fn namespace_argv() { [\"--tmpfs\"] }\nfn run() {\n cmd.arg(\"--ro-bind\");\n sh(\"mount --bind a b\");\n}\n".to_owned(),
                _ => String::new(),
            })
        };
        let files = vec![
            "crates/atl-broker/src/lib.rs".to_owned(),
            "xtask/src/tree.rs".to_owned(),
        ];
        let fails = judge(&scan(text, &files, &flags), EX);
        assert_eq!(fails.len(), 1, "{fails:?}");
        assert!(fails[0].starts_with("xtask/src/tree.rs:3:"), "{fails:?}");
        let fails = judge(&scan(text, &files[1..], &flags), EX);
        assert!(
            fails
                .iter()
                .any(|f| f.contains("sandbox_shape") && f.contains("stale")),
            "{fails:?}"
        );
    }
}
