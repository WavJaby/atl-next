// SPDX-License-Identifier: MIT
//! android.view.SurfaceControl (frameworks/base/core/java/android/view/SurfaceControl.java writeToParcel:1560-, readFromParcel:1538-1551): the name (String8), width, height,
//! then a presence int and, when it is non-zero, the native handle (libgui's SurfaceControl::writeToParcel, not modelled here). Only the absent form is modelled: a SurfaceControl with
//! no native object, which is what the window path sends until the composer serves layers (M3 slice 4). A present one is refused.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SurfaceControl {
    /// mName (String8): null for the empty SurfaceControl.
    pub name: Option<String>,
    pub width: i32,
    pub height: i32,
}

impl Part for SurfaceControl {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.name)?;
        w_i32(p, self.width)?;
        w_i32(p, self.height)?;
        // mNativeObject == 0: no native handle follows
        w_i32(p, 0)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let name = r_str8(p)?;
        let width = r_i32(p)?;
        let height = r_i32(p)?;
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.view.SurfaceControl (native handle)"));
        }
        Ok(SurfaceControl {
            name,
            width,
            height,
        })
    }
}

crate::codec!(SurfaceControl);
