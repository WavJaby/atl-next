// SPDX-License-Identifier: MIT
//! android.view.InsetsSourceControl.Array (frameworks/base/core/java/android/view/InsetsSourceControl.java writeToParcel:377-380, readFromParcel:371-375): the controls of a window's
//! insets (a writeTypedArray) then their sequence number. The controls are not modelled: null (-1) or empty (0) only; a non-empty array is refused. The count is kept as
//! written so the bytes come back as they were.

use crate::parcel::*;
use crate::parts::*;

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InsetsSourceControl_Array {
    /// The writeTypedArray count of mControls as written: -1 null, 0 empty.
    pub controls_count: i32,
    pub seq: i32,
}

impl Part for InsetsSourceControl_Array {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.controls_count)?;
        w_i32(p, self.seq)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(InsetsSourceControl_Array {
            controls_count: r_absent_array(p, "android.view.InsetsSourceControl[]")?,
            seq: r_i32(p)?,
        })
    }
}

crate::codec!(InsetsSourceControl_Array);
