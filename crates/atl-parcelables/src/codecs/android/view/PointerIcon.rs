// SPDX-License-Identifier: MIT
//! android.view.PointerIcon (frameworks/base/core/java/android/view/PointerIcon.java writeToParcel / CREATOR): the int32 type; for TYPE_CUSTOM the Bitmap (parts.rs), the hot spot floats and the drop-shadow flag follow.
//! A system type is the whole form: the receiver reloads the image from resources. A custom icon is read whole; its pixels are read past and not kept (the image cursor is the next slice), so it cannot be written back: the encode is refused and counted.

use crate::opaque::refuse;
use crate::parcel::*;
use crate::parts::*;

/// PointerIcon.TYPE_CUSTOM.
const TYPE_CUSTOM: i32 = -1;

/// A pointer icon: its PointerIcon.TYPE_* value and, for TYPE_CUSTOM, the custom payload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PointerIcon {
    pub kind: i32,
    pub custom: Option<CustomIcon>,
}

/// TYPE_CUSTOM's payload: the bitmap's size, the hot spot in bitmap pixels, the drop-shadow flag, and whether the bitmap carries a gainmap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CustomIcon {
    pub width: i32,
    pub height: i32,
    pub hot_x: f32,
    pub hot_y: f32,
    pub drop_shadow: bool,
    pub gainmap: bool,
}

impl Part for PointerIcon {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        if self.kind == TYPE_CUSTOM {
            return Err(refuse("android.view.PointerIcon.bitmap-write"));
        }
        w_i32(p, self.kind)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let kind = r_i32(p)?;
        if kind != TYPE_CUSTOM {
            return Ok(PointerIcon { kind, custom: None });
        }
        let bitmap = Bitmap::read(p)?;
        let hot_x = r_f32(p)?;
        let hot_y = r_f32(p)?;
        let drop_shadow = r_bool(p)?;
        Ok(PointerIcon {
            kind,
            custom: Some(CustomIcon {
                width: bitmap.width,
                height: bitmap.height,
                hot_x,
                hot_y,
                drop_shadow,
                gainmap: bitmap.gainmap,
            }),
        })
    }
}

crate::codec!(PointerIcon);
