// SPDX-License-Identifier: MIT
//! The C ABI: thin shells over `crate::fonts`. `generated::*` checks every signature against the NDK's headers.
//! An `AFont*` / `ASystemFontIterator*` is a `Box` of `fonts::Font` / `fonts::SystemFontIterator`, made here and freed by its `_close`.
//! AOSP aborts (`LOG_ALWAYS_FATAL_IF`) on a null font or iterator and on an axis index out of range; so do these, without its log line.

use core::ffi::c_char;

use crate::fonts::{Font, SystemFontIterator};
use crate::generated::font::AFont;
use crate::generated::system_fonts::{AFont as IteratorFont, ASystemFontIterator};

/// The font behind `font`; aborts on null.
///
/// SAFETY (caller): `font` is null or an `AFont_*`-owned pointer this module made, not yet closed.
unsafe fn font<'a>(font: *const AFont) -> &'a Font {
    // SAFETY: the caller's contract: null or a live `Box<Font>`
    unsafe { font.cast::<Font>().as_ref() }.unwrap_or_else(|| std::process::abort())
}

/// The axis at `index`; aborts out of range.
fn axis(font: &Font, index: u32) -> (u32, f32) {
    usize::try_from(index)
        .ok()
        .and_then(|i| font.axes.get(i).copied())
        .unwrap_or_else(|| std::process::abort())
}

#[unsafe(no_mangle)]
pub extern "C" fn ASystemFontIterator_open() -> *mut ASystemFontIterator {
    Box::into_raw(Box::new(SystemFontIterator::open())).cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ASystemFontIterator_close(iterator: *mut ASystemFontIterator) {
    if !iterator.is_null() {
        // SAFETY: the caller's contract: an iterator from `ASystemFontIterator_open`, closed once
        drop(unsafe { Box::from_raw(iterator.cast::<SystemFontIterator>()) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ASystemFontIterator_next(
    iterator: *mut ASystemFontIterator,
) -> *mut IteratorFont {
    // SAFETY: the caller's contract: null or a live iterator from `ASystemFontIterator_open`, not shared
    let Some(iterator) = (unsafe { iterator.cast::<SystemFontIterator>().as_mut() }) else {
        std::process::abort()
    };
    iterator
        .next_font()
        .map_or(core::ptr::null_mut(), |f| Box::into_raw(Box::new(f)).cast())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_close(font: *mut AFont) {
    if !font.is_null() {
        // SAFETY: the caller's contract: a font from `ASystemFontIterator_next`, closed once
        drop(unsafe { Box::from_raw(font.cast::<Font>()) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_getFontFilePath(f: *const AFont) -> *const c_char {
    // SAFETY: the caller's contract (`font`); the string lives as long as the font
    unsafe { font(f) }.path.as_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_getWeight(f: *const AFont) -> u16 {
    // SAFETY: the caller's contract (`font`)
    unsafe { font(f) }.weight
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_isItalic(f: *const AFont) -> bool {
    // SAFETY: the caller's contract (`font`)
    unsafe { font(f) }.italic
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_getLocale(f: *const AFont) -> *const c_char {
    // SAFETY: the caller's contract (`font`); the string lives as long as the font
    unsafe { font(f) }
        .locale
        .as_ref()
        .map_or(core::ptr::null(), |l| l.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_getCollectionIndex(f: *const AFont) -> usize {
    // SAFETY: the caller's contract (`font`)
    let index = unsafe { font(f) }.collection_index;
    usize::try_from(index).expect("size_t holds a uint32_t on every arch in arches.toml")
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_getAxisCount(f: *const AFont) -> usize {
    // SAFETY: the caller's contract (`font`)
    unsafe { font(f) }.axes.len()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_getAxisTag(f: *const AFont, axis_index: u32) -> u32 {
    // SAFETY: the caller's contract (`font`)
    axis(unsafe { font(f) }, axis_index).0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn AFont_getAxisValue(f: *const AFont, axis_index: u32) -> f32 {
    // SAFETY: the caller's contract (`font`)
    axis(unsafe { font(f) }, axis_index).1
}
