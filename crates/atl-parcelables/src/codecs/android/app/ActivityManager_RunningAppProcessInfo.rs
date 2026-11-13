// SPDX-License-Identifier: MIT
//! android.app.ActivityManager.RunningAppProcessInfo (ActivityManager.java, writeToParcel / readFromParcel): IActivityManager.getRunningAppProcesses' list element, read by SDK
//! "am I the main process" checks (pid / processName against the caller's own).

use crate::parcel::*;
use crate::parts::*;

/// Java's own field order (writeToParcel): processName, pid, uid, pkgList, pkgDeps, flags, lastTrimLevel, importance, lru, importanceReasonCode, importanceReasonPid,
/// importanceReasonComponent (ComponentName.writeToParcel: null = null package), importanceReasonImportance, processState, isFocused (int), lastActivityTime (long).
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityManager_RunningAppProcessInfo {
    pub process_name: Option<String>,
    pub pid: i32,
    pub uid: i32,
    pub pkg_list: Option<Vec<Option<String>>>,
    pub pkg_deps: Option<Vec<Option<String>>>,
    pub flags: i32,
    pub last_trim_level: i32,
    pub importance: i32,
    pub lru: i32,
    pub importance_reason_code: i32,
    pub importance_reason_pid: i32,
    pub importance_reason_component: Option<ComponentName>,
    pub importance_reason_importance: i32,
    pub process_state: i32,
    pub is_focused: bool,
    pub last_activity_time: i64,
}

impl Part for ActivityManager_RunningAppProcessInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &self.process_name)?;
        w_i32(p, self.pid)?;
        w_i32(p, self.uid)?;
        w_str16_array(p, &self.pkg_list)?;
        w_str16_array(p, &self.pkg_deps)?;
        w_i32(p, self.flags)?;
        w_i32(p, self.last_trim_level)?;
        w_i32(p, self.importance)?;
        w_i32(p, self.lru)?;
        w_i32(p, self.importance_reason_code)?;
        w_i32(p, self.importance_reason_pid)?;
        w_component(p, &self.importance_reason_component)?;
        w_i32(p, self.importance_reason_importance)?;
        w_i32(p, self.process_state)?;
        w_i32(p, i32::from(self.is_focused))?;
        w_i64(p, self.last_activity_time)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(Self {
            process_name: r_str16(p)?,
            pid: r_i32(p)?,
            uid: r_i32(p)?,
            pkg_list: r_str16_array(p)?,
            pkg_deps: r_str16_array(p)?,
            flags: r_i32(p)?,
            last_trim_level: r_i32(p)?,
            importance: r_i32(p)?,
            lru: r_i32(p)?,
            importance_reason_code: r_i32(p)?,
            importance_reason_pid: r_i32(p)?,
            importance_reason_component: r_component(p)?,
            importance_reason_importance: r_i32(p)?,
            process_state: r_i32(p)?,
            is_focused: r_i32(p)? != 0,
            last_activity_time: r_i64(p)?,
        })
    }
}

crate::codec!(ActivityManager_RunningAppProcessInfo);
