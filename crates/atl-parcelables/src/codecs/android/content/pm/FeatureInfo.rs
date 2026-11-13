// SPDX-License-Identifier: MIT
//! android.content.pm.FeatureInfo (frameworks/base/core/java/android/content/pm/FeatureInfo.java writeToParcel / FeatureInfo(Parcel)): writeString8 name, then version, reqGlEsVersion, flags.
//! IPackageManager.getSystemAvailableFeatures returns a ParceledListSlice of them: one per SystemConfig feature (name set), plus one with only reqGlEsVersion (name null).

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeatureInfo {
    /// None: the entry describes the GL ES version (`reqGlEsVersion`) instead of a named feature
    pub name: Option<String>,
    pub version: i32,
    /// `ro.opengles.version`'s encoding: major << 16 | minor; 0 = GL_ES_VERSION_UNDEFINED
    pub req_gl_es_version: i32,
    pub flags: i32,
}

impl Part for FeatureInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str8(p, &self.name)?;
        w_i32(p, self.version)?;
        w_i32(p, self.req_gl_es_version)?;
        w_i32(p, self.flags)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(FeatureInfo {
            name: r_str8(p)?,
            version: r_i32(p)?,
            req_gl_es_version: r_i32(p)?,
            flags: r_i32(p)?,
        })
    }
}

crate::codec!(FeatureInfo);
