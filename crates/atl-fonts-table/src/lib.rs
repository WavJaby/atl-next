// SPDX-License-Identifier: MIT
//! The font tables Android's system font list is built from (plan C7), as definitions only: the tracked `fonts.toml` ([`Table`], checked against AOSP's own fonts.xml names), AOSP's `alias.json` and `fallback_order.json` (frameworks/base/data/fonts, read as-is).
//! No desktop access and no build script: atl-fonts builds the model from these with the platform layer's fonts; xtask step `tables` checks `fonts.toml` with the same parser.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use serde::Deserialize;

/// The tracked table (repo root `fonts.toml`), compiled in: rows change with the code that reads them.
pub const FONTS_TOML: &str = include_str!("../../../fonts.toml");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fonts.toml: {0}")]
    Table(String),
    #[error("{0}: {1}")]
    Json(&'static str, serde_json::Error),
}

/// When a row's head counts as the family (fonts.toml header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accept {
    Required,
    Differs,
    Monospaced,
    Narrower,
    Named,
}

/// One fonts.toml row.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub query: Option<String>,
    pub width: Option<i32>,
    pub accept: Option<Accept>,
    pub from: Option<String>,
    pub none: Option<String>,
}

/// fonts.toml, rows in file order, cross-references checked.
#[derive(Debug, Clone)]
pub struct Table {
    pub rows: Vec<(String, Row)>,
}

impl Table {
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut t: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| Error::Table(e.to_string()))?;
        // toml::Table sorts its keys; the file order (the XML's) comes from toml_edit
        let order: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| Error::Table(e.to_string()))?;
        let mut rows = Vec::new();
        for (name, _) in order.iter() {
            let v = t
                .remove(name)
                .ok_or_else(|| Error::Table(format!("[{name}] vanished")))?;
            let row: Row = v
                .try_into()
                .map_err(|e: toml::de::Error| Error::Table(format!("[{name}]: {e}")))?;
            rows.push((name.to_owned(), row));
        }
        let table = Self { rows };
        table.check()?;
        Ok(table)
    }

    fn check(&self) -> Result<(), Error> {
        let bad = |n: &str, why: &str| Err(Error::Table(format!("[{n}]: {why}")));
        let names: BTreeSet<&str> = self.rows.iter().map(|(n, _)| n.as_str()).collect();
        if self.rows.first().map(|(n, _)| n.as_str()) != Some(DEFAULT_FAMILY) {
            return bad(DEFAULT_FAMILY, "must be the first row");
        }
        for (n, r) in &self.rows {
            match (&r.none, &r.query, r.accept) {
                (Some(_), None, None) if r.from.is_none() && r.width.is_none() => continue,
                (None, Some(_), Some(a)) => {
                    let from_ok = match (a, &r.from) {
                        (Accept::Differs, Some(f)) => names.contains(f.as_str()) && f != n,
                        (Accept::Differs, None) => false,
                        (_, f) => f.is_none(),
                    };
                    if !from_ok {
                        return bad(
                            n,
                            "`from` must name another row exactly when accept = \"differs\"",
                        );
                    }
                }
                _ => return bad(n, "a row has either `none`, or `query` + `accept`"),
            }
        }
        if self.rows[0].1.accept != Some(Accept::Required) {
            return bad(DEFAULT_FAMILY, "accept must be \"required\"");
        }
        Ok(())
    }

    /// The table's names equal the named families of AOSP's own fonts.xml (`<family name>`).
    pub fn check_against_aosp(&self, aosp_fonts_xml: &str) -> Result<(), Error> {
        let mut aosp = BTreeSet::new();
        for part in aosp_fonts_xml.split("<family name=\"").skip(1) {
            aosp.insert(part.split('"').next().unwrap_or_default());
        }
        let ours: BTreeSet<&str> = self.rows.iter().map(|(n, _)| n.as_str()).collect();
        let missing: Vec<_> = aosp.difference(&ours).collect();
        let extra: Vec<_> = ours.difference(&aosp).collect();
        if missing.is_empty() && extra.is_empty() {
            Ok(())
        } else {
            Err(Error::Table(format!(
                "rows differ from AOSP fonts.xml's named families: missing {missing:?}, not in AOSP {extra:?}"
            )))
        }
    }
}

/// Typeface.DEFAULT_FAMILY, which Typeface.setSystemFontMap requires.
pub const DEFAULT_FAMILY: &str = "sans-serif";

/// One `<alias>` (alias.json as-is; FontListParser drops one whose `to` is no named family).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Alias {
    pub name: String,
    pub to: String,
    pub weight: Option<String>,
}

#[derive(Deserialize)]
struct OrderEntry {
    lang: Option<String>,
}

/// AOSP's JSON with `//` line comments (alias.json, fallback_order.json; xml_builder.py strips them the same way): comments outside strings removed.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut in_str = false;
        let mut prev = '\0';
        let mut cut = line.len();
        for (i, c) in line.char_indices() {
            if c == '"' && prev != '\\' {
                in_str = !in_str;
            } else if !in_str && c == '/' && prev == '/' {
                cut = i - 1;
                break;
            }
            prev = c;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

pub fn parse_aliases(alias_json: &str) -> Result<Vec<Alias>, Error> {
    serde_json::from_str(&strip_comments(alias_json)).map_err(|e| Error::Json("alias.json", e))
}

/// fallback_order.json's lang entries in order (entries with only `id` name AOSP's own font files: skipped).
pub fn parse_order(order_json: &str) -> Result<Vec<String>, Error> {
    let e: Vec<OrderEntry> = serde_json::from_str(&strip_comments(order_json))
        .map_err(|e| Error::Json("fallback_order.json", e))?;
    Ok(e.into_iter().filter_map(|e| e.lang).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        Table::parse(text).expect("table")
    }

    #[test]
    fn the_tracked_table_matches_aosp_names() {
        let t = table(FONTS_TOML);
        // the ten named families of android-16.0.0_r1 frameworks/base/data/fonts/fonts.xml
        let aosp: String = [
            "casual",
            "cursive",
            "monospace",
            "roboto-flex",
            "sans-serif",
            "sans-serif-condensed",
            "sans-serif-smallcaps",
            "serif",
            "serif-monospace",
            "source-sans-pro",
        ]
        .iter()
        .map(|n| format!("<family name=\"{n}\">"))
        .collect();
        t.check_against_aosp(&aosp).expect("same names");
        assert!(
            t.check_against_aosp("<family name=\"sans-serif\">")
                .is_err()
        );
    }

    #[test]
    fn table_cross_references_are_checked() {
        assert!(Table::parse("[serif]\nquery = \"serif\"\naccept = \"required\"\n").is_err()); // sans-serif not first
        assert!(Table::parse("[sans-serif]\nquery = \"s\"\naccept = \"required\"\n[serif]\nquery = \"serif\"\naccept = \"differs\"\nfrom = \"nope\"\n").is_err());
        assert!(
            Table::parse(
                "[sans-serif]\nquery = \"s\"\naccept = \"required\"\n[x]\nnone = \"r\"\nquery = \"q\"\n"
            )
            .is_err()
        );
    }

    #[test]
    fn comments_outside_strings_are_stripped() {
        assert_eq!(
            strip_comments("{ \"a\": \"//x\" } // y"),
            "{ \"a\": \"//x\" } \n"
        );
    }
}
