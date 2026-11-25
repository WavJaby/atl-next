// SPDX-License-Identifier: MIT
//! Synthetic desktop answers -> model -> XML, read back with the rules of FontListParser.java / SystemFonts.java (line numbers: android-16.0.0_r1).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use atl_fonts_table::parse_aliases;
use atl_platform_host::{Face, FontError, FontQuery, FontSource};

use super::*;

/// A desktop: faces with their languages, plus the sorted answer per (family, lang, width); unlisted queries sort every face in insertion order.
#[derive(Default)]
struct Desktop {
    faces: Vec<(Face, Vec<&'static str>, FaceInfo)>,
    sorted: BTreeMap<(String, Option<String>), Vec<usize>>,
}

impl Desktop {
    fn face(
        &mut self,
        path: &str,
        index: u32,
        family: &str,
        width: i32,
        langs: &[&'static str],
        info: FaceInfo,
    ) -> usize {
        let f = Face {
            path: PathBuf::from(path),
            index,
            family: family.into(),
            width,
        };
        self.faces.push((f, langs.to_vec(), info));
        self.faces.len() - 1
    }
    fn prefer(&mut self, family: &str, lang: Option<&str>, order: &[usize]) {
        self.sorted
            .insert((family.into(), lang.map(Into::into)), order.to_vec());
    }
}

impl FontSource for Desktop {
    fn sorted(&self, q: &FontQuery) -> Result<Vec<Face>, FontError> {
        let order = self
            .sorted
            .get(&(q.family.clone(), q.lang.clone()))
            .cloned()
            .unwrap_or_else(|| (0..self.faces.len()).collect());
        Ok(order.into_iter().map(|i| self.faces[i].0.clone()).collect())
    }
    fn list(&self, q: &FontQuery) -> Result<Vec<Face>, FontError> {
        Ok(self
            .faces
            .iter()
            .filter(|(f, l, _)| {
                (q.family.is_empty() || q.family == f.family)
                    && q.lang.as_ref().is_none_or(|x| l.contains(&x.as_str()))
                    && q.width.is_none_or(|w| w == f.width)
            })
            .map(|(f, _, _)| f.clone())
            .collect())
    }
}

impl FaceReader for Desktop {
    fn info(&self, face: &Face) -> Result<FaceInfo, Error> {
        // the fixture's Type 1 faces: what FileReader answers for a `.t1` / `.pfb` file (face::read's magic check)
        if face.path.extension().is_some_and(|e| e == "t1") {
            return Err(Error::NotSfnt);
        }
        Ok(self
            .faces
            .iter()
            .find(|(f, _, _)| f == face)
            .map(|(_, _, i)| i.clone())
            .expect("a listed face"))
    }
}

fn stat(weight: u16, italic: bool, mono: bool) -> FaceInfo {
    FaceInfo {
        weight,
        italic,
        monospaced: mono,
        postscript: None,
        axes: vec![],
    }
}

/// Roboto's axes (Roboto-Regular.ttf fvar: wght 100..900, ital 0..1).
fn roboto() -> FaceInfo {
    FaceInfo {
        weight: 400,
        italic: false,
        monospaced: false,
        postscript: Some("Roboto-Regular".into()),
        axes: vec![
            Axis {
                tag: *b"wght",
                min: 100.0,
                max: 900.0,
            },
            Axis {
                tag: *b"ital",
                min: 0.0,
                max: 1.0,
            },
        ],
    }
}

fn table(text: &str) -> Table {
    Table::parse(text).expect("table")
}

const MINI: &str = "[sans-serif]\nquery = \"sans-serif\"\naccept = \"required\"\n[serif]\nquery = \"serif\"\naccept = \"differs\"\nfrom = \"sans-serif\"\n[monospace]\nquery = \"monospace\"\naccept = \"monospaced\"\n";

/// The fallback langs AOSP lists for CJK (fallback_order.json) and the fontconfig langs a Noto Sans CJK .ttc lists per face (index 0 JP, 1 KR, 2 SC, 3 TC: NotoSansCJK-Regular.ttc's face order).
fn cjk_desktop() -> Desktop {
    let mut d = Desktop::default();
    let sans = d.face(
        "/f/a/Sans.ttf",
        0,
        "Sans",
        100,
        &["en"],
        stat(400, false, false),
    );
    let jp = d.face(
        "/f/cjk/NotoSansCJK.ttc",
        0,
        "Noto Sans CJK JP",
        100,
        &["ja", "zh-cn", "zh-tw", "ko"],
        stat(400, false, false),
    );
    let kr = d.face(
        "/f/cjk/NotoSansCJK.ttc",
        1,
        "Noto Sans CJK KR",
        100,
        &["ja", "zh-cn", "zh-tw", "ko"],
        stat(400, false, false),
    );
    let sc = d.face(
        "/f/cjk/NotoSansCJK.ttc",
        2,
        "Noto Sans CJK SC",
        100,
        &["ja", "zh-cn", "zh-tw", "ko"],
        stat(400, false, false),
    );
    let tc = d.face(
        "/f/cjk/NotoSansCJK.ttc",
        3,
        "Noto Sans CJK TC",
        100,
        &["ja", "zh-cn", "zh-tw", "ko"],
        stat(400, false, false),
    );
    // fontconfig's 65-nonlatin.conf style answer: each language prefers its own face of the collection
    d.prefer("sans-serif", None, &[sans, jp]);
    d.prefer("sans-serif", Some("ja"), &[sans, jp, kr, sc, tc]);
    d.prefer("sans-serif", Some("ko"), &[sans, kr, jp, sc, tc]);
    d.prefer("sans-serif", Some("zh-cn"), &[sans, sc, tc, jp, kr]);
    d.prefer("sans-serif", Some("zh-tw"), &[sans, tc, sc, jp, kr]);
    d
}

fn langs() -> Vec<String> {
    ["zh-Hans", "zh-Hant,zh-Bopo", "ja", "ko", "und-Zsye"]
        .map(String::from)
        .to_vec()
}

#[test]
fn cjk_chain_takes_each_language_its_own_face_of_the_collection() {
    let d = cjk_desktop();
    let m = build(&d, &d, &table(MINI), vec![], &langs()).expect("model");
    let chain: Vec<(&str, u32)> = m
        .families
        .iter()
        .filter_map(|f| Some((f.lang.as_deref()?, f.fonts[0].index)))
        .collect();
    // ttc indices per face order above; und-Zsye: no face lists it -> omitted, never the head taken blindly
    assert_eq!(
        chain,
        [("zh-Hans", 2), ("zh-Hant,zh-Bopo", 3), ("ja", 0), ("ko", 1)]
    );
}

#[test]
fn rejected_rows_emit_nothing_and_sans_serif_comes_first() {
    let d = cjk_desktop();
    // serif and monospace both answer Sans: serif = sans-serif's head (differs fails), Sans is proportional (monospaced fails)
    let m = build(&d, &d, &table(MINI), vec![], &[]).expect("model");
    let named: Vec<_> = m
        .families
        .iter()
        .filter_map(|f| f.name.as_deref())
        .collect();
    assert_eq!(named, ["sans-serif"]);
}

#[test]
fn no_sans_serif_fails_the_start() {
    let d = Desktop::default();
    assert!(matches!(
        build(&d, &d, &table(MINI), vec![], &[]),
        Err(Error::Required(..))
    ));
}

/// Type 1 headers (PFB segment marker 0x80 0x01; PFA/`.t1` text `%!PS`) are no sfnt: the error the builder skips, not a parse failure that stops atld.
#[test]
fn type1_files_read_as_not_sfnt() {
    for header in [&b"\x80\x01\x84\x03\0\0"[..], b"%!PS-AdobeFont-1.0"] {
        assert!(matches!(crate::face::read(header, 0), Err(Error::NotSfnt)));
    }
}

/// A desktop lists Type 1 faces too (Debian/Arch urw-base35: `.t1`, `.pfb`): a family's Type 1 member is skipped and the build goes on; a Type 1 head is no answer, like an unnameable one.
#[test]
fn type1_faces_are_skipped_not_fatal() {
    let mut d = Desktop::default();
    let sans = d.face(
        "/f/a/Sans.ttf",
        0,
        "Sans",
        100,
        &["en"],
        stat(400, false, false),
    );
    let t1 = d.face(
        "/f/b/Sans-Bold.t1",
        0,
        "Sans",
        100,
        &["en"],
        stat(700, false, false),
    );
    d.prefer("sans-serif", None, &[sans, t1]);
    let m = build(&d, &d, &table(MINI), vec![], &[]).expect("a Type 1 member is skipped");
    let files: Vec<&str> = m.families[0]
        .fonts
        .iter()
        .map(|f| f.file.as_str())
        .collect();
    assert_eq!(files, ["Sans.ttf"]);
    d.prefer("sans-serif", None, &[t1, sans]);
    assert!(matches!(
        build(&d, &d, &table(MINI), vec![], &[]),
        Err(Error::Required(..))
    ));
}

#[test]
fn files_with_whitespace_are_never_named() {
    let mut d = Desktop::default();
    let a = d.face(
        "/f/a/My Sans.ttf",
        0,
        "My Sans",
        100,
        &["en"],
        stat(400, false, false),
    );
    let b = d.face(
        "/f/a/Other.ttf",
        0,
        "Other",
        100,
        &["en"],
        stat(400, false, false),
    );
    d.prefer("sans-serif", None, &[a, b]);
    // FontListParser.readFont strips whitespace from the file text (FILENAME_WHITESPACE_PATTERN): such a head is unusable, so sans-serif has no font
    assert!(matches!(
        build(&d, &d, &table(MINI), vec![], &[]),
        Err(Error::Required(..))
    ));
}

/// A desktop answering sans-serif with `faces` (path, tables; one family, Sans), and face `ja` for Japanese.
fn one_family(faces: &[(&str, FaceInfo)], ja: usize) -> Desktop {
    let mut d = Desktop::default();
    for (path, info) in faces {
        d.face(path, 0, "Sans", 100, &["ja"], info.clone());
    }
    d.prefer("sans-serif", None, &[0]);
    d.prefer("sans-serif", Some("ja"), &[ja]);
    d
}

/// The (dir, file) of family `k`'s fonts, as listed.
fn files(m: &Model, k: usize) -> Vec<(usize, &str)> {
    m.families[k]
        .fonts
        .iter()
        .map(|f| (f.dir, f.file.as_str()))
        .collect()
}

/// k3's Noto Sans: Thin and ExtraLight both usWeightClass 250, and a second package's Bold and Regular in another dir.
#[test]
fn a_family_holds_each_style_once() {
    let d = one_family(
        &[
            ("/f/o/Regular.ttf", stat(400, false, false)),
            ("/f/o/Bold.ttf", stat(700, false, false)),
            ("/f/o/ExtraLight.ttf", stat(250, false, false)),
            ("/f/o/Thin.ttf", stat(250, false, false)),
            ("/f/t/Bold.ttf", stat(700, false, false)),
            ("/f/t/BoldItalic.ttf", stat(700, true, false)),
            ("/f/t/Regular.ttf", stat(400, false, false)),
        ],
        0,
    );
    let m = build(&d, &d, &table(MINI), vec![], &[]).expect("model");
    check_schema(&m.font_fallback_xml(), &m);
    check_schema(&m.fonts_xml(), &m);
    assert_eq!(
        files(&m, 0),
        [
            (0, "Regular.ttf"),
            (0, "Bold.ttf"),
            (0, "ExtraLight.ttf"),
            (1, "BoldItalic.ttf")
        ]
    );
}

/// A static face beside a variable one of its family: the instances leave out the style the static head has.
#[test]
fn a_variable_face_leaves_out_a_static_heads_style() {
    let d = one_family(
        &[
            ("/f/a/Sans-Regular.ttf", stat(400, false, false)),
            ("/f/a/Sans-VF.ttf", roboto()),
        ],
        0,
    );
    let m = build(&d, &d, &table(MINI), vec![], &[]).expect("model");
    check_schema(&m.font_fallback_xml(), &m);
    check_schema(&m.fonts_xml(), &m);
    // the static 400 upright + the VF's 9 weights x 2 slants without that one
    assert_eq!(m.fonts_xml().matches("<font ").count(), 18);
}

/// A face dropped for its style is not in the list: it still answers a language the desktop prefers it for.
#[test]
fn a_face_dropped_for_its_style_still_answers_a_language() {
    let d = one_family(
        &[
            ("/f/o/Regular.ttf", stat(400, false, false)),
            ("/f/t/Regular.ttf", stat(400, false, false)),
        ],
        1,
    );
    let m = build(&d, &d, &table(MINI), vec![], &["ja".into()]).expect("model");
    assert_eq!(files(&m, 0), [(0, "Regular.ttf")]);
    assert_eq!(m.families[1].lang.as_deref(), Some("ja"));
    assert_eq!(files(&m, 1), [(1, "Regular.ttf")]);
}

/// The XML back through FontListParser's rules.
fn check_schema(xml: &str, m: &Model) {
    let doc = roxmltree::Document::parse(xml).expect("well-formed");
    let root = doc.root_element();
    assert_eq!(root.tag_name().name(), "familyset"); // readFamilies 168
    let fams: Vec<_> = root
        .children()
        .filter(|n| n.has_tag_name("family"))
        .collect();
    // the first family is also the default fallback (193-196): sans-serif
    assert_eq!(fams[0].attribute("name"), Some(DEFAULT_FAMILY));
    let mut seen = BTreeSet::new();
    for f in &fams {
        // a named family carries no lang/variant/ignore (310-325)
        if f.attribute("name").is_some() {
            assert!(f.attribute("lang").is_none());
        }
        let mut styles = BTreeSet::new();
        for font in f.children().filter(|n| n.has_tag_name("font")) {
            // FontFamily.Builder.addFont throws on a style a second time (readFont's defaults: weight 400, upright)
            assert!(
                styles.insert((
                    font.attribute("weight").unwrap_or("400"),
                    font.attribute("style") == Some("italic")
                )),
                "{font:?}: a style twice in one family"
            );
            let text: String = font
                .children()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>()
                .split_whitespace()
                .collect();
            let (k, file) = text.split_once('/').expect("<k>/<file>");
            assert!(k.parse::<usize>().expect("k") < m.dirs.len());
            assert!(!file.is_empty());
            assert!(font.attribute("postScriptName").is_some()); // default would be text minus 4 chars (409-413)
            if let Some(ax) = font.attribute("supportedAxes") {
                assert!(ax.split(',').all(|a| a == "wght" || a == "ital")); // the only two readFont knows (397-407)
            }
            if f.attribute("lang").is_some() {
                assert!(seen.insert((
                    text,
                    font.attribute("index").unwrap_or("0").to_owned(),
                    font.attribute("weight").map(str::to_owned),
                    font.attribute("style").map(str::to_owned)
                )));
            }
        }
    }
    for a in root.children().filter(|n| n.has_tag_name("alias")) {
        assert!(a.attribute("name").is_some() && a.attribute("to").is_some()); // readAlias 471
    }
}

#[test]
fn both_files_follow_the_parser_schema() {
    let mut d = cjk_desktop();
    d.faces[0].2 = roboto();
    let aliases = parse_aliases("[\n  // c\n  { \"name\": \"arial\", \"to\": \"sans-serif\" },\n  { \"name\": \"sans-serif-light\", \"to\": \"sans-serif\", \"weight\": \"300\" }\n]").expect("aliases");
    let m = build(&d, &d, &table(MINI), aliases, &langs()).expect("model");
    check_schema(&m.font_fallback_xml(), &m);
    check_schema(&m.fonts_xml(), &m);
    // xml_builder.py write_xml: no version attribute on the generated font_fallback.xml
    assert!(m.font_fallback_xml().contains("\n<familyset>\n"));
    assert!(m.fonts_xml().contains("<familyset version=\"23\">"));
    assert!(
        m.fonts_xml()
            .contains("<alias name=\"sans-serif-light\" to=\"sans-serif\" weight=\"300\" />")
    );
}

fn groups() -> Vec<atl_fonts_table::Group> {
    atl_fonts_table::groups(atl_fonts_table::DEPS_TOML).expect("groups")
}

/// A desktop with Sans only: no lang has a face, and serif and monospace answer Sans, which they reject.
fn sans_only() -> Desktop {
    let mut d = Desktop::default();
    d.face(
        "/f/a/Sans.ttf",
        0,
        "Sans",
        100,
        &["en"],
        stat(400, false, false),
    );
    d
}

#[test]
fn a_desktop_without_cjk_or_emoji_names_both_groups() {
    let d = sans_only();
    let m = build(&d, &d, &table(MINI), vec![], &langs()).expect("model");
    assert_eq!(m.missing.langs, langs());
    assert_eq!(m.missing.families, ["serif", "monospace"]);
    assert_eq!(
        report(&m, &groups()),
        [
            "no CJK face on this desktop (zh-Hans, zh-Hant,zh-Bopo, ja, ko render as boxes); install: apt install fonts-noto-cjk | pacman -S noto-fonts-cjk",
            "no emoji face on this desktop (und-Zsye render as boxes); install: apt install fonts-noto-color-emoji | pacman -S noto-fonts-emoji",
            "diag missing_groups=2 missing_langs=5 missing_families=2",
        ]
    );
}

#[test]
fn a_desktop_with_cjk_and_emoji_names_no_group() {
    let mut d = cjk_desktop();
    d.face(
        "/f/emoji/NotoColorEmoji.ttf",
        0,
        "Noto Color Emoji",
        100,
        &["und-zsye"],
        stat(400, false, false),
    );
    let m = build(&d, &d, &table(MINI), vec![], &langs()).expect("model");
    assert!(m.missing.langs.is_empty());
    assert_eq!(
        report(&m, &groups()),
        ["diag missing_groups=0 missing_langs=0 missing_families=2"]
    );
}

#[test]
fn the_tracked_groups_cover_known_scripts() {
    let e = icu_locale::LocaleExpander::new_common();
    for g in groups() {
        for s in &g.covers {
            // ICU reads `und-<s>` back as script `s` only when `s` is an ISO 15924 script
            assert_eq!(lang::script(&e, &format!("und-{s}")), Some(s.clone()));
        }
    }
}

#[test]
fn a_variable_family_takes_supported_axes_only_in_font_fallback() {
    let mut d = Desktop::default();
    d.face(
        "/f/r/Roboto-Regular.ttf",
        0,
        "Roboto",
        100,
        &["en"],
        roboto(),
    );
    let m = build(&d, &d, &table(MINI), vec![], &[]).expect("model");
    // resolveVarFamilyType 130-191: one font with wght+ital -> SINGLE_FONT_WGHT_ITAL
    assert!(m.font_fallback_xml().contains("<font style=\"normal\" postScriptName=\"Roboto-Regular\" supportedAxes=\"wght,ital\">0/Roboto-Regular.ttf</font>"));
    let legacy = m.fonts_xml();
    // 100..900 step 100 inside wght 100..900 = 9 weights, x2 for ital 0/1
    assert_eq!(legacy.matches("<font ").count(), 18);
    assert!(legacy.contains("<font weight=\"700\" style=\"italic\" postScriptName=\"Roboto-Regular\">0/Roboto-Regular.ttf\n            <axis tag=\"wght\" stylevalue=\"700\"/>\n            <axis tag=\"ital\" stylevalue=\"1\"/>"));
}
