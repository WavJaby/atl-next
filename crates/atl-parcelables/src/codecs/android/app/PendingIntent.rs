// SPDX-License-Identifier: MIT
//! android.app.PendingIntent (PendingIntent.java writeToParcel:1485-1486): one strong binder, the IIntentSender's. The NULL flag before it is the caller's (libbinder's Option impl).
//! PendingIntent.equals (:1450-1456) compares that binder, so the binder is the identity an alarm is matched by (atl-services alarm_table.rs). A null binder is refused: the CREATOR (:1494-1500) and readPendingIntentOrNullFromParcel (:1538-1540) read it as a null PendingIntent, which the writer never sends for a non-null one.

use binder::SpIBinder;

use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, PartialEq)]
pub struct PendingIntent {
    /// the IIntentSender's binder
    pub target: SpIBinder,
}

impl Part for PendingIntent {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_binder(p, &Some(self.target.clone()))
    }

    fn read(p: &In<'_>) -> Result<Self> {
        match r_binder(p)? {
            Some(target) => Ok(PendingIntent { target }),
            None => Err(crate::refuse("android.app.PendingIntent.target (null)")),
        }
    }
}

crate::codec!(PendingIntent);
