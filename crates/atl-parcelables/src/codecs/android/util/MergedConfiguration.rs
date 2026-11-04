// SPDX-License-Identifier: MIT
//! android.util.MergedConfiguration (frameworks/base/core/java/android/util/MergedConfiguration.java writeToParcel:61-65, readFromParcel:67-71): the global, override and merged
//! Configurations a relayout reports to the client. Three Configuration.writeToParcel bodies in that order, no presence flags (the Java object always holds all three).

use crate::codecs::android::content::res::Configuration::Configuration;
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MergedConfiguration {
    pub global: Configuration,
    pub override_config: Configuration,
    pub merged: Configuration,
}

impl Part for MergedConfiguration {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.global.write(p)?;
        self.override_config.write(p)?;
        self.merged.write(p)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(MergedConfiguration {
            global: Configuration::read(p)?,
            override_config: Configuration::read(p)?,
            merged: Configuration::read(p)?,
        })
    }
}

crate::codec!(MergedConfiguration);
