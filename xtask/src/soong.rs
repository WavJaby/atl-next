// SPDX-License-Identifier: MIT
//! `check` step `soong`: tracked code never invokes Soong (`soong_ui`, `build/soong/soong_ui.bash`, `lunch`). Soong is retired (a84d9b6, 2026-11-04). The converter reads Soong's `.go` sources as the reference for build behaviour, which stays allowed.
//! Allowed: whole-line comments, a trailing ` //` comment, and Markdown (prose, not code). This file names the tokens it looks for, so it is exempt by path.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::spdx;

/// Tokens that mean an invocation. `build/soong/soong_ui.bash` contains `soong_ui`, so it has no entry of its own.
const TOKENS: [&str; 2] = ["soong_ui", "lunch"];
/// This file: its own tokens and its test fixtures.
const EXEMPT: &str = "xtask/src/soong.rs";

pub fn check(root: &Path) -> Result<()> {
    let files = spdx::candidate_files(root)?;
    let mut hits = Vec::new();
    for rel in &files {
        if rel == EXEMPT || rel.ends_with(".md") {
            continue;
        }
        let bytes = std::fs::read(root.join(rel)).with_context(|| format!("reading {rel}"))?;
        // binary files carry no source line to invoke anything
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        hits.extend(violations(rel, text));
    }
    if hits.is_empty() {
        eprintln!("soong: {} files ok", files.len());
        return Ok(());
    }
    for hit in &hits {
        eprintln!("soong: {hit}");
    }
    bail!(
        "{} code lines invoke Soong (retired, a84d9b6): remove the invocation, or write it as a comment",
        hits.len()
    );
}

/// `rel:line: text` for each code line that names a token.
fn violations(rel: &str, text: &str) -> Vec<String> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let code = code_part(line);
            TOKENS.iter().any(|t| has_word(code, t))
        })
        .map(|(i, line)| format!("{rel}:{}: {}", i + 1, line.trim()))
        .collect()
}

/// The code before any comment: "" for a whole-line comment, the text before a trailing ` //`.
/// `#` opens a comment only at line start (a trailing `#` may be part of code, e.g. a URL fragment).
fn code_part(line: &str) -> &str {
    let t = line.trim_start();
    if ["//", "#", "/*", "*", "<!--"]
        .iter()
        .any(|m| t.starts_with(m))
    {
        return "";
    }
    line.find(" //").map_or(line, |i| &line[..i])
}

/// `word` in `s` as a whole identifier: not part of a longer one on either side.
fn has_word(s: &str, word: &str) -> bool {
    s.match_indices(word).any(|(i, _)| {
        let before = s[..i].chars().next_back();
        let after = s[i + word.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocations_are_named_and_comments_pass() {
        // expected counts read off the text: the shebang is a comment, line 2 invokes
        let sh = "#!/bin/sh\nbuild/soong/soong_ui.bash --make-mode\n";
        let found = violations("scripts/env/x.sh", sh);
        assert_eq!(
            found,
            ["scripts/env/x.sh:2: build/soong/soong_ui.bash --make-mode"]
        );
        assert_eq!(violations("a.mk", "lunch aosp_x86_64-userdebug\n").len(), 1);
        assert_eq!(violations("a.sh", "  lunch;\n").len(), 1);
    }

    #[test]
    fn comments_and_reading_soong_sources_pass() {
        let ok = "// soong_ui sets the product variables\n    // lunch is retired\nlet p = \"build/soong/android/variable.go\";\nlet t = 1; // was soong_ui\nlet m = \"lunched\";\n";
        assert!(violations("tools/bp2ninja/x.rs", ok).is_empty());
    }
}
