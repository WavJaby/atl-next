// SPDX-License-Identifier: MIT
//! The system font list of `ASystemFontIterator` / `AFont`, as AOSP's `system_fonts.cpp` reads it from the font configuration files.
//!
//! AOSP's iterator first asks minikin for the font set the framework registered in this process (`SystemFonts::getFontSet`, deduplicated, unordered) and reads the XML only when that set is empty.
//! Here the XML is the only source: minikin's set is C++ state of libminikin, and the plan generates `fonts.xml` from the same font model the framework's font map comes from, so both name the same fonts.
//!
//! AOSP's XML walk (`findFirstFontNode` / `findNextFontNode` / `copyFont`), for each file in turn -- `/system/etc/fonts.xml` (font names relative to `/system/fonts/`), then `/product/etc/fonts_customization.xml` (`/product/fonts/`); a file that is missing or not well-formed XML gives nothing:
//! - every `<family>` child of the root element, in document order; every `<font>` child of a family; nothing else is read (`name`, `variant`, `fallbackFor`, `postScriptName`, `<alias>` are not part of the NDK font).
//! - path: the font element's own text nodes concatenated, trimmed of XML whitespace, appended to the directory.
//! - weight: `weight`, as `strtol` base 10 cast to `uint16_t`; absent = 400. italic: `style` equal to `italic`. collection index: `index`, `strtol` cast to `uint32_t`; absent = 0.
//! - locale: the family's `lang`, verbatim (`und-Geor,und-Geok` stays a list); absent = none.
//! - axes: each `<axis>` child with a 4-byte `tag` and a `stylevalue` (`strtod`, as `float`), the tag big-endian (`'w','g','h','t'` = 0x77676874); others skipped.
//! - a font whose path is not a regular file (`stat`, symlinks followed) is skipped when the iterator reaches it.
//!
//! Divergence: AOSP's walk on a `<family>` with no `<font>` misbehaves -- first family empty: the next font gets the empty family's `lang`; later: it loops forever on the same family. Here an empty family is skipped and each font carries its own family's `lang`. A generated `fonts.xml` has no empty family.
//! Divergence: `strtod`'s hexadecimal and `nan(...)` forms parse as their longest decimal prefix.

use std::collections::VecDeque;
use std::ffi::CString;

/// Environment variable naming a directory that stands for `/` in the font configuration paths: the files are read at `$ATL_FONT_ROOT/system/etc/fonts.xml`, ..., and font paths returned under it.
/// Unset = AOSP's absolute paths, where the sandbox layout puts the files (plan "sandbox layout table"); set by whoever starts the process until the broker does.
/// Not valid UTF-8 = no fonts (fail closed; never the host's `/system`).
pub const ROOT_ENV: &str = "ATL_FONT_ROOT";

/// AOSP's configuration files and the directory each one's font names are relative to, in reading order (`ASystemFontIterator_open`).
const SOURCES: &[(&str, &str)] = &[
    ("/system/etc/fonts.xml", "/system/fonts/"),
    ("/product/etc/fonts_customization.xml", "/product/fonts/"),
];

/// AOSP's default weight of a `<font>` without `weight` (`copyFont`; = `AFONT_WEIGHT_NORMAL`).
const DEFAULT_WEIGHT: u16 = 400;

/// One font of the configuration (`struct AFont`).
#[derive(Debug, Clone, PartialEq)]
pub struct Font {
    pub path: CString,
    pub locale: Option<CString>,
    pub weight: u16,
    pub italic: bool,
    pub collection_index: u32,
    /// (OpenType tag, value) in document order.
    pub axes: Vec<(u32, f32)>,
}

/// `ASystemFontIterator`: the fonts of the configuration not yet returned.
#[derive(Debug, Default)]
pub struct SystemFontIterator {
    pending: VecDeque<Font>,
}

impl SystemFontIterator {
    /// The configuration under `ROOT_ENV`.
    pub fn open() -> Self {
        match std::env::var(ROOT_ENV) {
            Ok(root) => Self::open_at(&root),
            Err(std::env::VarError::NotPresent) => Self::open_at(""),
            Err(std::env::VarError::NotUnicode(_)) => Self::default(),
        }
    }

    /// The configuration with `root` (`""` = `/`) prepended to AOSP's absolute paths; trailing `/` of `root` dropped.
    pub fn open_at(root: &str) -> Self {
        let root = root.trim_end_matches('/');
        let mut pending = VecDeque::new();
        for (config, dir) in SOURCES {
            // unreadable as UTF-8 text is as missing: libxml2 would fail the same file on encoding
            if let Ok(xml) = std::fs::read_to_string(format!("{root}{config}")) {
                pending.extend(parse(&xml, &format!("{root}{dir}")));
            }
        }
        SystemFontIterator { pending }
    }

    /// The next font whose file exists (`ASystemFontIterator_next`); none at the end.
    pub fn next_font(&mut self) -> Option<Font> {
        while let Some(font) = self.pending.pop_front() {
            if is_regular_file(&font.path) {
                return Some(font);
            }
        }
        None
    }
}

/// `isFontFileAvailable`: `stat` succeeds and names a regular file.
fn is_regular_file(path: &CString) -> bool {
    path.to_str()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .is_some_and(|m| m.is_file())
}

/// The fonts of one configuration file's text, paths prefixed with `dir`; none when it is not well-formed.
pub fn parse(xml: &str, dir: &str) -> Vec<Font> {
    let Ok(doc) = roxmltree::Document::parse(xml) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for family in elements(doc.root_element(), "family") {
        let locale = prop(family, "lang").and_then(|l| CString::new(l).ok());
        for font in elements(family, "font") {
            out.extend(copy_font(font, locale.clone(), dir));
        }
    }
    out
}

/// `copyFont`: one `<font>` element; none only for a path holding NUL, which XML text cannot.
fn copy_font(node: roxmltree::Node, locale: Option<CString>, dir: &str) -> Option<Font> {
    let text: String = node
        .children()
        .filter(|c| c.is_text())
        .filter_map(|c| c.text())
        .collect();
    let path = CString::new(format!("{dir}{}", xml_trim(&text))).ok()?;
    let axes = elements(node, "axis")
        .filter_map(|axis| {
            let tag: [u8; 4] = prop(axis, "tag")?.as_bytes().try_into().ok()?;
            let value = strtod(prop(axis, "stylevalue")?) as f32;
            Some((u32::from_be_bytes(tag), value))
        })
        .collect();
    Some(Font {
        path,
        locale,
        // C's conversion of `long` to an unsigned type: modulo 2^N, which `as` is
        weight: prop(node, "weight").map_or(DEFAULT_WEIGHT, |w| strtol(w) as u16),
        italic: prop(node, "style") == Some("italic"),
        collection_index: prop(node, "index").map_or(0, |i| strtol(i) as u32),
        axes,
    })
}

/// Element children of `node` named `name` (`firstElement` / `nextSibling`: the local name).
fn elements<'a, 'i>(
    node: roxmltree::Node<'a, 'i>,
    name: &'static str,
) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    node.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == name)
}

/// `xmlGetProp`: the first attribute of that local name, whatever its namespace.
fn prop<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|a| a.name() == name)
        .map(|a| a.value())
}

/// `xmlTrim`: XML whitespace (space, CR, LF, tab) off both ends.
fn xml_trim(s: &str) -> &str {
    s.trim_matches([' ', '\r', '\n', '\t'])
}

/// C's `isspace` in the C locale.
fn c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// `strtol(s, nullptr, 10)` on LP64: leading space, a sign, the longest digit run; 0 without digits, saturated at `long`'s range.
fn strtol(s: &str) -> i64 {
    let s = s.trim_start_matches(c_space);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut value: i64 = 0;
    for d in digits.bytes().take_while(u8::is_ascii_digit) {
        let d = i64::from(d - b'0');
        value = if negative {
            value.saturating_mul(10).saturating_sub(d)
        } else {
            value.saturating_mul(10).saturating_add(d)
        };
    }
    value
}

/// `strtod(s, nullptr)`: leading space, then the longest prefix that is a decimal floating constant, `inf` / `infinity` or `nan`; 0 when there is none.
fn strtod(s: &str) -> f64 {
    let s = s.trim_start_matches(c_space);
    // Rust's float grammar is C's decimal one plus the same inf/nan words; a char boundary is needed to slice
    (1..=s.len())
        .rev()
        .filter(|&end| s.is_char_boundary(end))
        .find_map(|end| s[..end].parse::<f64>().ok())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtol_is_c_strtol() {
        // C11 7.22.1.4: leading isspace, optional sign, digits up to the first non-digit; none -> 0
        assert_eq!(strtol("700"), 700);
        assert_eq!(strtol(" \t+42abc"), 42);
        assert_eq!(strtol("-5"), -5);
        assert_eq!(strtol("bold"), 0);
        assert_eq!(strtol(""), 0);
        // out of range -> LONG_MAX / LONG_MIN (LP64: 2^63 - 1 / -2^63)
        assert_eq!(strtol("99999999999999999999"), i64::MAX);
        assert_eq!(strtol("-99999999999999999999"), i64::MIN);
        // uint16_t conversion of 65936: 65936 - 65536 = 400
        assert_eq!(strtol("65936") as u16, 400);
    }

    #[test]
    fn strtod_is_c_strtod() {
        assert_eq!(strtod("100"), 100.0);
        assert_eq!(strtod(" 0.5"), 0.5);
        assert_eq!(strtod("-1e2x"), -100.0);
        // "1e" has no exponent digits: the constant is "1"
        assert_eq!(strtod("1e"), 1.0);
        assert_eq!(strtod("abc"), 0.0);
        assert!(strtod("inf").is_infinite());
    }

    #[test]
    fn xml_trim_is_aosps() {
        assert_eq!(
            xml_trim("\n   Roboto-Regular.ttf\n  "),
            "Roboto-Regular.ttf"
        );
        // only XML's four whitespace characters: a vertical tab stays
        assert_eq!(xml_trim("\x0ba.ttf"), "\x0ba.ttf");
    }

    #[test]
    fn copy_font_defaults() {
        let fonts = parse(
            "<familyset><family><font>a.ttf</font></family></familyset>",
            "/d/",
        );
        assert_eq!(
            fonts,
            [Font {
                path: c"/d/a.ttf".into(),
                locale: None,
                weight: DEFAULT_WEIGHT,
                italic: false,
                collection_index: 0,
                axes: vec![],
            }]
        );
    }

    #[test]
    fn only_families_and_fonts_count() {
        // the root's name is not checked; an empty family is skipped (see the module's divergence note); text after an axis is part of the name, as xmlNodeListGetString concatenates text children
        let xml = "<x><alias name=\"a\" to=\"b\"/><family lang=\"ja\"/><family lang=\"ko\"><font style=\"oblique\" weight=\"x\">a<axis tag=\"wdth\"/><axis tag=\"wght1\" stylevalue=\"1\"/>b</font><other><font>c.ttf</font></other></family></x>";
        let fonts = parse(xml, "");
        assert_eq!(fonts.len(), 1);
        assert_eq!(fonts[0].path.as_c_str(), c"ab");
        assert_eq!(fonts[0].locale.as_deref(), Some(c"ko"));
        assert!(!fonts[0].italic);
        assert_eq!(fonts[0].weight, 0);
        assert!(fonts[0].axes.is_empty());
    }

    #[test]
    fn not_xml_is_no_fonts() {
        assert!(parse("<familyset><family>", "").is_empty());
        assert!(parse("", "").is_empty());
    }
}
