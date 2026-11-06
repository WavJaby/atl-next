// SPDX-License-Identifier: MIT
//! android.view.inputmethod.SurroundingText (view/inputmethod/SurroundingText.java writeToParcel:128-133): text (TextUtils.writeToParcel), selection start / end, offset.
//! Indices are UTF-16 code units into `text`; `offset` is where `text` starts in the editor (-1 unknown). Carried inline by EditorInfo.initialSurroundingText and as the
//! IRemoteInputConnection.getSurroundingText result (AndroidFuture.rs).

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurroundingText {
    pub text: Option<String>,
    pub selection_start: i32,
    pub selection_end: i32,
    pub offset: i32,
}

impl Part for SurroundingText {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_char_sequence(p, &self.text)?;
        w_i32(p, self.selection_start)?;
        w_i32(p, self.selection_end)?;
        w_i32(p, self.offset)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(SurroundingText {
            text: r_char_sequence(p)?,
            selection_start: r_i32(p)?,
            selection_end: r_i32(p)?,
            offset: r_i32(p)?,
        })
    }
}

crate::codec!(SurroundingText);
