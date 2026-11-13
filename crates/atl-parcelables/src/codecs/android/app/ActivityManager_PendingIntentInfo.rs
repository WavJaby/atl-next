// SPDX-License-Identifier: MIT
//! android.app.ActivityManager.PendingIntentInfo (ActivityManager.java writeToParcel:6566-6571): IActivityManager.getInfoForIntentSender's answer, which PendingIntent.getCreatorPackage / getCreatorUid /
//! isImmutable / isBroadcast read. Written as a typed object: the presence int (the caller's), then the four fields below.

use crate::parcel::*;
use crate::parts::*;

/// Java's own field order: creatorPackage (String, nullable), creatorUid, immutable, intentSenderType.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityManager_PendingIntentInfo {
    pub creator_package: Option<String>,
    pub creator_uid: i32,
    pub immutable: bool,
    pub intent_sender_type: i32,
}

impl Part for ActivityManager_PendingIntentInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_str16(p, &self.creator_package)?;
        w_i32(p, self.creator_uid)?;
        w_bool(p, self.immutable)?;
        w_i32(p, self.intent_sender_type)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(ActivityManager_PendingIntentInfo {
            creator_package: r_str16(p)?,
            creator_uid: r_i32(p)?,
            immutable: r_bool(p)?,
            intent_sender_type: r_i32(p)?,
        })
    }
}

crate::codec!(ActivityManager_PendingIntentInfo);
