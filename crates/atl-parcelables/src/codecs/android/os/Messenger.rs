// SPDX-License-Identifier: MIT
//! android.os.Messenger (os/Messenger.java writeToParcel / CREATOR): one strong binder, its IMessenger's. The CREATOR reads a null binder as a null Messenger, which the writer never sends for a non-null one, so it is refused.
//! Messenger.equals compares that binder: the binder is the identity a service keys a client's callbacks by.

use binder::SpIBinder;

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, PartialEq)]
pub struct Messenger {
    /// the IMessenger's binder
    pub target: SpIBinder,
}

impl Part for Messenger {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &Some(self.target.clone()))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        match r_binder(p)? {
            Some(target) => Ok(Messenger { target }),
            None => Err(crate::refuse("android.os.Messenger.mTarget (null)")),
        }
    }
}

crate::codec!(Messenger);
