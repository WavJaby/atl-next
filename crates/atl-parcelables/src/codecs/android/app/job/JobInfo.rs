// SPDX-License-Identifier: MIT
//! android.app.job.JobInfo (apex/jobscheduler/framework/java/android/app/job/JobInfo.java writeToParcel :1097-1143 / JobInfo(Parcel) :1006-1055), every field in wire order.
//! extras is writePersistableBundle and transientExtras writeBundle: both are BaseBundle.writeToParcelInner behind a -1 null, so both use the Bundle codec (byte- and object-exact).
//! JobInfo(Parcel) turns a null extras into PersistableBundle.EMPTY; kept as read here, a re-write of a null sends -1, which reads back as EMPTY again: the same JobInfo.
//! Not modelled: clipData (ClipData has no codec): a JobInfo carrying one is refused and counted. TriggerContentUri: Uri + flags.

use binder::binder_impl::Parcel;

use crate::codecs::android::net::NetworkRequest::NetworkRequest;
use crate::codecs::android::net::Uri::Uri;
use crate::codecs::android::os::Bundle::{Bundle, r_bundle, w_bundle};
use crate::parcel::*;
use crate::parts::*;

/// ComponentName's Parcelable class name: what writeParcelable(service) writes first.
const COMPONENT_CLASS: &str = "android.content.ComponentName";

/// JobInfo.TriggerContentUri (JobInfo.java:1222-1236): the Uri, then the flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerContentUri {
    pub uri: Uri,
    pub flags: i32,
}

impl Part for TriggerContentUri {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        self.uri.write(p)?;
        w_i32(p, self.flags)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        Ok(TriggerContentUri {
            uri: Uri::read(p)?,
            flags: r_i32(p)?,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct JobInfo {
    pub job_id: i32,
    pub extras: Option<Bundle>,
    pub transient_extras: Option<Bundle>,
    /// the job's JobService; null only in a malformed parcel (the Builder requires it)
    pub service: Option<ComponentName>,
    pub constraint_flags: i32,
    pub trigger_content_uris: Option<Vec<Option<TriggerContentUri>>>,
    pub trigger_content_update_delay: i64,
    pub trigger_content_max_delay: i64,
    pub network_request: Option<NetworkRequest>,
    pub network_download_bytes: i64,
    pub network_upload_bytes: i64,
    pub minimum_network_chunk_bytes: i64,
    pub min_latency_millis: i64,
    pub max_execution_delay_millis: i64,
    pub is_periodic: bool,
    pub is_persisted: bool,
    pub interval_millis: i64,
    pub flex_millis: i64,
    pub initial_backoff_millis: i64,
    pub backoff_policy: i32,
    pub has_early_constraint: bool,
    pub has_late_constraint: bool,
    pub bias: i32,
    pub priority: i32,
    pub flags: i32,
    /// mDebugTags; each non-null (the reader throws on a null one)
    pub debug_tags: Vec<String>,
    pub trace_tag: Option<String>,
}

impl JobInfo {
    /// The JobInfo's wire bytes: the identity JobSchedulerService's `job.equals` stands in for (equal fields write equal bytes). Refused (counted) when a Bundle holds binder or descriptor objects.
    pub fn wire_bytes(&self) -> Result<Vec<u8>> {
        let mut parcel = Parcel::new();
        self.write(&mut parcel.borrowed())?;
        parcel.marshal().map_err(|e| {
            if e == binder::StatusCode::INVALID_OPERATION {
                crate::refuse(
                    "android.app.job.JobInfo (a Bundle with binder or file descriptor objects)",
                )
            } else {
                e
            }
        })
    }
}

/// writeInt(b ? 1 : 0) / readInt() == 1: any other value reads as false, as Java does.
fn w_flag(p: &mut In<'_>, v: bool) -> Result<()> {
    w_i32(p, i32::from(v))
}
fn r_flag(p: &In<'_>) -> Result<bool> {
    Ok(r_i32(p)? == 1)
}

impl Part for JobInfo {
    fn write(&self, p: &mut In<'_>) -> Result<()> {
        w_i32(p, self.job_id)?;
        w_bundle(p, &self.extras)?;
        w_bundle(p, &self.transient_extras)?;
        // clipData: never held (refused on read)
        w_i32(p, 0)?;
        match &self.service {
            None => w_class(p, None)?,
            Some(c) => {
                w_class(p, Some(COMPONENT_CLASS))?;
                w_component(p, &Some(c.clone()))?;
            }
        }
        w_i32(p, self.constraint_flags)?;
        w_typed_list(p, &self.trigger_content_uris)?;
        w_i64(p, self.trigger_content_update_delay)?;
        w_i64(p, self.trigger_content_max_delay)?;
        w_typed(p, &self.network_request)?;
        w_i64(p, self.network_download_bytes)?;
        w_i64(p, self.network_upload_bytes)?;
        w_i64(p, self.minimum_network_chunk_bytes)?;
        w_i64(p, self.min_latency_millis)?;
        w_i64(p, self.max_execution_delay_millis)?;
        w_flag(p, self.is_periodic)?;
        w_flag(p, self.is_persisted)?;
        w_i64(p, self.interval_millis)?;
        w_i64(p, self.flex_millis)?;
        w_i64(p, self.initial_backoff_millis)?;
        w_i32(p, self.backoff_policy)?;
        w_flag(p, self.has_early_constraint)?;
        w_flag(p, self.has_late_constraint)?;
        w_i32(p, self.bias)?;
        w_i32(p, self.priority)?;
        w_i32(p, self.flags)?;
        w_i32(p, len_i32(self.debug_tags.len())?)?;
        for t in &self.debug_tags {
            w_str16(p, &Some(t.clone()))?;
        }
        w_str16(p, &self.trace_tag)
    }

    fn read(p: &In<'_>) -> Result<Self> {
        let job_id = r_i32(p)?;
        let extras = r_bundle(p)?;
        let transient_extras = r_bundle(p)?;
        if r_i32(p)? != 0 {
            return Err(crate::refuse("android.app.job.JobInfo.clipData"));
        }
        let service = match r_class(
            p,
            COMPONENT_CLASS,
            "android.app.job.JobInfo.service (not a ComponentName)",
        )? {
            None => None,
            Some(()) => r_component(p)?,
        };
        let constraint_flags = r_i32(p)?;
        let trigger_content_uris = r_typed_list(p)?;
        let trigger_content_update_delay = r_i64(p)?;
        let trigger_content_max_delay = r_i64(p)?;
        let network_request = r_typed(p)?;
        let network_download_bytes = r_i64(p)?;
        let network_upload_bytes = r_i64(p)?;
        let minimum_network_chunk_bytes = r_i64(p)?;
        let min_latency_millis = r_i64(p)?;
        let max_execution_delay_millis = r_i64(p)?;
        let is_periodic = r_flag(p)?;
        let is_persisted = r_flag(p)?;
        let interval_millis = r_i64(p)?;
        let flex_millis = r_i64(p)?;
        let initial_backoff_millis = r_i64(p)?;
        let backoff_policy = r_i32(p)?;
        let has_early_constraint = r_flag(p)?;
        let has_late_constraint = r_flag(p)?;
        let bias = r_i32(p)?;
        let priority = r_i32(p)?;
        let flags = r_i32(p)?;
        let n = r_i32(p)?;
        let mut debug_tags = Vec::new();
        for _ in 0..n.max(0) {
            // JobInfo(Parcel) throws "malformed parcel" on a null tag
            debug_tags.push(
                r_str16(p)?.ok_or_else(|| {
                    crate::refuse("android.app.job.JobInfo.mDebugTags (null tag)")
                })?,
            );
        }
        let trace_tag = r_str16(p)?;
        Ok(JobInfo {
            job_id,
            extras,
            transient_extras,
            service,
            constraint_flags,
            trigger_content_uris,
            trigger_content_update_delay,
            trigger_content_max_delay,
            network_request,
            network_download_bytes,
            network_upload_bytes,
            minimum_network_chunk_bytes,
            min_latency_millis,
            max_execution_delay_millis,
            is_periodic,
            is_persisted,
            interval_millis,
            flex_millis,
            initial_backoff_millis,
            backoff_policy,
            has_early_constraint,
            has_late_constraint,
            bias,
            priority,
            flags,
            debug_tags,
            trace_tag,
        })
    }
}

crate::codec!(JobInfo);
