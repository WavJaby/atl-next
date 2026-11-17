// SPDX-License-Identifier: MIT
//! android.app.ActivityManager.MemoryInfo (ActivityManager.java, writeToParcel / readFromParcel): IActivityManager.getMemoryInfo's out parameter, read by games' quality tiers and low-memory checks.

use crate::parcel::*;

/// Java's own field order (writeToParcel): advertisedMem, availMem, totalMem, threshold (longs), lowMemory (int), hiddenAppThreshold, secondaryServerThreshold, visibleAppThreshold,
/// foregroundAppThreshold (longs). Every size is in bytes.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityManager_MemoryInfo {
    pub advertised_mem: i64,
    pub avail_mem: i64,
    pub total_mem: i64,
    pub threshold: i64,
    pub low_memory: bool,
    pub hidden_app_threshold: i64,
    pub secondary_server_threshold: i64,
    pub visible_app_threshold: i64,
    pub foreground_app_threshold: i64,
}

impl crate::parts::Part for ActivityManager_MemoryInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i64(p, self.advertised_mem)?;
        w_i64(p, self.avail_mem)?;
        w_i64(p, self.total_mem)?;
        w_i64(p, self.threshold)?;
        w_i32(p, i32::from(self.low_memory))?;
        w_i64(p, self.hidden_app_threshold)?;
        w_i64(p, self.secondary_server_threshold)?;
        w_i64(p, self.visible_app_threshold)?;
        w_i64(p, self.foreground_app_threshold)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(Self {
            advertised_mem: r_i64(p)?,
            avail_mem: r_i64(p)?,
            total_mem: r_i64(p)?,
            threshold: r_i64(p)?,
            low_memory: r_i32(p)? != 0,
            hidden_app_threshold: r_i64(p)?,
            secondary_server_threshold: r_i64(p)?,
            visible_app_threshold: r_i64(p)?,
            foreground_app_threshold: r_i64(p)?,
        })
    }
}

crate::codec!(ActivityManager_MemoryInfo);
