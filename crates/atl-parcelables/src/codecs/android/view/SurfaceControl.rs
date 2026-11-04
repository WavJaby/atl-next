// SPDX-License-Identifier: MIT
//! android.view.SurfaceControl (frameworks/base/core/java/android/view/SurfaceControl.java writeToParcel:1560-1573, readFromParcel:1538-1552): the name (String8), width, height, then a presence int.
//! Java reads any non-zero presence as present and then calls libgui's SurfaceControl::readFromParcel, whose writer is SurfaceControl::writeToParcel (frameworks/native/libs/gui/SurfaceControl.cpp:238-249):
//! client binder, handle binder, layer id, layer name (String16), transform hint, width, height, format. Absent: the SurfaceControl has no native layer. Present: the window path's layer (M3 slice 5).

use binder::SpIBinder;

use crate::parcel::*;
use crate::parts::*;

/// libgui's SurfaceControl state after the Java fields, in libgui's parcel order. The binders are objects of the parcel: a round trip carries them as Java's does.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NativeSurfaceControl {
    /// The `ISurfaceComposerClient` the layer belongs to (`mClient->getClient()`).
    pub client: Option<SpIBinder>,
    /// The layer's handle (`mHandle`): the binder that layer state in transactions refers to.
    pub handle: Option<SpIBinder>,
    pub layer_id: i32,
    /// libgui's mName, UTF-8 in libgui, written as String16.
    pub name: Option<String>,
    pub transform_hint: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SurfaceControl {
    /// mName (String8): null for the empty SurfaceControl.
    pub name: Option<String>,
    pub width: i32,
    pub height: i32,
    /// mNativeObject: None when there is no native layer (a SurfaceControl the app has not created).
    pub native: Option<NativeSurfaceControl>,
}

impl Part for SurfaceControl {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.name)?;
        w_i32(p, self.width)?;
        w_i32(p, self.height)?;
        let Some(n) = &self.native else {
            return w_i32(p, 0);
        };
        w_i32(p, 1)?;
        w_binder(p, &n.client)?;
        w_binder(p, &n.handle)?;
        w_i32(p, n.layer_id)?;
        w_str16(p, &n.name)?;
        w_i32(p, n.transform_hint as i32)?;
        w_i32(p, n.width as i32)?;
        w_i32(p, n.height as i32)?;
        w_i32(p, n.format as i32)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let name = r_str8(p)?;
        let width = r_i32(p)?;
        let height = r_i32(p)?;
        if r_i32(p)? == 0 {
            return Ok(SurfaceControl {
                name,
                width,
                height,
                native: None,
            });
        }
        let client = r_binder(p)?;
        let handle = r_binder(p)?;
        let layer_id = r_i32(p)?;
        let native_name = r_str16(p)?;
        let transform_hint = r_i32(p)? as u32;
        let native_width = r_i32(p)? as u32;
        let native_height = r_i32(p)? as u32;
        let format = r_i32(p)? as u32;
        Ok(SurfaceControl {
            name,
            width,
            height,
            native: Some(NativeSurfaceControl {
                client,
                handle,
                layer_id,
                name: native_name,
                transform_hint,
                width: native_width,
                height: native_height,
                format,
            }),
        })
    }
}

crate::codec!(SurfaceControl);
