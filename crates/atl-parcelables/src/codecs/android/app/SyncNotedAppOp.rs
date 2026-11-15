// SPDX-License-Identifier: MIT
//! android.app.SyncNotedAppOp (SyncNotedAppOp.java writeToParcel:208-220, DataClass-generated): IAppOpsService.noteOperation's answer, whose op mode AppOpsManager.noteOpNoThrow returns.
//! Layout: a flag byte (written as an int: 0x4 = attribution tag present, 0x8 = package present), op mode, op code, then the present strings in that order.

use crate::parcel::*;
use crate::parts::*;

/// SyncNotedAppOp's DataClass flag bits (writeToParcel).
const HAS_ATTRIBUTION_TAG: i32 = 0x4;
const HAS_PACKAGE_NAME: i32 = 0x8;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncNotedAppOp {
    pub op_mode: i32,
    pub op_code: i32,
    pub attribution_tag: Option<String>,
    pub package_name: Option<String>,
}

impl Part for SyncNotedAppOp {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        let mut flg = 0;
        if self.attribution_tag.is_some() {
            flg |= HAS_ATTRIBUTION_TAG;
        }
        if self.package_name.is_some() {
            flg |= HAS_PACKAGE_NAME;
        }
        w_i32(p, flg)?;
        w_i32(p, self.op_mode)?;
        w_i32(p, self.op_code)?;
        if self.attribution_tag.is_some() {
            w_str16(p, &self.attribution_tag)?;
        }
        if self.package_name.is_some() {
            w_str16(p, &self.package_name)?;
        }
        Ok(())
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let flg = r_i32(p)?;
        let op_mode = r_i32(p)?;
        let op_code = r_i32(p)?;
        let attribution_tag = if flg & HAS_ATTRIBUTION_TAG != 0 {
            r_str16(p)?
        } else {
            None
        };
        let package_name = if flg & HAS_PACKAGE_NAME != 0 {
            r_str16(p)?
        } else {
            None
        };
        Ok(SyncNotedAppOp {
            op_mode,
            op_code,
            attribution_tag,
            package_name,
        })
    }
}

crate::codec!(SyncNotedAppOp);
