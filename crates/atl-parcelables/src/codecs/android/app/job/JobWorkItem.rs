// SPDX-License-Identifier: MIT
//! android.app.job.JobWorkItem (apex/jobscheduler/framework/java/android/app/job/JobWorkItem.java writeToParcel / JobWorkItem(Parcel)): a presence int + the Intent, the extras (writePersistableBundle:
//! the Bundle codec), the three network byte counts, mDeliveryCount and mWorkId. mGrants is not parcelled. The service sets delivery count and work id when it hands an item out (JobStatus.enqueueWorkLocked).

use crate::codecs::android::content::Intent::Intent;
use crate::codecs::android::os::Bundle::{Bundle, r_bundle, w_bundle};
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct JobWorkItem {
    pub intent: Option<Intent>,
    pub extras: Option<Bundle>,
    pub network_download_bytes: i64,
    pub network_upload_bytes: i64,
    pub minimum_chunk_bytes: i64,
    pub delivery_count: i32,
    pub work_id: i32,
}

impl Part for JobWorkItem {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_typed(p, &self.intent)?;
        w_bundle(p, &self.extras)?;
        w_i64(p, self.network_download_bytes)?;
        w_i64(p, self.network_upload_bytes)?;
        w_i64(p, self.minimum_chunk_bytes)?;
        w_i32(p, self.delivery_count)?;
        w_i32(p, self.work_id)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(JobWorkItem {
            intent: r_typed(p)?,
            extras: r_bundle(p)?,
            network_download_bytes: r_i64(p)?,
            network_upload_bytes: r_i64(p)?,
            minimum_chunk_bytes: r_i64(p)?,
            delivery_count: r_i32(p)?,
            work_id: r_i32(p)?,
        })
    }
}

crate::codec!(JobWorkItem);
