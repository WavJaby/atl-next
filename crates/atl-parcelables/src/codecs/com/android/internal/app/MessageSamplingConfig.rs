// SPDX-License-Identifier: MIT
//! com.android.internal.app.MessageSamplingConfig (MessageSamplingConfig.java writeToParcel:119-126, DataClass-generated): IAppOpsService.reportRuntimeAppOpAccessMessageAndGetConfig's answer,
//! which AppOpsManager stores as its sConfig (a null one makes its next isCollectingStackTraces throw). Layout: sampled op code, acceptable left distance, expiration (long).

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageSamplingConfig {
    pub sampled_op_code: i32,
    pub acceptable_left_distance: i32,
    pub expiration_time_since_boot_millis: i64,
}

impl Part for MessageSamplingConfig {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.sampled_op_code)?;
        w_i32(p, self.acceptable_left_distance)?;
        w_i64(p, self.expiration_time_since_boot_millis)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(MessageSamplingConfig {
            sampled_op_code: r_i32(p)?,
            acceptable_left_distance: r_i32(p)?,
            expiration_time_since_boot_millis: r_i64(p)?,
        })
    }
}

crate::codec!(MessageSamplingConfig);
