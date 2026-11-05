// SPDX-License-Identifier: MIT
//! android.view.inputmethod.ImeTracker.Token (ImeTracker.java writeToParcel:838-841): a strong binder and a tag (writeString8). IInputMethodManager.showSoftInput / hideSoftInput carry one.

use binder::SpIBinder;

use crate::parcel::*;
use crate::parts::*;

/// The Java nested class name (generated parcelables.rs names it so).
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImeTracker_Token {
    pub binder: Option<SpIBinder>,
    pub tag: Option<String>,
}

impl Part for ImeTracker_Token {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &self.binder)?;
        w_str8(p, &self.tag)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ImeTracker_Token {
            binder: r_binder(p)?,
            tag: r_str8(p)?,
        })
    }
}

crate::codec!(ImeTracker_Token);
