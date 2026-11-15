// SPDX-License-Identifier: MIT
//! android.app.job.JobParameters (apex/jobscheduler/framework/java/android/app/job/JobParameters.java writeToParcel / JobParameters(Parcel)), every field in wire order: jobId, namespace, extras
//! (writePersistableBundle) and transientExtras (writeBundle) through the Bundle codec, the clipData presence int, the IJobCallback binder, overrideDeadlineExpired (int), expedited and
//! user-initiated (writeBoolean), the triggered content Uris (writeTypedArray) and authorities, the Network presence int, the two stop reasons and the debug stop reason.
//! Not modelled: clipData, triggered Uris and Network (no codec for ClipData; the service never sets the others): only null is written, and a parcel carrying one is refused.

use binder::SpIBinder;

use crate::codecs::android::os::Bundle::{Bundle, r_bundle, w_bundle};
use crate::parcel::*;
use crate::parts::*;

#[derive(Debug, Clone, Default)]
pub struct JobParameters {
    pub job_id: i32,
    pub namespace: Option<String>,
    pub extras: Option<Bundle>,
    pub transient_extras: Option<Bundle>,
    /// IJobCallback: the running job's callback binder
    pub callback: Option<SpIBinder>,
    pub override_deadline_expired: bool,
    pub is_expedited: bool,
    pub is_user_initiated: bool,
    pub triggered_content_authorities: Option<Vec<Option<String>>>,
    pub stop_reason: i32,
    pub internal_stop_reason: i32,
    pub debug_stop_reason: Option<String>,
}

impl Part for JobParameters {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.job_id)?;
        w_str16(p, &self.namespace)?;
        w_bundle(p, &self.extras)?;
        w_bundle(p, &self.transient_extras)?;
        // clipData absent
        w_i32(p, 0)?;
        w_binder(p, &self.callback)?;
        w_i32(p, i32::from(self.override_deadline_expired))?;
        w_bool(p, self.is_expedited)?;
        w_bool(p, self.is_user_initiated)?;
        // mTriggeredContentUris: null typed array
        w_null(p)?;
        w_str16_array(p, &self.triggered_content_authorities)?;
        // mNetwork absent
        w_i32(p, 0)?;
        w_i32(p, self.stop_reason)?;
        w_i32(p, self.internal_stop_reason)?;
        w_str16(p, &self.debug_stop_reason)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let job_id = r_i32(p)?;
        let namespace = r_str16(p)?;
        let extras = r_bundle(p)?;
        let transient_extras = r_bundle(p)?;
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.app.job.JobParameters (clipData)"));
        }
        let callback = r_binder(p)?;
        let override_deadline_expired = r_i32(p)? == 1;
        let is_expedited = r_bool(p)?;
        let is_user_initiated = r_bool(p)?;
        r_null(p, "android.app.job.JobParameters (triggered content Uris)")?;
        let triggered_content_authorities = r_str16_array(p)?;
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.app.job.JobParameters (Network)"));
        }
        Ok(JobParameters {
            job_id,
            namespace,
            extras,
            transient_extras,
            callback,
            override_deadline_expired,
            is_expedited,
            is_user_initiated,
            triggered_content_authorities,
            stop_reason: r_i32(p)?,
            internal_stop_reason: r_i32(p)?,
            debug_stop_reason: r_str16(p)?,
        })
    }
}

crate::codec!(JobParameters);
