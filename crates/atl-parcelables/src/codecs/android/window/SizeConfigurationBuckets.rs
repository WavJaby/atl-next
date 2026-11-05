// SPDX-License-Identifier: MIT
//! android.window.SizeConfigurationBuckets (core/java/android/window/SizeConfigurationBuckets.java, DataClass genAidl): the size thresholds an app's resources name (layout-w800dp, drawable-sw400dp, ...), reported by ActivityThread.reportSizeConfigurations (ActivityThread.java:4489-4500).
//! Parcel (writeToParcel:337-352): one flag byte as an int (0x1 horizontal, 0x2 vertical, 0x4 smallest, 0x8 screen layout size present; 0x10 = mScreenLayoutLongSet), then only the present int[]s. An app with no size-qualified resource sends the flags alone, 0.
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

/// The flag of mScreenLayoutLongSet; bits 0-3 are the arrays, in field order.
const LONG_SET: i32 = 0x10;

impl Part for SizeConfigurationBuckets {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        let arrays = [
            &self.horizontal,
            &self.vertical,
            &self.smallest,
            &self.screen_layout_size,
        ];
        let mut flags = if self.screen_layout_long_set {
            LONG_SET
        } else {
            0
        };
        for (i, array) in arrays.iter().enumerate() {
            if array.is_some() {
                flags |= 1 << i;
            }
        }
        w_i32(p, flags)?;
        for array in arrays.into_iter().filter(|a| a.is_some()) {
            w_i32_array(p, array)?;
        }
        Ok(())
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let flags = r_i32(p)?;
        let array = |bit: i32| {
            if flags & bit != 0 {
                r_i32_array(p)
            } else {
                Ok(None)
            }
        };
        Ok(SizeConfigurationBuckets {
            horizontal: array(0x1)?,
            vertical: array(0x2)?,
            smallest: array(0x4)?,
            screen_layout_size: array(0x8)?,
            screen_layout_long_set: flags & LONG_SET != 0,
        })
    }
}

crate::codec!(SizeConfigurationBuckets);
