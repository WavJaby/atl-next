// SPDX-License-Identifier: MIT
//! android.view.PointerIcon (frameworks/base/core/java/android/view/PointerIcon.java writeToParcel / CREATOR): the int32 type; for TYPE_CUSTOM the Bitmap, hot spot and shadow flag follow.
//! System types are the whole form: the receiver reloads the image from resources. No Bitmap codec exists, so a custom icon is refused, counted.

use crate::opaque::refuse;
use crate::parcel::*;
use crate::parts::*;

/// PointerIcon.TYPE_CUSTOM.
const TYPE_CUSTOM: i32 = -1;

/// A system pointer icon: its PointerIcon.TYPE_* value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointerIcon {
    pub kind: i32,
}

impl Part for PointerIcon {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.kind)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let kind = r_i32(p)?;
        if kind == TYPE_CUSTOM {
            return Err(refuse("android.view.PointerIcon.bitmap"));
        }
        Ok(PointerIcon { kind })
    }
}

crate::codec!(PointerIcon);
