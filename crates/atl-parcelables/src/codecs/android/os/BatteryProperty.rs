// SPDX-License-Identifier: MIT
//! android.os.BatteryProperty (core/java/android/os/BatteryProperty.java writeToParcel:78-81, readFromParcel:73-76): a long then a String8.
//! The `out` value of IBatteryPropertiesRegistrar.getProperty (battery.rs): the service fills one value, BatteryManager reads getLong / getString.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatteryProperty {
    pub value_long: i64,
    pub value_string: Option<String>,
}

/// BatteryProperty() (:36-39): Long.MIN_VALUE, null -- what a property the service leaves untouched reads as.
impl Default for BatteryProperty {
    fn default() -> Self {
        BatteryProperty {
            value_long: i64::MIN,
            value_string: None,
        }
    }
}

impl Part for BatteryProperty {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i64(p, self.value_long)?;
        w_str8(p, &self.value_string)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(BatteryProperty {
            value_long: r_i64(p)?,
            value_string: r_str8(p)?,
        })
    }
}

crate::codec!(BatteryProperty);
