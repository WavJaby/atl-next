// SPDX-License-Identifier: MIT
//! android.view.inputmethod.EditorInfo (view/inputmethod/EditorInfo.java writeToParcel:1349-1384 / CREATOR:1389-1425): the focused editor's attributes, which
//! IInputMethodManager.startInputOrWindowGainedFocusAsync carries on every window focus gain (InputMethodManager.startInputInner). Every field is modelled; text fields are plain
//! CharSequences (Spanned text is refused by `r_char_sequence`). Nested forms: autofillId is writeParcelable (class name String16, -1 for null, then AutofillId.writeToParcel:300-311),
//! hintLocales LocaleList.writeToParcel (one String8 language-tag list; null is written as the empty list, so a read empty list stays ""), targetInputMethodUser UserHandle.writeToParcel
//! (the user id, USER_NULL for null). The stylus-handwriting boolean exists only when READ_ONLY flag android.view.inputmethod.editorinfo_handwriting_enabled is true.

use crate::aconfig::EDITORINFO_HANDWRITING_ENABLED;
use crate::codecs::android::os::Bundle::{Bundle, r_bundle, w_bundle};
use crate::opaque::refuse;
use crate::parcel::*;
use crate::parts::*;

/// The class name writeParcelable writes before an AutofillId.
const AUTOFILL_ID: &str = "android.view.autofill.AutofillId";
/// AutofillId.FLAG_IS_VIRTUAL_INT / FLAG_IS_VIRTUAL_LONG / FLAG_HAS_SESSION (AutofillId.java:38-40).
const VIRTUAL_INT: i32 = 0x1;
const VIRTUAL_LONG: i32 = 0x2;
const HAS_SESSION: i32 = 0x4;

/// android.view.autofill.AutofillId: view id, flags, then the session id and the virtual child id as the flags say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutofillId {
    pub view_id: i32,
    pub flags: i32,
    pub session_id: Option<i32>,
    pub virtual_int: Option<i32>,
    pub virtual_long: Option<i64>,
}

impl Part for AutofillId {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.view_id)?;
        w_i32(p, self.flags)?;
        if let Some(s) = self.session_id {
            w_i32(p, s)?;
        }
        if let Some(v) = self.virtual_int {
            w_i32(p, v)?;
        } else if let Some(v) = self.virtual_long {
            w_i64(p, v)?;
        }
        Ok(())
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let view_id = r_i32(p)?;
        let flags = r_i32(p)?;
        let session_id = if flags & HAS_SESSION != 0 {
            Some(r_i32(p)?)
        } else {
            None
        };
        let (virtual_int, virtual_long) = if flags & VIRTUAL_INT != 0 {
            (Some(r_i32(p)?), None)
        } else if flags & VIRTUAL_LONG != 0 {
            (None, Some(r_i64(p)?))
        } else {
            (None, None)
        };
        Ok(AutofillId {
            view_id,
            flags,
            session_id,
            virtual_int,
            virtual_long,
        })
    }
}

/// android.view.inputmethod.SurroundingText (SurroundingText.java writeToParcel:128-133): text, selection start / end, offset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurroundingText {
    pub text: Option<String>,
    pub selection_start: i32,
    pub selection_end: i32,
    pub offset: i32,
}

#[derive(Debug, Clone, Default)]
pub struct EditorInfo {
    pub input_type: i32,
    pub ime_options: i32,
    pub private_ime_options: Option<String>,
    pub internal_ime_options: i32,
    pub action_label: Option<String>,
    pub action_id: i32,
    pub initial_sel_start: i32,
    pub initial_sel_end: i32,
    pub initial_caps_mode: i32,
    pub initial_tool_type: i32,
    pub hint_text: Option<String>,
    pub label: Option<String>,
    pub package_name: Option<String>,
    pub autofill_id: Option<AutofillId>,
    pub field_id: i32,
    pub field_name: Option<String>,
    pub extras: Option<Bundle>,
    pub handwriting_gesture_types: i32,
    pub handwriting_gesture_preview_types: i32,
    /// present in the format only with EDITORINFO_HANDWRITING_ENABLED
    pub stylus_handwriting_enabled: bool,
    pub initial_surrounding_text: Option<SurroundingText>,
    /// LocaleList's language tags ("" = empty list = null hintLocales)
    pub hint_locales: Option<String>,
    pub content_mime_types: Option<Vec<Option<String>>>,
    /// UserHandle id; USER_NULL (-10000) for null
    pub target_input_method_user: i32,
    pub writing_tools_enabled: bool,
}

impl Part for EditorInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.input_type)?;
        w_i32(p, self.ime_options)?;
        w_str16(p, &self.private_ime_options)?;
        w_i32(p, self.internal_ime_options)?;
        w_char_sequence(p, &self.action_label)?;
        w_i32(p, self.action_id)?;
        w_i32(p, self.initial_sel_start)?;
        w_i32(p, self.initial_sel_end)?;
        w_i32(p, self.initial_caps_mode)?;
        w_i32(p, self.initial_tool_type)?;
        w_char_sequence(p, &self.hint_text)?;
        w_char_sequence(p, &self.label)?;
        w_str16(p, &self.package_name)?;
        match &self.autofill_id {
            None => w_str16(p, &None)?,
            Some(id) => {
                w_str16(p, &Some(AUTOFILL_ID.to_owned()))?;
                id.write(p)?;
            }
        }
        w_i32(p, self.field_id)?;
        w_str16(p, &self.field_name)?;
        w_bundle(p, &self.extras)?;
        w_i32(p, self.handwriting_gesture_types)?;
        w_i32(p, self.handwriting_gesture_preview_types)?;
        if EDITORINFO_HANDWRITING_ENABLED {
            w_bool(p, self.stylus_handwriting_enabled)?;
        }
        w_bool(p, self.initial_surrounding_text.is_some())?;
        if let Some(t) = &self.initial_surrounding_text {
            w_char_sequence(p, &t.text)?;
            w_i32(p, t.selection_start)?;
            w_i32(p, t.selection_end)?;
            w_i32(p, t.offset)?;
        }
        w_str8(p, &self.hint_locales)?;
        w_str16_array(p, &self.content_mime_types)?;
        w_i32(p, self.target_input_method_user)?;
        w_bool(p, self.writing_tools_enabled)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let input_type = r_i32(p)?;
        let ime_options = r_i32(p)?;
        let private_ime_options = r_str16(p)?;
        let internal_ime_options = r_i32(p)?;
        let action_label = r_char_sequence(p)?;
        let action_id = r_i32(p)?;
        let initial_sel_start = r_i32(p)?;
        let initial_sel_end = r_i32(p)?;
        let initial_caps_mode = r_i32(p)?;
        let initial_tool_type = r_i32(p)?;
        let hint_text = r_char_sequence(p)?;
        let label = r_char_sequence(p)?;
        let package_name = r_str16(p)?;
        let autofill_id = match r_str16(p)?.as_deref() {
            None => None,
            Some(AUTOFILL_ID) => Some(AutofillId::read(p)?),
            Some(_) => {
                return Err(refuse(
                    "android.view.inputmethod.EditorInfo.autofillId (subclass)",
                ));
            }
        };
        let field_id = r_i32(p)?;
        let field_name = r_str16(p)?;
        let extras = r_bundle(p)?;
        let handwriting_gesture_types = r_i32(p)?;
        let handwriting_gesture_preview_types = r_i32(p)?;
        let stylus_handwriting_enabled = EDITORINFO_HANDWRITING_ENABLED && r_bool(p)?;
        let initial_surrounding_text = if r_bool(p)? {
            Some(SurroundingText {
                text: r_char_sequence(p)?,
                selection_start: r_i32(p)?,
                selection_end: r_i32(p)?,
                offset: r_i32(p)?,
            })
        } else {
            None
        };
        Ok(EditorInfo {
            input_type,
            ime_options,
            private_ime_options,
            internal_ime_options,
            action_label,
            action_id,
            initial_sel_start,
            initial_sel_end,
            initial_caps_mode,
            initial_tool_type,
            hint_text,
            label,
            package_name,
            autofill_id,
            field_id,
            field_name,
            extras,
            handwriting_gesture_types,
            handwriting_gesture_preview_types,
            stylus_handwriting_enabled,
            initial_surrounding_text,
            hint_locales: r_str8(p)?,
            content_mime_types: r_str16_array(p)?,
            target_input_method_user: r_i32(p)?,
            writing_tools_enabled: r_bool(p)?,
        })
    }
}

crate::codec!(EditorInfo);
