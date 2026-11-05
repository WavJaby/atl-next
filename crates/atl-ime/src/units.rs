// SPDX-License-Identifier: MIT
//! Text index conversions between Android and the desktop toolkit; every unit change of the IME path happens here and nowhere else.
//! Android: Java `String` indices are UTF-16 code units (selection, composing span, `setComposingText` cursor); `deleteSurroundingTextInCodePoints` counts code points.
//! Toolkit (`atl-platform-ui` TextInput, GTK's units): surrounding text is UTF-8 with byte indices; preedit cursor and delete-surrounding count chars.
//! A Rust `char` is a Unicode scalar value, which is exactly what Android's code point counts for well-formed text: chars and code points need no conversion. Lone surrogates cannot reach here (a Rust `str` cannot hold them).

/// Why an index has no counterpart in the other unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitError {
    /// Past the end of the text.
    OutOfRange,
    /// Inside one character: a UTF-16 index between the two halves of a surrogate pair, or a byte index inside a UTF-8 sequence.
    InsideChar,
}

/// The UTF-8 byte index of UTF-16 index `utf16` in `text`.
pub fn utf16_to_byte(text: &str, utf16: usize) -> Result<usize, UnitError> {
    let mut units = 0;
    for (byte, c) in text.char_indices() {
        if units == utf16 {
            return Ok(byte);
        }
        units += c.len_utf16();
        if units > utf16 {
            return Err(UnitError::InsideChar);
        }
    }
    if units == utf16 {
        Ok(text.len())
    } else {
        Err(UnitError::OutOfRange)
    }
}

/// The UTF-16 index of UTF-8 byte index `byte` in `text`.
pub fn byte_to_utf16(text: &str, byte: usize) -> Result<usize, UnitError> {
    let head = text.get(..byte).ok_or(if byte > text.len() {
        UnitError::OutOfRange
    } else {
        UnitError::InsideChar
    })?;
    Ok(head.encode_utf16().count())
}

/// The UTF-16 index of the position `chars` chars into `text` (a preedit cursor in Java string units).
pub fn chars_to_utf16(text: &str, chars: usize) -> Result<usize, UnitError> {
    let mut units = 0;
    let mut it = text.chars();
    for _ in 0..chars {
        units += it.next().ok_or(UnitError::OutOfRange)?.len_utf16();
    }
    Ok(units)
}

/// A toolkit delete-surrounding (`offset_chars` from the cursor, negative = before it, `n_chars` long) as Android's `deleteSurroundingTextInCodePoints(before, after)`.
/// `None` when the range does not touch the cursor: Android deletes only a span around the cursor, so a range with a gap to it has no equivalent.
pub fn delete_to_code_points(offset_chars: i32, n_chars: u32) -> Option<(u32, u32)> {
    let start = i64::from(offset_chars);
    let end = start + i64::from(n_chars);
    if start > 0 || end < 0 {
        return None;
    }
    // start <= 0 <= end, both within i32 / u32 + i32 range
    Some((u32::try_from(-start).ok()?, u32::try_from(end).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values are counted from the encodings, never computed by the code under test:
    // "a😀b": a = U+0061 (UTF-8 1 byte, UTF-16 1 unit); 😀 = U+1F600 (UTF-8 F0 9F 98 80 = 4 bytes, UTF-16 D83D DE00 = 2 units); b = 1 byte, 1 unit. Total 6 bytes, 4 units.
    const EMOJI: &str = "a😀b";
    // "xかy": か = U+304B (UTF-8 E3 81 8B = 3 bytes, UTF-16 1 unit). Total 5 bytes, 3 units.
    const KANA: &str = "xかy";

    #[test]
    fn utf16_to_byte_by_hand() {
        assert_eq!(utf16_to_byte(EMOJI, 0), Ok(0));
        assert_eq!(utf16_to_byte(EMOJI, 1), Ok(1));
        assert_eq!(
            utf16_to_byte(EMOJI, 2),
            Err(UnitError::InsideChar),
            "between D83D and DE00"
        );
        assert_eq!(utf16_to_byte(EMOJI, 3), Ok(5));
        assert_eq!(utf16_to_byte(EMOJI, 4), Ok(6));
        assert_eq!(utf16_to_byte(EMOJI, 5), Err(UnitError::OutOfRange));
        assert_eq!(utf16_to_byte(KANA, 2), Ok(4));
        assert_eq!(utf16_to_byte("", 0), Ok(0));
    }

    #[test]
    fn byte_to_utf16_by_hand() {
        assert_eq!(byte_to_utf16(EMOJI, 0), Ok(0));
        assert_eq!(byte_to_utf16(EMOJI, 1), Ok(1));
        for inside in 2..=4 {
            assert_eq!(byte_to_utf16(EMOJI, inside), Err(UnitError::InsideChar));
        }
        assert_eq!(byte_to_utf16(EMOJI, 5), Ok(3));
        assert_eq!(byte_to_utf16(EMOJI, 6), Ok(4));
        assert_eq!(byte_to_utf16(EMOJI, 7), Err(UnitError::OutOfRange));
        assert_eq!(byte_to_utf16(KANA, 4), Ok(2));
    }

    #[test]
    fn chars_to_utf16_by_hand() {
        assert_eq!(chars_to_utf16(EMOJI, 0), Ok(0));
        assert_eq!(chars_to_utf16(EMOJI, 1), Ok(1));
        assert_eq!(chars_to_utf16(EMOJI, 2), Ok(3));
        assert_eq!(chars_to_utf16(EMOJI, 3), Ok(4));
        assert_eq!(chars_to_utf16(EMOJI, 4), Err(UnitError::OutOfRange));
        // "かな": 2 chars, 1 unit each
        assert_eq!(chars_to_utf16("かな", 2), Ok(2));
    }

    #[test]
    fn delete_around_the_cursor() {
        // GTK (-1, 1): the one char before the cursor
        assert_eq!(delete_to_code_points(-1, 1), Some((1, 0)));
        // (-2, 3): two before, one after
        assert_eq!(delete_to_code_points(-2, 3), Some((2, 1)));
        assert_eq!(delete_to_code_points(0, 2), Some((0, 2)));
        assert_eq!(delete_to_code_points(0, 0), Some((0, 0)));
        // a gap between cursor and range: no Android equivalent
        assert_eq!(delete_to_code_points(1, 1), None);
        assert_eq!(delete_to_code_points(-3, 1), None);
    }
}
