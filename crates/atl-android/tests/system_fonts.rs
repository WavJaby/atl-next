// SPDX-License-Identifier: MIT
//! The exports as C calls them, on testdata's configuration under `ATL_FONT_ROOT`. Expected values are AOSP `system_fonts.cpp`'s reading of the fixtures, derived by hand:
//! font names under `/system/fonts/` then `/product/fonts/`; the family's `lang`; `weight` / `index` as numbers; `style="italic"`; each 4-byte axis tag big-endian with its value; a font whose file is missing (NotoSansSymbols) skipped.
//! One test: it sets the process's environment, which nothing else may read meanwhile.

use std::ffi::{CStr, c_char};
use std::path::Path;

use atl_android as _;

#[repr(C)]
struct AFont([u8; 0]);
#[repr(C)]
struct ASystemFontIterator([u8; 0]);

unsafe extern "C" {
    fn ASystemFontIterator_open() -> *mut ASystemFontIterator;
    fn ASystemFontIterator_close(it: *mut ASystemFontIterator);
    fn ASystemFontIterator_next(it: *mut ASystemFontIterator) -> *mut AFont;
    fn AFont_close(font: *mut AFont);
    fn AFont_getFontFilePath(font: *const AFont) -> *const c_char;
    fn AFont_getWeight(font: *const AFont) -> u16;
    fn AFont_isItalic(font: *const AFont) -> bool;
    fn AFont_getLocale(font: *const AFont) -> *const c_char;
    fn AFont_getCollectionIndex(font: *const AFont) -> usize;
    fn AFont_getAxisCount(font: *const AFont) -> usize;
    fn AFont_getAxisTag(font: *const AFont, index: u32) -> u32;
    fn AFont_getAxisValue(font: *const AFont, index: u32) -> f32;
}

#[derive(Debug, PartialEq)]
struct Seen {
    path: String,
    weight: u16,
    italic: bool,
    locale: Option<String>,
    index: usize,
    axes: Vec<(u32, f32)>,
}

fn tag(t: &[u8; 4]) -> u32 {
    // OpenType: a tag is four bytes read as a big-endian uint32
    u32::from_be_bytes(*t)
}

fn string(p: *const c_char) -> Option<String> {
    // SAFETY: null or a NUL-terminated string owned by a live font
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

fn seen(font: *const AFont) -> Seen {
    // SAFETY: a live font from the iterator; axis indices below its count
    unsafe {
        let axes = (0..AFont_getAxisCount(font) as u32)
            .map(|i| (AFont_getAxisTag(font, i), AFont_getAxisValue(font, i)))
            .collect();
        Seen {
            path: string(AFont_getFontFilePath(font)).expect("AFont_getFontFilePath is _Nonnull"),
            weight: AFont_getWeight(font),
            italic: AFont_isItalic(font),
            locale: string(AFont_getLocale(font)),
            index: AFont_getCollectionIndex(font),
            axes,
        }
    }
}

#[test]
fn iterates_the_configuration_under_the_root() {
    let root = std::env::temp_dir().join(format!("atl-android-fonts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    for (from, to) in [
        ("fonts.xml", "system/etc/fonts.xml"),
        (
            "fonts_customization.xml",
            "product/etc/fonts_customization.xml",
        ),
    ] {
        let to = root.join(to);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(data.join(from), to).unwrap();
    }
    // every font file but NotoSansSymbols-Regular-Subsetted.ttf
    for f in [
        "system/fonts/Roboto-Regular.ttf",
        "system/fonts/NotoSansEthiopic-VF.ttf",
        "system/fonts/NotoSerifEthiopic-VF.ttf",
        "system/fonts/NotoSansCJK-Regular.ttc",
        "product/fonts/b3em.ttf",
    ] {
        let f = root.join(f);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, b"").unwrap();
    }
    let r = root.to_str().unwrap();
    let font =
        |path: &str, weight, italic, locale: Option<&str>, index, axes: &[(u32, f32)]| Seen {
            path: format!("{r}{path}"),
            weight,
            italic,
            locale: locale.map(str::to_owned),
            index,
            axes: axes.to_vec(),
        };
    let (ital, wdth, wght) = (tag(b"ital"), tag(b"wdth"), tag(b"wght"));
    let expected = [
        font(
            "/system/fonts/Roboto-Regular.ttf",
            100,
            false,
            None,
            0,
            &[(ital, 0.0), (wdth, 100.0), (wght, 100.0)],
        ),
        font(
            "/system/fonts/Roboto-Regular.ttf",
            700,
            true,
            None,
            0,
            &[(ital, 1.0), (wdth, 100.0), (wght, 700.0)],
        ),
        font(
            "/system/fonts/NotoSansEthiopic-VF.ttf",
            400,
            false,
            Some("und-Ethi"),
            0,
            &[(wght, 400.0)],
        ),
        font(
            "/system/fonts/NotoSerifEthiopic-VF.ttf",
            700,
            false,
            Some("und-Ethi"),
            0,
            &[(wght, 700.0)],
        ),
        font(
            "/system/fonts/NotoSansCJK-Regular.ttc",
            400,
            false,
            Some("zh-Hans"),
            2,
            &[(wght, 400.0)],
        ),
        font("/product/fonts/b3em.ttf", 400, false, None, 0, &[]),
    ];

    // SAFETY: the test's only thread reading the environment; then the API's own contract
    unsafe {
        // a trailing slash names the same root
        std::env::set_var("ATL_FONT_ROOT", format!("{r}/"));
        let it = ASystemFontIterator_open();
        assert!(!it.is_null());
        let mut got = Vec::new();
        loop {
            let f = ASystemFontIterator_next(it);
            if f.is_null() {
                break;
            }
            got.push(seen(f));
            AFont_close(f);
        }
        assert_eq!(got, expected);
        // the end stays the end
        assert!(ASystemFontIterator_next(it).is_null());
        ASystemFontIterator_close(it);
        // closing null is a no-op, as AOSP's delete
        ASystemFontIterator_close(core::ptr::null_mut());
        AFont_close(core::ptr::null_mut());

        // no configuration under the root: no fonts
        std::env::set_var("ATL_FONT_ROOT", root.join("none"));
        let it = ASystemFontIterator_open();
        assert!(ASystemFontIterator_next(it).is_null());
        ASystemFontIterator_close(it);
    }
    std::fs::remove_dir_all(&root).unwrap();
}
