// SPDX-License-Identifier: MIT
//! android.window.SizeConfigurationBuckets (core/java/android/window/SizeConfigurationBuckets.java, DataClass genAidl): the size thresholds an app's resources name (layout-w800dp, drawable-sw400dp, ...), reported by ActivityThread.reportSizeConfigurations (ActivityThread.java:4489-4500).
//! Parcel order = declaration order: mHorizontal, mVertical, mSmallest, mScreenLayoutSize (int[], null when no resource names that size; the thresholds of an app with no size-qualified resource are all null), mScreenLayoutLongSet (boolean).
//! The filter that reads them (filterDiff) is size_buckets.rs in atl-services: this crate builds only under the converter (`#![cfg(soong)]`), so its logic would have no test run.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SizeConfigurationBuckets {
    pub horizontal: Option<Vec<i32>>,
    pub vertical: Option<Vec<i32>>,
    pub smallest: Option<Vec<i32>>,
    pub screen_layout_size: Option<Vec<i32>>,
    pub screen_layout_long_set: bool,
}

impl Part for SizeConfigurationBuckets {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32_array(p, &self.horizontal)?;
        w_i32_array(p, &self.vertical)?;
        w_i32_array(p, &self.smallest)?;
        w_i32_array(p, &self.screen_layout_size)?;
        w_bool(p, self.screen_layout_long_set)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(SizeConfigurationBuckets {
            horizontal: r_i32_array(p)?,
            vertical: r_i32_array(p)?,
            smallest: r_i32_array(p)?,
            screen_layout_size: r_i32_array(p)?,
            screen_layout_long_set: r_bool(p)?,
        })
    }
}

crate::codec!(SizeConfigurationBuckets);
