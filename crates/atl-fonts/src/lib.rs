// SPDX-License-Identifier: MIT
//! Android's system font list from the desktop's (plan C7). fontconfig (through the platform layer's [`FontSource`]) answers which faces, in what order, for which language; the font's own tables answer what a face is (face.rs).
//! Inputs: the tracked `fonts.toml` (named families), AOSP's `alias.json` and `fallback_order.json` (frameworks/base/data/fonts, read as-is). Output: [`Model`], written as `font_fallback.xml` (SystemFonts.FONTS_XML, the Java/minikin path) and the legacy `fonts.xml` (NDK ASystemFontIterator, bundled Skia's SkFontMgr_android) by xml.rs.
//! Faces are named `<k>/<basename>` below /system/fonts/ (FontListParser.readFont: path = fontDir + text, so subdirectories work), k = index of the face's directory in [`Model::dirs`], which the sandbox binds read-only at /system/fonts/<k>.
//! Every named family's chain = its own fonts + every unnamed lang family in document order (SystemFonts.buildSystemFallback): fallback is one global list.
#![forbid(unsafe_code)]

mod face;
mod lang;
mod xml;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use atl_platform_host::{Face, FontQuery, FontSource};
use serde::Deserialize;

pub use face::{Axis, FaceInfo};

/// The tracked table (repo root `fonts.toml`), compiled in: rows change with the code that reads them.
pub const FONTS_TOML: &str = include_str!("../../../fonts.toml");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fonts.toml: {0}")]
    Table(String),
    #[error("{0}: {1}")]
    Json(&'static str, serde_json::Error),
    #[error(transparent)]
    Source(#[from] atl_platform_host::FontError),
    #[error("font table: {0}")]
    Face(String),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("the desktop has no font for `{0}` (fontconfig answers no face for `{1}`)")]
    Required(String, String),
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

/// One `<font>`: a face with what its tables say.
#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    /// Index into [`Model::dirs`].
    pub dir: usize,
    pub file: String,
    pub index: u32,
    pub info: FaceInfo,
}

/// One `<family>`: named (`name`) or a lang fallback (`lang`, AOSP's string verbatim).
#[derive(Debug, Clone, PartialEq)]
pub struct Family {
    pub name: Option<String>,
    pub lang: Option<String>,
    pub fonts: Vec<Font>,
}

/// One `<alias>` (alias.json as-is; FontListParser drops one whose `to` is no named family).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Alias {
    pub name: String,
    pub to: String,
    pub weight: Option<String>,
}

/// The system font list: named families first (sans-serif first), then the lang fallbacks in fallback_order.json's order.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub families: Vec<Family>,
    pub aliases: Vec<Alias>,
    /// Host directories of every listed face, canonical and sorted: dir k is /system/fonts/<k>.
    pub dirs: Vec<PathBuf>,
}

impl Model {
    /// `/system/etc/font_fallback.xml`.
    pub fn font_fallback_xml(&self) -> String {
        xml::write(self, xml::Flavor::Fallback)
    }

    /// The legacy `/system/etc/fonts.xml`.
    pub fn fonts_xml(&self) -> String {
        xml::write(self, xml::Flavor::Legacy)
    }
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

/// Reads a face's tables; the file is read through `read` (tests feed synthetic bytes).
pub trait FaceReader {
    fn info(&self, face: &Face) -> Result<FaceInfo, Error>;
}

/// Reads font files from the host filesystem.
pub struct FileReader;

impl FaceReader for FileReader {
    fn info(&self, face: &Face) -> Result<FaceInfo, Error> {
        let data = std::fs::read(&face.path).map_err(|e| Error::Io(face.path.clone(), e))?;
        face::read(&data, face.index)
    }
}

/// FontListParser removes whitespace from the file text (FILENAME_WHITESPACE_PATTERN), so a file whose name holds any cannot be named.
fn nameable(f: &Face) -> bool {
    f.path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| !n.chars().any(char::is_whitespace))
}

struct Builder<'a, S, R> {
    src: &'a S,
    read: &'a R,
    dirs: BTreeMap<PathBuf, ()>,
    raw: Vec<(Family, Vec<(Face, FaceInfo)>)>,
}

impl<S: FontSource, R: FaceReader> Builder<'_, S, R> {
    /// The faces of `head`'s family at its width, head first, nameable ones only.
    fn family_faces(&self, head: &Face) -> Result<Vec<(Face, FaceInfo)>, Error> {
        let mut faces = self.src.list(&FontQuery {
            family: head.family.clone(),
            lang: None,
            width: Some(head.width),
        })?;
        faces.retain(|f| f != head && nameable(f));
        faces.insert(0, head.clone());
        faces
            .into_iter()
            .map(|f| Ok((f.clone(), self.read.info(&f)?)))
            .collect()
    }

    fn head(
        &self,
        family: &str,
        lang: Option<&str>,
        width: Option<i32>,
    ) -> Result<Option<Face>, Error> {
        let q = FontQuery {
            family: family.to_owned(),
            lang: lang.map(str::to_owned),
            width,
        };
        Ok(self.src.sorted(&q)?.into_iter().next())
    }
}

/// Builds the model: one desktop query per named row, one per fallback lang.
pub fn build(
    src: &impl FontSource,
    read: &impl FaceReader,
    table: &Table,
    aliases: Vec<Alias>,
    order: &[String],
) -> Result<Model, Error> {
    let mut b = Builder {
        src,
        read,
        dirs: BTreeMap::new(),
        raw: Vec::new(),
    };
    let mut heads: BTreeMap<&str, Face> = BTreeMap::new();
    for (name, row) in &table.rows {
        let (Some(query), Some(accept)) = (&row.query, row.accept) else {
            continue;
        };
        let Some(head) = b.head(query, None, row.width)?.filter(nameable) else {
            if accept == Accept::Required {
                return Err(Error::Required(name.clone(), query.clone()));
            }
            continue;
        };
        let faces = b.family_faces(&head)?;
        let ok = match accept {
            Accept::Required => true,
            Accept::Differs => row
                .from
                .as_deref()
                .and_then(|f| heads.get(f))
                .is_none_or(|h| h != &head),
            Accept::Monospaced => faces[0].1.monospaced,
            Accept::Narrower => head.width < 100,
            Accept::Named => head.family.eq_ignore_ascii_case(query),
        };
        heads.insert(name, head);
        if ok {
            let fam = Family {
                name: Some(name.clone()),
                lang: None,
                fonts: vec![],
            };
            b.raw.push((fam, faces));
        }
    }
    let expander = icu_locale::LocaleExpander::new_common();
    let mut used: BTreeSet<(PathBuf, u32)> = b
        .raw
        .iter()
        .flat_map(|(_, fs)| fs.iter().map(|(f, _)| (f.path.clone(), f.index)))
        .collect();
    for l in order {
        let Some(head) = lang_head(&b, &expander, l)? else {
            continue;
        };
        if !used.insert((head.path.clone(), head.index)) {
            continue;
        }
        let mut faces = b.family_faces(&head)?;
        faces.retain(|(f, _)| f == &head || used.insert((f.path.clone(), f.index)));
        let fam = Family {
            name: None,
            lang: Some(l.clone()),
            fonts: vec![],
        };
        b.raw.push((fam, faces));
    }
    for (_, faces) in &b.raw {
        for (f, _) in faces {
            b.dirs.insert(dir_of(&f.path)?, ());
        }
    }
    let dirs: Vec<PathBuf> = b.dirs.into_keys().collect();
    let mut families = Vec::new();
    for (mut fam, faces) in b.raw {
        for (f, info) in faces {
            let dir = dirs
                .binary_search(&dir_of(&f.path)?)
                .map_err(|_| Error::Face("dir vanished".into()))?;
            // the bound dir is the canonical one, so the name is the link target's
            let real = f.path.canonicalize().unwrap_or_else(|_| f.path.clone());
            let file = real
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            fam.fonts.push(Font {
                dir,
                file,
                index: f.index,
                info,
            });
        }
        families.push(fam);
    }
    Ok(Model {
        families,
        aliases,
        dirs,
    })
}

fn dir_of(p: &Path) -> Result<PathBuf, Error> {
    let c = p.canonicalize().unwrap_or_else(|_| p.to_owned());
    Ok(c.parent().map(Path::to_owned).unwrap_or_default())
}

/// The face the desktop prefers for fallback lang `l` (a comma list), among faces that list the language: fontconfig's sorted answer always holds *something*, so membership is checked, never the head taken blindly.
fn lang_head<S: FontSource, R: FaceReader>(
    b: &Builder<'_, S, R>,
    e: &icu_locale::LocaleExpander,
    l: &str,
) -> Result<Option<Face>, Error> {
    for tag in l.split(',') {
        for fl in lang::candidates(e, tag.trim()) {
            let members: BTreeSet<(PathBuf, u32)> = b
                .src
                .list(&FontQuery {
                    family: String::new(),
                    lang: Some(fl.clone()),
                    width: None,
                })?
                .into_iter()
                .map(|f| (f.path, f.index))
                .collect();
            if members.is_empty() {
                continue;
            }
            let sorted = b.src.sorted(&FontQuery {
                family: DEFAULT_FAMILY.into(),
                lang: Some(fl),
                width: None,
            })?;
            if let Some(f) = sorted
                .into_iter()
                .find(|f| members.contains(&(f.path.clone(), f.index)) && nameable(f))
            {
                return Ok(Some(f));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
