// SPDX-License-Identifier: MIT
//! android.content.pm.ConfigurationInfo (frameworks/base/core/java/android/content/pm/ConfigurationInfo.java writeToParcel / ConfigurationInfo(Parcel)): five int32s, touchscreen,
//! keyboard type, navigation, input features, GL ES version. IActivityTaskManager.getDeviceConfigurationInfo returns it.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfigurationInfo {
    pub req_touch_screen: i32,
    pub req_keyboard_type: i32,
    pub req_navigation: i32,
    pub req_input_features: i32,
    /// `ro.opengles.version`'s encoding: major << 16 | minor; 0 = GL_ES_VERSION_UNDEFINED
    pub req_gl_es_version: i32,
}

impl Part for ConfigurationInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.req_touch_screen)?;
        w_i32(p, self.req_keyboard_type)?;
        w_i32(p, self.req_navigation)?;
        w_i32(p, self.req_input_features)?;
        w_i32(p, self.req_gl_es_version)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ConfigurationInfo {
            req_touch_screen: r_i32(p)?,
            req_keyboard_type: r_i32(p)?,
            req_navigation: r_i32(p)?,
            req_input_features: r_i32(p)?,
            req_gl_es_version: r_i32(p)?,
        })
    }
}

crate::codec!(ConfigurationInfo);
