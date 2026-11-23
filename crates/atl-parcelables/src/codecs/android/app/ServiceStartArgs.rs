// SPDX-License-Identifier: MIT
//! android.app.ServiceStartArgs: app/ServiceStartArgs.java writeToParcel (:58-68) / ServiceStartArgs(Parcel) (:75-84). Wire: int32 taskRemoved (0/1), int32 startId, int32 flags,
//! int32 hasIntent, then the Intent body (Intent.writeToParcel with flags 0) when hasIntent is non-zero. Carried as the elements of a ParceledListSlice (scheduleServiceArgs).

use crate::codecs::android::content::Intent::Intent;
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone)]
pub struct ServiceStartArgs {
    /// taskRemoved: this start is the onTaskRemoved delivery
    pub task_removed: bool,
    /// startId: what stopSelf(startId) names
    pub start_id: i32,
    /// flags: START_FLAG_REDELIVERY / START_FLAG_RETRY
    pub flags: i32,
    /// args: the start's Intent; None for the sticky restart's null Intent
    pub args: Option<Intent>,
}

impl Part for ServiceStartArgs {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_bool(p, self.task_removed)?;
        w_i32(p, self.start_id)?;
        w_i32(p, self.flags)?;
        match &self.args {
            Some(intent) => {
                w_i32(p, 1)?;
                intent.write(p)
            }
            None => w_i32(p, 0),
        }
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let task_removed = r_bool(p)?;
        let start_id = r_i32(p)?;
        let flags = r_i32(p)?;
        let args = if r_i32(p)? != 0 {
            Some(Intent::read(p)?)
        } else {
            None
        };
        Ok(ServiceStartArgs {
            task_removed,
            start_id,
            flags,
            args,
        })
    }
}

crate::codec!(ServiceStartArgs);
