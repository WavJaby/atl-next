// SPDX-License-Identifier: MIT
//! android.content.res.CompatibilityInfo: content/res/CompatibilityInfo.java writeToParcel / CompatibilityInfo(Parcel).

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompatibilityInfo {
    pub compatibility_flags: i32,
    pub application_density: i32,
    pub application_scale: f32,
    pub application_inverted_scale: f32,
    pub application_density_scale: f32,
    pub application_density_inverted_scale: f32,
    pub application_display_rotation: i32,
}

impl Part for CompatibilityInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.compatibility_flags)?;
        w_i32(p, self.application_density)?;
        w_f32(p, self.application_scale)?;
        w_f32(p, self.application_inverted_scale)?;
        w_f32(p, self.application_density_scale)?;
        w_f32(p, self.application_density_inverted_scale)?;
        w_i32(p, self.application_display_rotation)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(CompatibilityInfo {
            compatibility_flags: r_i32(p)?,
            application_density: r_i32(p)?,
            application_scale: r_f32(p)?,
            application_inverted_scale: r_f32(p)?,
            application_density_scale: r_f32(p)?,
            application_density_inverted_scale: r_f32(p)?,
            application_display_rotation: r_i32(p)?,
        })
    }
}

crate::codec!(CompatibilityInfo);
