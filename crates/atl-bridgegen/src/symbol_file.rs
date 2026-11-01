// SPDX-License-Identifier: MIT
//! AOSP's NDK symbol files (`<lib>.map.txt`): the export list of an NDK library, a linker version script with tags in comments, as Soong's `cc/symbolfile` reads it.
//! Shape: `NAME {` opens a version block (`# tags` may follow), `global:` / `local:` label the scope, `name; # tag tag=value` is one symbol, `extern "C++" {` ... `};` nests C++ patterns, `} [BASE];` closes the version; a line starting with `#` is a comment.
//! Only what a checker needs is kept: each C symbol with its version, scope and tags.

use anyhow::{Result, bail, ensure};

/// One symbol line of a version block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// The version block's name (`LIBANDROID`).
    pub version: String,
    /// In a `global:` scope (a block without a scope label is global, as in ld's version scripts).
    pub global: bool,
    /// Inside `extern "C++" { }`: a demangled pattern, not a C symbol name.
    pub cxx: bool,
    /// The comment's words (`introduced=29`, `systemapi`, `var`, ...).
    pub tags: Vec<String>,
}

/// Every symbol of `text`, in file order.
pub fn parse(text: &str) -> Result<Vec<Symbol>> {
    let mut out = Vec::new();
    let mut version: Option<String> = None;
    let mut global = true;
    let mut cxx = false;
    for (n, raw) in text.lines().enumerate() {
        let line_no = n + 1;
        let (code, comment) = match raw.split_once('#') {
            Some((c, t)) => (c.trim(), t),
            None => (raw.trim(), ""),
        };
        if code.is_empty() {
            continue;
        }
        let Some(ver) = version.as_deref() else {
            let Some(name) = code.strip_suffix('{') else {
                bail!("line {line_no}: `{code}` outside a version block");
            };
            let name = name.trim();
            ensure!(
                !name.is_empty() && !name.contains(char::is_whitespace),
                "line {line_no}: bad version name `{name}`"
            );
            version = Some(name.to_owned());
            global = true;
            continue;
        };
        match code {
            "global:" => global = true,
            "local:" => global = false,
            "extern \"C++\" {" => {
                ensure!(!cxx, "line {line_no}: nested extern \"C++\"");
                cxx = true;
            }
            _ if code.starts_with('}') => {
                if cxx {
                    ensure!(
                        code == "};",
                        "line {line_no}: `{code}` closing extern \"C++\""
                    );
                    cxx = false;
                } else {
                    ensure!(
                        code.ends_with(';'),
                        "line {line_no}: `{code}` closing {ver}"
                    );
                    version = None;
                }
            }
            _ => {
                let Some(name) = code.strip_suffix(';') else {
                    bail!("line {line_no}: `{code}` is no symbol (`name;`)");
                };
                let name = name.trim();
                ensure!(!name.is_empty(), "line {line_no}: empty symbol");
                out.push(Symbol {
                    name: name.to_owned(),
                    version: ver.to_owned(),
                    global,
                    cxx,
                    tags: comment.split_whitespace().map(str::to_owned).collect(),
                });
            }
        }
    }
    ensure!(version.is_none(), "unclosed version block");
    Ok(out)
}

/// The global C symbols of `symbols` by name (C++ patterns and `local:` entries are no exports by name).
pub fn exports(symbols: &[Symbol]) -> impl Iterator<Item = &Symbol> {
    symbols.iter().filter(|s| s.global && !s.cxx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shapes of AOSP's libandroid.map.txt (pinned android-16): a tagged symbol, an untagged one, an extern "C++" block, local:, and a second version inheriting the first.
    const MAP: &str = "LIBANDROID {\n  global:\n    AFont_close; # introduced=29\n    AActivityManager_isUidActive; # systemapi introduced=31\n    extern \"C++\" {\n        ASurfaceControl_getChoreographer*;\n    };\n  local:\n    *;\n};\n\nLIBANDROID_PLATFORM {\n  global:\n    APerformanceHint_sendHint;\n} LIBANDROID;\n";

    #[test]
    fn reads_versions_scopes_and_tags() {
        let s = parse(MAP).unwrap();
        let names: Vec<(&str, &str, bool, bool)> = s
            .iter()
            .map(|s| (s.name.as_str(), s.version.as_str(), s.global, s.cxx))
            .collect();
        assert_eq!(
            names,
            [
                ("AFont_close", "LIBANDROID", true, false),
                ("AActivityManager_isUidActive", "LIBANDROID", true, false),
                (
                    "ASurfaceControl_getChoreographer*",
                    "LIBANDROID",
                    true,
                    true
                ),
                ("*", "LIBANDROID", false, false),
                (
                    "APerformanceHint_sendHint",
                    "LIBANDROID_PLATFORM",
                    true,
                    false
                ),
            ]
        );
        assert_eq!(s[1].tags, ["systemapi", "introduced=31"]);
        let exported: Vec<&str> = exports(&s).map(|s| s.name.as_str()).collect();
        assert_eq!(
            exported,
            [
                "AFont_close",
                "AActivityManager_isUidActive",
                "APerformanceHint_sendHint"
            ]
        );
    }

    #[test]
    fn refuses_what_is_no_symbol_file() {
        assert!(parse("AFont_close;\n").is_err());
        assert!(parse("V {\n  global:\n    AFont_close\n};\n").is_err());
        assert!(parse("V {\n  global:\n    a;\n").is_err());
    }
}
