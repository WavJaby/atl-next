// SPDX-License-Identifier: MIT
//! `check` step `diag-shape`: no crate writes a free-text refusal or unimplemented line; an API outcome is counted through atl-diag (plan diagnostics-f4 D3), whose `diag:` lines the run summary ranks.
//! A shape = a log macro (`eprintln!`, `writeln!`, tracing's `warn!` ...) whose string literals hold the word `refused` or `unimplemented`, in a crate's `src/` before its `#[cfg(test)]` module.
//! Lines of another subject (driver faults, capability tiers, the QEMU crossing: plan "Out of this family by subject") are listed in [`OTHER_SUBJECT`] with their reason; an entry no line matches fails, so the list cannot rot.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::spdx;

/// Log macros, matched by their last path segment (`tracing::warn!` and `warn!` alike).
const MACROS: [&str; 9] = [
    "eprintln", "eprint", "println", "writeln", "trace", "debug", "info", "warn", "error",
];
/// The words that make a log line a counter shape.
const WORDS: [&str; 2] = ["refused", "unimplemented"];

/// (file, a fragment of the literal, why it is not an API outcome of the family).
const OTHER_SUBJECT: &[(&str, &str, &str)] = &[
    (
        "crates/atl-composer/src/host.rs",
        "a second frame requester refused",
        "UI-thread mailbox protocol inside the app's composer, no API call",
    ),
    (
        "crates/atl-composer/src/task_sender.rs",
        "task sender: task {task} refused",
        "composer-internal routing of the window's task, no API call",
    ),
    (
        "crates/atl-audioflinger/src/track.rs",
        "track region smaller than its layout; refused",
        "shared-memory layout fault of an audio track, not a call's answer",
    ),
    (
        "crates/atl-audioflinger/src/track.rs",
        "invalid shared buffer alignment; refused",
        "shared-memory layout fault of an audio track, not a call's answer",
    ),
];

pub fn check(root: &Path) -> Result<()> {
    let files: Vec<String> = spdx::candidate_files(root)?
        .into_iter()
        .filter(|f| f.starts_with("crates/") && f.contains("/src/") && f.ends_with(".rs"))
        .collect();
    let mut found = Vec::new();
    for rel in &files {
        let text =
            std::fs::read_to_string(root.join(rel)).with_context(|| format!("reading {rel}"))?;
        found.extend(
            shapes(&text)
                .into_iter()
                .map(|(n, lit)| (rel.clone(), n, lit)),
        );
    }
    let bad = judge(&found, OTHER_SUBJECT);
    if bad.is_empty() {
        eprintln!(
            "diag-shape: ok ({} files, {} lines of another subject listed)",
            files.len(),
            OTHER_SUBJECT.len()
        );
        return Ok(());
    }
    for b in &bad {
        eprintln!("diag-shape: {b}");
    }
    bail!(
        "{} free-text refusal/unimplemented lines or stale entries: count the outcome through atl-diag (binder_reply / set_cause / refused / unimplemented_api!), or list a line of another subject in diag_shape.rs OTHER_SUBJECT with its reason",
        bad.len()
    );
}

/// Each shape not listed (`rel:line: "literal"`) and each listed entry no shape matches (stale).
fn judge(found: &[(String, usize, String)], listed: &[(&str, &str, &str)]) -> Vec<String> {
    let covers = |(f, frag, _): &(&str, &str, &str), (rel, _, lit): &(String, usize, String)| {
        rel == f && lit.contains(frag)
    };
    let mut bad: Vec<String> = found
        .iter()
        .filter(|s| !listed.iter().any(|l| covers(l, s)))
        .map(|(rel, n, lit)| format!("{rel}:{n}: \"{lit}\""))
        .collect();
    bad.extend(
        listed
            .iter()
            .filter(|l| !found.iter().any(|s| covers(l, s)))
            .map(|(f, frag, _)| {
                format!("stale OTHER_SUBJECT entry {f} \"{frag}\": no log line matches")
            }),
    );
    bad
}

/// (line, literal) of every log-macro string literal holding one of [`WORDS`], before the file's `#[cfg(test)]`.
fn shapes(text: &str) -> Vec<(usize, String)> {
    let end = text.find("#[cfg(test)]").unwrap_or(text.len());
    let src = &text[..end];
    let mut out = Vec::new();
    for m in MACROS {
        let pat = format!("{m}!");
        for (i, _) in src.match_indices(&pat) {
            let before = src[..i].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let rest = src[i + pat.len()..].trim_start();
            if !rest.starts_with('(') || line_is_comment(src, i) {
                continue;
            }
            let line = src[..i].matches('\n').count() + 1;
            for lit in literals(&rest[1..]) {
                if WORDS.iter().any(|w| has_word(&lit, w)) {
                    out.push((line, lit));
                }
            }
        }
    }
    out.sort();
    out
}

/// Whether the text before byte `i` on its line opens a `//` comment.
fn line_is_comment(src: &str, i: usize) -> bool {
    let start = src[..i].rfind('\n').map_or(0, |p| p + 1);
    src[start..i].contains("//")
}

/// The string literals of a macro's arguments, `args` starting after its `(`, up to the matching `)`. Raw strings (`r"..."`, `r#"..."#`) and escapes are honoured, `//` comments skipped.
fn literals(args: &str) -> Vec<String> {
    let b = args.as_bytes();
    let mut out = Vec::new();
    let mut depth = 1usize;
    let mut i = 0;
    while i < b.len() && depth > 0 {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = args[i..].find('\n').map_or(b.len(), |p| i + p);
                continue;
            }
            b'r' if matches!(b.get(i + 1), Some(b'"' | b'#'))
                && !(i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) =>
            {
                let hashes = args[i + 1..].bytes().take_while(|&c| c == b'#').count();
                let open = i + 1 + hashes;
                if b.get(open) == Some(&b'"') {
                    let close = format!("\"{}", "#".repeat(hashes));
                    let end = args[open + 1..]
                        .find(&close)
                        .map_or(b.len(), |p| open + 1 + p);
                    out.push(args[open + 1..end].to_owned());
                    i = end + close.len();
                    continue;
                }
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                out.push(args[i + 1..j.min(b.len())].to_owned());
                i = j + 1;
                continue;
            }
            // a char literal ('(' or '\'') would unbalance the count; a lifetime has no closing quote two or three bytes on
            b'\'' if b.get(i + 2) == Some(&b'\'') => {
                i += 3;
                continue;
            }
            b'\'' if b.get(i + 1) == Some(&b'\\') => {
                i = args[i + 2..].find('\'').map_or(b.len(), |p| i + 3 + p);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// `word` in `s` as a whole word: not part of a longer identifier on either side.
fn has_word(s: &str, word: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    s.match_indices(word).any(|(i, _)| {
        !s[..i].chars().next_back().is_some_and(ident)
            && !s[i + word.len()..].chars().next().is_some_and(ident)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Line numbers read off the fixture: the shapes are on lines 2 (multi-line writeln), 5 (tracing) and 6 (eprintln).
    #[test]
    fn log_macros_with_the_words_are_shapes() {
        let src = "fn f() {\n    let _ = writeln!(\n        std::io::stderr(),\n        \"services: display refused: {why}\");\n    tracing::warn!(\"composer: setTransactionState refused: {e}\");\n    eprintln!(\"x: unimplemented code {c}\");\n}\n";
        let got: Vec<usize> = shapes(src).into_iter().map(|s| s.0).collect();
        assert_eq!(got, [2, 5, 6]);
    }

    #[test]
    fn counts_comments_identifiers_and_tests_pass() {
        let src = "fn f() {\n    atl_diag::refused(\"I\", \"startActivity refused\");\n    // eprintln!(\"refused\");\n    warn!(\"counters refused_no_modifier={}\", n);\n    log(format_args!(\"answered\"));\n}\n#[cfg(test)]\nmod tests { fn t() { eprintln!(\"refused\"); } }\n";
        assert!(shapes(src).is_empty());
    }

    #[test]
    fn raw_strings_and_char_literals_keep_the_argument_count() {
        let src = "fn f() { eprintln!(r#\"a \"refused\" b\"#); warn!(\"{}\", ')'); error!(\"x unimplemented\"); }\n";
        let lits: Vec<String> = shapes(src).into_iter().map(|s| s.1).collect();
        assert_eq!(lits, ["a \"refused\" b", "x unimplemented"]);
    }

    #[test]
    fn listed_lines_pass_and_stale_entries_fail() {
        let found = vec![("a.rs".to_owned(), 3, "q: refused down call".to_owned())];
        assert!(judge(&found, &[("a.rs", "refused down", "why")]).is_empty());
        assert_eq!(judge(&found, &[]).len(), 1);
        assert_eq!(judge(&[], &[("a.rs", "refused down", "why")]).len(), 1);
    }
}
