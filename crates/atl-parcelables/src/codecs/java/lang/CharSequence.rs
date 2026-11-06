// SPDX-License-Identifier: MIT
//! java.lang.CharSequence as AIDL passes it (TextUtils.writeToParcel / CHAR_SEQUENCE_CREATOR, android/text/TextUtils.java:805-845): plain text is kind 1 + String8; Spanned text is kind 0 + String8 +
//! its spans + a 0 terminator. Modelled: plain text, and Spanned text without spans; a span is refused (`r_char_sequence`). The in-app IME sends composing and committed text this way
//! (IRemoteInputConnection setComposingText / commitText).

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CharSequence {
    pub text: Option<String>,
}

impl CharSequence {
    pub fn new(text: impl Into<String>) -> Self {
        CharSequence {
            text: Some(text.into()),
        }
    }
}

impl Part for CharSequence {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_char_sequence(p, &self.text)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(CharSequence {
            text: r_char_sequence(p)?,
        })
    }
}

crate::codec!(CharSequence);
