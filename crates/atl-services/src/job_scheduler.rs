// SPDX-License-Identifier: MIT
//! The `jobscheduler` service (Context.JOB_SCHEDULER_SERVICE): IJobScheduler-lite after JobSchedulerService (apex/jobscheduler/service/.../job/JobSchedulerService.java).
//! Without it SystemServiceRegistry answers null (JobSchedulerFrameworkInitializer registers a static service over `jobscheduler`), and androidx JobIntentService.enqueueWork NPEs on `JobScheduler.enqueue`.
//! Jobs and their work items are kept per (uid, namespace, jobId), as JobStore keys them; schedule / enqueue / cancel / the pending-job queries follow JobSchedulerService.
//! Nothing runs yet: running a job needs the activity service to bind the app's JobService (ActiveServices bindService -> IJobService.startJob), which it does not model.
//! Each accepted job is counted as `schedule|enqueue (stored, never runs)`, so the gap shows in the log (alarm.rs precedent).
//! Not modelled: persistence across restarts, constraints, quotas, the per-app 150-job limit (JobSchedulerService MAX_JOBS_PER_APP), user-initiated jobs, the canPersistJobs and network-permission checks.
//! Counted UNKNOWN (the stub default): getAllPendingJobs (its java.util.Map reply has no codec; JobScheduler.getAllPendingJobs uses getAllPendingJobsInNamespace), scheduleAsPackage, getPendingJobReason(s), getPendingJobReasonsHistory, canRunUserInitiatedJobs, hasRunUserInitiatedJobsPermission, getStartedJobs, getAllJobSnapshots,
//! and the @EnforcePermission observer methods after their permission check.

use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::app::job::IJobScheduler::{
    BnJobScheduler, EnforcePermission, IJobScheduler,
};
use atl_parcelables::android::app::job::{JobInfo, JobWorkItem};
use atl_parcelables::android::content::pm::ParceledListSlice;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::{Packages, Permissions};

/// The interface name counters use.
const INTERFACE: &str = "IJobScheduler";
/// JobScheduler.RESULT_SUCCESS.
pub const RESULT_SUCCESS: i32 = 1;
/// JobService.PERMISSION_BIND: the permission a job's service must require (enforceValidJobRequest).
const PERMISSION_BIND: &str = "android.permission.BIND_JOB_SERVICE";

/// One scheduled job (JobStatus, reduced): its owner, namespace, JobInfo and pending work (JobStatus.pendingWork).
#[derive(Debug, Clone)]
pub struct Job {
    pub uid: u32,
    pub namespace: Option<String>,
    pub info: JobInfo,
    pub work: Vec<JobWorkItem>,
}

/// The job table (JobStore, in memory). Logic only, so it is unit-tested without binder.
#[derive(Debug, Default)]
pub struct JobTable {
    jobs: Vec<Job>,
    /// JobStatus.nextPendingWorkId; one counter for the table is enough: ids only need to be unique per job
    next_work_id: i32,
}

impl JobTable {
    fn position(&self, uid: u32, namespace: Option<&str>, job_id: i32) -> Option<usize> {
        self.jobs.iter().position(|j| {
            j.uid == uid && j.namespace.as_deref() == namespace && j.info.job_id == job_id
        })
    }

    /// scheduleAsPackage (:1790-1860): with work, a pending job whose JobInfo equals `info` takes the item (enqueueWorkLocked) and stays; otherwise the old job (and its work) is cancelled and
    /// `info` replaces it, holding `work` if given. `same` is JobInfo.equals (here: equal wire bytes; a JobInfo that cannot be compared counts as different, so it replaces).
    pub fn schedule(
        &mut self,
        uid: u32,
        namespace: Option<&str>,
        info: JobInfo,
        work: Option<JobWorkItem>,
        same: impl Fn(&JobInfo, &JobInfo) -> bool,
    ) {
        let at = self.position(uid, namespace, info.job_id);
        let work = work.map(|mut w| {
            self.next_work_id = self.next_work_id.wrapping_add(1);
            w.work_id = self.next_work_id;
            w.delivery_count = 0;
            w
        });
        if let (Some(i), Some(w)) = (at, &work)
            && same(&self.jobs[i].info, &info)
        {
            self.jobs[i].work.push(w.clone());
            return;
        }
        if let Some(i) = at {
            self.jobs.remove(i);
        }
        self.jobs.push(Job {
            uid,
            namespace: namespace.map(str::to_owned),
            info,
            work: work.into_iter().collect(),
        });
    }

    /// cancelJob (:5246): the caller's job of that namespace and id, if any.
    pub fn cancel(&mut self, uid: u32, namespace: Option<&str>, job_id: i32) -> bool {
        self.position(uid, namespace, job_id)
            .map(|i| self.jobs.remove(i))
            .is_some()
    }

    /// cancelJobsForUid (cancelAll) or, with `Some(ns)`, only that namespace (cancelAllInNamespace; a null namespace there is the default one).
    pub fn cancel_all(&mut self, uid: u32, namespace: Option<Option<&str>>) {
        self.jobs
            .retain(|j| j.uid != uid || namespace.is_some_and(|ns| j.namespace.as_deref() != ns));
    }

    /// The caller's pending jobs.
    pub fn of(&self, uid: u32) -> impl Iterator<Item = &Job> {
        self.jobs.iter().filter(move |j| j.uid == uid)
    }

    pub fn get(&self, uid: u32, namespace: Option<&str>, job_id: i32) -> Option<&Job> {
        self.position(uid, namespace, job_id).map(|i| &self.jobs[i])
    }
}

struct Inner {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    table: Mutex<JobTable>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, JobTable> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// enforceValidJobRequest (JobSchedulerService.java): the service exists, belongs to the caller's uid and requires BIND_JOB_SERVICE; else IllegalArgumentException.
    fn validate(&self, uid: u32, job: &JobInfo) -> binder::Result<()> {
        let Some(service) = &job.service else {
            return Err(illegal_argument("JobInfo has no service".to_owned()));
        };
        let (package, class) = (service.package.as_str(), service.class.as_str());
        let shown = format!("ComponentInfo{{{package}/{class}}}");
        let p = self.packages.by_name(package);
        let Some(si) = p.and_then(|p| p.service(class)) else {
            return Err(illegal_argument(format!(
                "Tried to schedule job for non-existent component: {shown}"
            )));
        };
        if p.map(|p| p.uid) != Some(uid) {
            return Err(illegal_argument(format!(
                "uid {uid} cannot schedule job in {package}"
            )));
        }
        if si.permission.as_deref() != Some(PERMISSION_BIND) {
            return Err(illegal_argument(format!(
                "Scheduled service {shown} does not require android.permission.BIND_JOB_SERVICE permission"
            )));
        }
        Ok(())
    }

    fn schedule(
        &self,
        namespace: Option<&str>,
        job: &JobInfo,
        work: Option<&JobWorkItem>,
        method: &str,
    ) -> binder::Result<i32> {
        let uid = ThreadState::get_calling_uid();
        self.validate(uid, job)?;
        self.lock().schedule(
            uid,
            namespace,
            job.clone(),
            work.cloned(),
            |a, b| matches!((a.wire_bytes(), b.wire_bytes()), (Ok(x), Ok(y)) if x == y),
        );
        atl_diag::answered(INTERFACE, &format!("{method} (stored, never runs)"));
        Ok(RESULT_SUCCESS)
    }
}

fn non_null<'a, T>(v: Option<&'a T>, msg: &str) -> binder::Result<&'a T> {
    v.ok_or_else(|| Status::new_exception_str(ExceptionCode::NULL_POINTER, Some(msg)))
}

fn illegal_argument(msg: String) -> Status {
    Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(msg))
}

struct JobSchedulerService(Arc<Inner>);

impl Interface for JobSchedulerService {}

impl IJobScheduler for JobSchedulerService {
    /// @EnforcePermission (the user-visible job observer methods): root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .0
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    /// schedule (:5034) -> scheduleAsPackage without work. A null job NPEs in enforceValidJobRequest (`job.getService()`).
    fn r#schedule(&self, namespace: Option<&str>, job: Option<&JobInfo>) -> binder::Result<i32> {
        self.0
            .schedule(namespace, non_null(job, "job is null")?, None, "schedule")
    }

    /// enqueue (:5064): the job is validated first, then a null work item is NullPointerException ("work is null"), else scheduleAsPackage with it.
    fn r#enqueue(
        &self,
        namespace: Option<&str>,
        job: Option<&JobInfo>,
        work: Option<&JobWorkItem>,
    ) -> binder::Result<i32> {
        let job = non_null(job, "job is null")?;
        self.0.validate(ThreadState::get_calling_uid(), job)?;
        self.0.schedule(
            namespace,
            job,
            Some(non_null(work, "work is null")?),
            "enqueue",
        )
    }

    fn r#cancel(&self, namespace: Option<&str>, job_id: i32) -> binder::Result<()> {
        self.0
            .lock()
            .cancel(ThreadState::get_calling_uid(), namespace, job_id);
        atl_diag::answered(INTERFACE, "cancel");
        Ok(())
    }

    fn r#cancelAll(&self) -> binder::Result<()> {
        self.0
            .lock()
            .cancel_all(ThreadState::get_calling_uid(), None);
        atl_diag::answered(INTERFACE, "cancelAll");
        Ok(())
    }

    fn r#cancelAllInNamespace(&self, namespace: Option<&str>) -> binder::Result<()> {
        self.0
            .lock()
            .cancel_all(ThreadState::get_calling_uid(), Some(namespace));
        atl_diag::answered(INTERFACE, "cancelAllInNamespace");
        Ok(())
    }

    /// getAllPendingJobsInNamespace (:5150): what JobScheduler.getAllPendingJobs asks (JobSchedulerImpl.java:137).
    fn r#getAllPendingJobsInNamespace(
        &self,
        namespace: Option<&str>,
    ) -> binder::Result<Option<ParceledListSlice<JobInfo>>> {
        let uid = ThreadState::get_calling_uid();
        let jobs = self
            .0
            .lock()
            .of(uid)
            .filter(|j| j.namespace.as_deref() == namespace)
            .map(|j| j.info.clone())
            .collect();
        atl_diag::answered(INTERFACE, "getAllPendingJobsInNamespace");
        Ok(Some(ParceledListSlice::new(jobs)))
    }

    fn r#getPendingJob(
        &self,
        namespace: Option<&str>,
        job_id: i32,
    ) -> binder::Result<Option<JobInfo>> {
        let uid = ThreadState::get_calling_uid();
        let job = self
            .0
            .lock()
            .get(uid, namespace, job_id)
            .map(|j| j.info.clone());
        atl_diag::answered(INTERFACE, "getPendingJob");
        Ok(job)
    }
}

/// The service's binder, for `addService`.
pub fn new_binder(packages: Arc<Packages>, permissions: Arc<Permissions>) -> SpIBinder {
    BnJobScheduler::new_binder(
        JobSchedulerService(Arc::new(Inner {
            packages,
            permissions,
            table: Mutex::new(JobTable::default()),
        })),
        BinderFeatures::default(),
    )
    .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: i32, flags: i32) -> JobInfo {
        JobInfo {
            job_id: id,
            flags,
            ..Default::default()
        }
    }
    fn same(a: &JobInfo, b: &JobInfo) -> bool {
        a.job_id == b.job_id && a.flags == b.flags
    }
    fn item() -> JobWorkItem {
        JobWorkItem::default()
    }

    /// JobIntentService enqueues the same JobInfo each time: the work piles up on one job (scheduleAsPackage :1814 enqueueWorkLocked).
    #[test]
    fn enqueue_same_job_appends_work() {
        let mut t = JobTable::default();
        t.schedule(10, None, info(7, 0), Some(item()), same);
        t.schedule(10, None, info(7, 0), Some(item()), same);
        let j = t.get(10, None, 7).expect("job");
        assert_eq!(j.work.len(), 2);
        assert_ne!(j.work[0].work_id, j.work[1].work_id);
    }

    /// A different JobInfo under the same id replaces the job and drops its work (cancelJobImplLocked before the new one).
    #[test]
    fn different_job_replaces() {
        let mut t = JobTable::default();
        t.schedule(10, None, info(7, 0), Some(item()), same);
        t.schedule(10, None, info(7, 1), None, same);
        let j = t.get(10, None, 7).expect("job");
        assert!(j.work.is_empty());
        assert_eq!(j.info.flags, 1);
        assert_eq!(t.of(10).count(), 1);
    }

    /// Keys are (uid, namespace, id): another uid or namespace is a different job, and cancels stay inside the caller's uid.
    #[test]
    fn keys_and_cancel() {
        let mut t = JobTable::default();
        t.schedule(10, None, info(1, 0), None, same);
        t.schedule(10, Some("ns"), info(1, 0), None, same);
        t.schedule(11, None, info(1, 0), None, same);
        assert_eq!(t.of(10).count(), 2);
        assert!(!t.cancel(10, None, 2));
        t.cancel_all(10, Some(Some("ns")));
        assert_eq!(t.of(10).count(), 1);
        t.cancel_all(10, None);
        assert_eq!(t.of(10).count(), 0);
        assert_eq!(t.of(11).count(), 1);
    }
}
