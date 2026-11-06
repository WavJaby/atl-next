// SPDX-License-Identifier: MIT
//! A field's EditorInfo.inputType as the toolkit's content purpose and hints (IME study §5). Constants: core/java/android/text/InputType.java at the pin.
//! No toolkit purpose for dates and times (GTK has none): they stay free-form. Multi-line has no GTK hint either.

use atl_platform_ui::{ContentHints, ContentPurpose};

const TYPE_MASK_CLASS: i32 = 0x0000_000f;
const TYPE_MASK_VARIATION: i32 = 0x0000_0ff0;
const TYPE_CLASS_TEXT: i32 = 0x1;
const TYPE_CLASS_NUMBER: i32 = 0x2;
const TYPE_CLASS_PHONE: i32 = 0x3;
const TYPE_TEXT_FLAG_CAP_CHARACTERS: i32 = 0x0000_1000;
const TYPE_TEXT_FLAG_CAP_WORDS: i32 = 0x0000_2000;
const TYPE_TEXT_FLAG_CAP_SENTENCES: i32 = 0x0000_4000;
const TYPE_TEXT_FLAG_AUTO_CORRECT: i32 = 0x0000_8000;
const TYPE_TEXT_FLAG_NO_SUGGESTIONS: i32 = 0x0008_0000;
const TYPE_TEXT_VARIATION_URI: i32 = 0x10;
const TYPE_TEXT_VARIATION_EMAIL_ADDRESS: i32 = 0x20;
const TYPE_TEXT_VARIATION_PERSON_NAME: i32 = 0x60;
const TYPE_TEXT_VARIATION_PASSWORD: i32 = 0x80;
const TYPE_TEXT_VARIATION_VISIBLE_PASSWORD: i32 = 0x90;
const TYPE_TEXT_VARIATION_WEB_EMAIL_ADDRESS: i32 = 0xd0;
const TYPE_TEXT_VARIATION_WEB_PASSWORD: i32 = 0xe0;
const TYPE_NUMBER_FLAG_SIGNED: i32 = 0x0000_1000;
const TYPE_NUMBER_FLAG_DECIMAL: i32 = 0x0000_2000;
const TYPE_NUMBER_VARIATION_PASSWORD: i32 = 0x10;

/// The purpose and hints for `input_type`.
pub fn content(input_type: i32) -> (ContentPurpose, ContentHints) {
    let variation = input_type & TYPE_MASK_VARIATION;
    let flag = |f: i32| input_type & f != 0;
    match input_type & TYPE_MASK_CLASS {
        TYPE_CLASS_NUMBER if variation == TYPE_NUMBER_VARIATION_PASSWORD => {
            (ContentPurpose::Pin, ContentHints::PRIVATE)
        }
        TYPE_CLASS_NUMBER if flag(TYPE_NUMBER_FLAG_SIGNED | TYPE_NUMBER_FLAG_DECIMAL) => {
            (ContentPurpose::Number, ContentHints::NONE)
        }
        TYPE_CLASS_NUMBER => (ContentPurpose::Digits, ContentHints::NONE),
        TYPE_CLASS_PHONE => (ContentPurpose::Phone, ContentHints::NONE),
        TYPE_CLASS_TEXT => {
            let purpose = match variation {
                TYPE_TEXT_VARIATION_URI => ContentPurpose::Url,
                TYPE_TEXT_VARIATION_EMAIL_ADDRESS | TYPE_TEXT_VARIATION_WEB_EMAIL_ADDRESS => {
                    ContentPurpose::Email
                }
                TYPE_TEXT_VARIATION_PERSON_NAME => ContentPurpose::Name,
                TYPE_TEXT_VARIATION_PASSWORD
                | TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                | TYPE_TEXT_VARIATION_WEB_PASSWORD => ContentPurpose::Password,
                _ => ContentPurpose::FreeForm,
            };
            let mut hints = ContentHints::NONE;
            if purpose == ContentPurpose::Password {
                hints = hints | ContentHints::PRIVATE;
            }
            if flag(TYPE_TEXT_FLAG_CAP_CHARACTERS) {
                hints = hints | ContentHints::UPPERCASE_CHARS;
            } else if flag(TYPE_TEXT_FLAG_CAP_WORDS) {
                hints = hints | ContentHints::UPPERCASE_WORDS;
            } else if flag(TYPE_TEXT_FLAG_CAP_SENTENCES) {
                hints = hints | ContentHints::UPPERCASE_SENTENCES;
            }
            if flag(TYPE_TEXT_FLAG_NO_SUGGESTIONS) {
                hints = hints | ContentHints::NO_SPELLCHECK;
            } else if flag(TYPE_TEXT_FLAG_AUTO_CORRECT) {
                hints = hints | ContentHints::SPELLCHECK;
            }
            (purpose, hints)
        }
        _ => (ContentPurpose::FreeForm, ContentHints::NONE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values are the AOSP constants composed by hand (InputType.java), e.g. textEmailAddress = TYPE_CLASS_TEXT | TYPE_TEXT_VARIATION_EMAIL_ADDRESS = 0x21.
    #[test]
    fn input_types() {
        assert_eq!(content(0x21), (ContentPurpose::Email, ContentHints::NONE));
        // textPassword = 0x81
        assert_eq!(
            content(0x81),
            (ContentPurpose::Password, ContentHints::PRIVATE)
        );
        // numberPassword = 0x12
        assert_eq!(content(0x12), (ContentPurpose::Pin, ContentHints::PRIVATE));
        // numberSigned = 0x1002
        assert_eq!(
            content(0x1002),
            (ContentPurpose::Number, ContentHints::NONE)
        );
        assert_eq!(content(0x2), (ContentPurpose::Digits, ContentHints::NONE));
        // textCapSentences | textAutoCorrect = 0x1 | 0x4000 | 0x8000
        assert_eq!(
            content(0xc001),
            (
                ContentPurpose::FreeForm,
                ContentHints::UPPERCASE_SENTENCES | ContentHints::SPELLCHECK
            )
        );
        // datetime = 0x4: no toolkit purpose
        assert_eq!(content(0x4), (ContentPurpose::FreeForm, ContentHints::NONE));
    }
}
