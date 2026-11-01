// SPDX-License-Identifier: MIT
//! Generates, with atl-bridgegen for the target cargo builds for: `OUT_DIR/<api>.rs` per NDK header (the opaque types, and the check that each export of `crate::ffi` has the header's signature).
//! Fails unless each export is in libandroid's NDK symbol file: declared (header) == implemented (`crate::ffi`) == exported (map).
//! No header is printed: the NDK headers are public and the tree's header modules put AOSP's own on every dependent's include path (frameworks/native/include).

use anyhow::Result;
use atl_bridgegen::Lang;
use atl_bridgegen::buildscript::{self, Api, DECLARATIONS};

/// libandroid's NDK symbol file: the library's export list.
const SYMBOL_FILE: &str = "frameworks/base/native/android/libandroid.map.txt";

const APIS: &[Api] = &[
    Api {
        name: "font",
        source: "frameworks/native/include/android/font.h",
        lang: Lang::C,
        header: None,
        functions: &[
            "AFont_close",
            "AFont_getFontFilePath",
            "AFont_getWeight",
            "AFont_isItalic",
            "AFont_getLocale",
            "AFont_getCollectionIndex",
            "AFont_getAxisCount",
            "AFont_getAxisTag",
            "AFont_getAxisValue",
        ],
        macros: &[],
        include_dirs: &[],
        kept: DECLARATIONS,
    },
    // `AFontMatcher_*` (font_matcher.h) are not implemented: they itemize text through minikin's system font collections, and nothing built here calls them.
    Api {
        name: "system_fonts",
        source: "frameworks/native/include/android/system_fonts.h",
        lang: Lang::C,
        header: None,
        functions: &[
            "ASystemFontIterator_open",
            "ASystemFontIterator_close",
            "ASystemFontIterator_next",
        ],
        macros: &[],
        include_dirs: &["frameworks/native/include"],
        kept: DECLARATIONS,
    },
];

fn main() -> Result<()> {
    let mut rerun = buildscript::api_crate(APIS, "crate::ffi")?;
    rerun.extend(buildscript::listed_in_symbol_file(APIS, SYMBOL_FILE)?);
    for path in rerun {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    Ok(())
}
