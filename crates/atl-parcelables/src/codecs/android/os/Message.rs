// SPDX-License-Identifier: MIT
//! android.os.Message (os/Message.java writeToParcel / readFromParcel): what, arg1, arg2, obj behind an int flag (a writeParcelable), when, data (writeBundle), replyTo (writeMessengerOrNullToParcel: a strong binder, null allowed),
//! sendingUid, workSourceUid. Not modelled, null only: obj (an arbitrary Parcelable).

use binder::SpIBinder;

use crate::codecs::android::os::Bundle::{Bundle, r_bundle, w_bundle};
use crate::parcel::*;
use crate::parts::*;

/// Message.UID_NONE: sendingUid and workSourceUid of a Message nobody set them on.
pub const UID_NONE: i32 = -1;

#[derive(Debug, Clone)]
pub struct Message {
    pub what: i32,
    pub arg1: i32,
    pub arg2: i32,
    pub when: i64,
    pub data: Option<Bundle>,
    pub reply_to: Option<SpIBinder>,
    pub sending_uid: i32,
    pub work_source_uid: i32,
}

impl Message {
    /// Message.obtain() with `what`, `arg1` and `data` set, as a service builds one to send (ConnectivityService.callCallbackForRequest).
    pub fn new(what: i32, arg1: i32, data: Bundle) -> Self {
        Message {
            what,
            arg1,
            arg2: 0,
            when: 0,
            data: Some(data),
            reply_to: None,
            sending_uid: UID_NONE,
            work_source_uid: UID_NONE,
        }
    }
}

impl Part for Message {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.what)?;
        w_i32(p, self.arg1)?;
        w_i32(p, self.arg2)?;
        // obj: null
        w_i32(p, 0)?;
        w_i64(p, self.when)?;
        w_bundle(p, &self.data)?;
        w_binder(p, &self.reply_to)?;
        w_i32(p, self.sending_uid)?;
        w_i32(p, self.work_source_uid)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let what = r_i32(p)?;
        let arg1 = r_i32(p)?;
        let arg2 = r_i32(p)?;
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.os.Message.obj"));
        }
        Ok(Message {
            what,
            arg1,
            arg2,
            when: r_i64(p)?,
            data: r_bundle(p)?,
            reply_to: r_binder(p)?,
            sending_uid: r_i32(p)?,
            work_source_uid: r_i32(p)?,
        })
    }
}

crate::codec!(Message);
