// SPDX-License-Identifier: MIT
//! com.android.internal.inputmethod.InputConnectionCommandHeader (InputConnectionCommandHeader.java writeToParcel:65-67): the session id every IRemoteInputConnection call carries.
//! The app's RemoteInputConnectionImpl drops a call whose id differs from its current one (RemoteInputConnectionImpl.java:496); the id starts at 0 and moves with invalidateInput.

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputConnectionCommandHeader {
    pub session_id: i32,
}

impl Part for InputConnectionCommandHeader {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.session_id)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(InputConnectionCommandHeader {
            session_id: r_i32(p)?,
        })
    }
}

crate::codec!(InputConnectionCommandHeader);
