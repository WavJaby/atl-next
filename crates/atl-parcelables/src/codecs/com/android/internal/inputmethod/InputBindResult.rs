// SPDX-License-Identifier: MIT
//! com.android.internal.inputmethod.InputBindResult (InputBindResult.java writeToParcel:283-307): result int, the IME session's strong interface, the accessibility sessions
//! (-1 = null map), the InputChannel (presence int), id (writeString = String16), sequence, isInputMethodSuppressingSpellChecker. IMMS-lite sends it through
//! IInputMethodClient.onStartInputResult. Modelled: the no-session results (InputBindResult.error: null session map, no channel); a non-null map or a channel is refused.

use binder::SpIBinder;

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InputBindResult {
    /// InputBindResult.ResultCode
    pub result: i32,
    pub method: Option<SpIBinder>,
    pub id: Option<String>,
    pub sequence: i32,
    pub is_input_method_suppressing_spell_checker: bool,
}

impl InputBindResult {
    /// ResultCode.ERROR_NO_IME (InputBindResult.java:113).
    pub const ERROR_NO_IME: i32 = 6;

    /// InputBindResult.error(result): no session, no channel, no id, sequence -1 (InputBindResult.java error()).
    pub fn error(result: i32) -> Self {
        InputBindResult {
            result,
            method: None,
            id: None,
            sequence: -1,
            is_input_method_suppressing_spell_checker: false,
        }
    }
}

impl Part for InputBindResult {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.result)?;
        w_binder(p, &self.method)?;
        // accessibilitySessions == null
        w_i32(p, -1)?;
        // channel == null
        w_present(p, false)?;
        w_str16(p, &self.id)?;
        w_i32(p, self.sequence)?;
        w_bool(p, self.is_input_method_suppressing_spell_checker)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let result = r_i32(p)?;
        let method = r_binder(p)?;
        if r_i32(p)? != -1 {
            return Err(crate::refuse("InputBindResult (accessibility sessions)"));
        }
        r_absent(p, "InputBindResult (input channel)")?;
        Ok(InputBindResult {
            result,
            method,
            id: r_str16(p)?,
            sequence: r_i32(p)?,
            is_input_method_suppressing_spell_checker: r_bool(p)?,
        })
    }
}

crate::codec!(InputBindResult);
