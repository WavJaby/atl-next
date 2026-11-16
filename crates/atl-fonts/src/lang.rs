// SPDX-License-Identifier: MIT
//! AOSP fallback lang tags (`fallback_order.json`, BCP-47: `und-Hebr`, `zh-Hant`, `ja`, `und-Zsye`) -> the language ids a desktop font lists (fontconfig's: `he`, `zh-tw`, `ja`, `und-zsye`).
//! Candidates, best first: the tag itself lowercased (fontconfig carries `und-zsye`, `ja`, `ko` as-is); then CLDR's likely language-region and language (`und-Hebr` -> `he-il`, `he`), but only when that language's own likely script is the tag's script: `und-Zsye` maximizes to English, and English text is not emoji.

use icu_locale::LanguageIdentifier;
use icu_locale::LocaleExpander;

/// The candidate ids of one tag; empty for an unparsable tag.
pub fn candidates(expander: &LocaleExpander, tag: &str) -> Vec<String> {
    let mut out = vec![tag.to_ascii_lowercase()];
    let Ok(mut max) = tag.parse::<LanguageIdentifier>() else {
        return vec![];
    };
    expander.maximize(&mut max);
    let mut own = LanguageIdentifier::UNKNOWN;
    own.language = max.language;
    own.region = max.region;
    expander.maximize(&mut own);
    if own.script == max.script && !max.language.is_unknown() {
        if let Some(r) = max.region {
            out.push(format!("{}-{}", max.language, r).to_ascii_lowercase());
        }
        out.push(max.language.to_string());
    }
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|c| seen.insert(c.clone()));
    out
}

/// The script a tag maximizes to (CLDR likelySubtags): the key a fallback group's `covers` names; None for an unparsable tag.
pub fn script(expander: &LocaleExpander, tag: &str) -> Option<String> {
    let mut max = tag.parse::<LanguageIdentifier>().ok()?;
    expander.maximize(&mut max);
    max.script.map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected ids: fontconfig's orth file names (fc-lang/*.orth: he.orth, zh_tw.orth -> `zh-tw`, ja.orth, und_zsye.orth); likely subtags per CLDR likelySubtags.xml (und-Hebr -> he-Hebr-IL, zh-Hant -> zh-Hant-TW, zh-Hans -> zh-Hans-CN).
    #[test]
    fn script_tags_map_to_the_language_a_font_lists() {
        let e = LocaleExpander::new_common();
        assert_eq!(candidates(&e, "und-Hebr"), ["und-hebr", "he-il", "he"]);
        assert_eq!(candidates(&e, "zh-Hant"), ["zh-hant", "zh-tw", "zh"]);
        assert_eq!(candidates(&e, "zh-Hans"), ["zh-hans", "zh-cn", "zh"]);
        assert_eq!(candidates(&e, "ja"), ["ja", "ja-jp"]);
    }

    #[test]
    fn script_is_the_likely_script_of_each_tag() {
        let e = LocaleExpander::new_common();
        // CLDR likelySubtags: ja -> ja-Jpan-JP, ko -> ko-Kore-KR, zh-Hant -> zh-Hant-TW, und-Hebr -> he-Hebr-IL
        assert_eq!(script(&e, "ja").as_deref(), Some("Jpan"));
        assert_eq!(script(&e, "ko").as_deref(), Some("Kore"));
        assert_eq!(script(&e, "zh-Hant").as_deref(), Some("Hant"));
        assert_eq!(script(&e, "zh-Hans").as_deref(), Some("Hans"));
        assert_eq!(script(&e, "und-Zsye").as_deref(), Some("Zsye"));
        assert_eq!(script(&e, "und-Hebr").as_deref(), Some("Hebr"));
        assert_eq!(script(&e, "!!"), None);
    }

    #[test]
    fn emoji_never_falls_to_a_language() {
        let e = LocaleExpander::new_common();
        assert_eq!(candidates(&e, "und-Zsye"), ["und-zsye"]);
    }
}
