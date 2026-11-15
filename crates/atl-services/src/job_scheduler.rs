// SPDX-License-Identifier: MIT
//! The `jobscheduler` service (Context.JOB_SCHEDULER_SERVICE): IJobScheduler-lite after JobSchedulerService (apex/jobscheduler/service/.../job/JobSchedulerService.java).
//! Without it SystemServiceRegistry answers null (JobSchedulerFrameworkInitializer registers a static service over `jobscheduler`), and androidx JobIntentService.enqueueWork NPEs on `JobScheduler.enqueue`.
//! Jobs and their work items are kept per (uid, namespace, jobId), as JobStore keys them; schedule / enqueue / cancel / the pending-job queries follow JobSchedulerService.
//! Running follows JobServiceContext: the activity service binds the app's JobService as the system (BIND_AUTO_CREATE), onServiceConnected sends IJobService.startJob(JobParameters) carrying this run's
//! IJobCallback; acknowledgeStartMessage(false), jobFinished, or dequeueWork finding no work left ends the run (unbind). dequeueWork / completeWork move work items as JobStatus does.
//! Desktop policy (owner behaviour decisions 2/3): a job runs as soon as it is scheduled. No constraint is modelled (charging, battery not low, idle, storage not low, network, minimum latency,
//! content triggers): each one a started job carries is counted `constraint ignored (runs at once): <name>`. Periodic and rescheduled jobs stay stored after their run and do not run again
//! (interval and backoff not modelled; counted). A running job that is cancelled gets stopJob and ends at acknowledgeStopMessage; one replaced by a different JobInfo keeps running on the new job's
//! work (AOSP stops it; counted at its end as cancelled or replaced).
//! Not modelled: persistence across restarts, quotas and the concurrency limit, the per-app 150-job limit (JobSchedulerService MAX_JOBS_PER_APP), user-initiated jobs, timeouts, the canPersistJobs and
//! network-permission checks.
//! Counted UNKNOWN (the stub default): getAllPendingJobs (its java.util.Map reply has no codec; JobScheduler.getAllPendingJobs uses getAllPendingJobsInNamespace), scheduleAsPackage, getPendingJobReason(s), getPendingJobReasonsHistory, canRunUserInitiatedJobs, hasRunUserInitiatedJobsPermission, getStartedJobs, getAllJobSnapshots,
//! and the @EnforcePermission observer methods after their permission check.

use std::sync::{Arc, Mutex, MutexGuard, Weak};

use atl_aidl::aidl::android::app::IServiceConnection::{BnServiceConnection, IServiceConnection};
use atl_aidl::aidl::android::app::job::IJobCallback::{BnJobCallback, IJobCallback};
use atl_aidl::aidl::android::app::job::IJobScheduler::{
    BnJobScheduler, EnforcePermission, IJobScheduler,
};
use atl_aidl::aidl::android::app::job::IJobService::IJobService;
use atl_parcelables::android::app::job::{JobInfo, JobParameters, JobWorkItem};
use atl_parcelables::android::content::pm::ParceledListSlice;
use atl_parcelables::android::content::{ComponentName, Intent};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::activity::ActivityManager;
use crate::activity::log;
use crate::bound_services::BIND_AUTO_CREATE;
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

    fn get_mut(&mut self, uid: u32, namespace: Option<&str>, job_id: i32) -> Option<&mut Job> {
        self.position(uid, namespace, job_id)
            .map(|i| &mut self.jobs[i])
    }

    /// JobStatus.dequeueWorkLocked: the first pending item moves to the run's executing list with its delivery count bumped.
    pub fn dequeue(
        &mut self,
        uid: u32,
        namespace: Option<&str>,
        job_id: i32,
    ) -> Option<JobWorkItem> {
        let job = self.get_mut(uid, namespace, job_id)?;
        if job.work.is_empty() {
            return None;
        }
        let mut w = job.work.remove(0);
        w.delivery_count += 1;
        Some(w)
    }

    /// The end of a run (JobSchedulerService.onJobCompletedLocked, reduced): items still executing go back to the front of the pending work; the job stays when rescheduled, periodic or holding
    /// work, else it is done and leaves the table. Returns whether it should run again now (work left; rescheduled and periodic jobs do not: backoff and intervals are not modelled).
    pub fn finished(
        &mut self,
        key: (u32, Option<&str>, i32),
        executing: Vec<JobWorkItem>,
        reschedule: bool,
    ) -> Finished {
        let Some(i) = self.position(key.0, key.1, key.2) else {
            return Finished::Gone;
        };
        let job = &mut self.jobs[i];
        job.work.splice(0..0, executing);
        if reschedule {
            Finished::KeptRescheduled
        } else if job.info.is_periodic {
            Finished::KeptPeriodic
        } else if !job.work.is_empty() {
            Finished::RunAgain
        } else {
            self.jobs.remove(i);
            Finished::Done
        }
    }
}

/// What [`JobTable::finished`] did with the job.
#[derive(Debug, PartialEq, Eq)]
pub enum Finished {
    /// cancelled or replaced while it ran
    Gone,
    Done,
    RunAgain,
    KeptRescheduled,
    KeptPeriodic,
}

/// JobServiceContext.mVerb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Binding,
    Starting,
    Executing,
    /// stopJob sent (the job was cancelled); the run ends at acknowledgeStopMessage or jobFinished
    Stopping,
}

/// One running job (a JobServiceContext with its job).
struct Run {
    /// the run's identity: its callback and connection carry it (verifyCallerLocked compares the callback object)
    generation: u64,
    uid: u32,
    namespace: Option<String>,
    job_id: i32,
    verb: Verb,
    conn: Strong<dyn IServiceConnection>,
    /// the app's IJobService once connected
    service: Option<Strong<dyn IJobService>>,
    params: JobParameters,
    /// JobStatus.executingWork
    executing: Vec<JobWorkItem>,
}

#[derive(Default)]
struct State {
    table: JobTable,
    runs: Vec<Run>,
    next_generation: u64,
}

impl State {
    fn run(&mut self, generation: u64) -> Option<usize> {
        self.runs.iter().position(|r| r.generation == generation)
    }
}

/// JobInfo.CONSTRAINT_FLAG_* (JobInfo.java) with the names the counter uses.
const CONSTRAINT_FLAGS: [(i32, &str); 4] = [
    (1, "charging"),
    (1 << 1, "battery not low"),
    (1 << 2, "device idle"),
    (1 << 3, "storage not low"),
];

/// The constraints a job carries that the desktop policy ignores.
fn ignored_constraints(info: &JobInfo) -> Vec<&'static str> {
    let mut v: Vec<&str> = CONSTRAINT_FLAGS
        .iter()
        .filter(|(bit, _)| info.constraint_flags & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if info.network_request.is_some() {
        v.push("network");
    }
    if info.min_latency_millis > 0 {
        v.push("minimum latency");
    }
    if info.trigger_content_uris.is_some() {
        v.push("content trigger");
    }
    v
}

struct Inner {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    state: Mutex<State>,
    /// the activity service that binds job services; None is the test-only break `--no-job-runner`: jobs are stored, never run
    runner: Option<ActivityManager>,
    me: Weak<Inner>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// JobServiceContext.executeRunnableJob: a job not already running gets a run and its service is bound (bindServiceAsUser as the system, BIND_AUTO_CREATE).
    fn start(&self, uid: u32, namespace: Option<&str>, job_id: i32) {
        let Some(am) = &self.runner else {
            atl_diag::answered(INTERFACE, "job stored, runner off (--no-job-runner)");
            return;
        };
        let (conn, intent) = {
            let mut st = self.lock();
            if st
                .runs
                .iter()
                .any(|r| r.uid == uid && r.namespace.as_deref() == namespace && r.job_id == job_id)
            {
                return;
            }
            let Some(info) = st.table.get(uid, namespace, job_id).map(|j| j.info.clone()) else {
                return;
            };
            let Some(service) = info.service.clone() else {
                return;
            };
            for c in ignored_constraints(&info) {
                atl_diag::answered(
                    INTERFACE,
                    &format!("constraint ignored (runs at once): {c}"),
                );
            }
            st.next_generation += 1;
            let generation = st.next_generation;
            let callback = BnJobCallback::new_binder(
                JobCallback {
                    inner: self.me.clone(),
                    generation,
                },
                BinderFeatures::default(),
            );
            let conn = BnServiceConnection::new_binder(
                JobConnection {
                    inner: self.me.clone(),
                    generation,
                },
                BinderFeatures::default(),
            );
            let params = JobParameters {
                job_id,
                namespace: namespace.map(str::to_owned),
                extras: info.extras,
                transient_extras: info.transient_extras,
                callback: Some(callback.as_binder()),
                ..Default::default()
            };
            st.runs.push(Run {
                generation,
                uid,
                namespace: namespace.map(str::to_owned),
                job_id,
                verb: Verb::Binding,
                conn: conn.clone(),
                service: None,
                params,
                executing: Vec::new(),
            });
            let intent = Intent {
                component: Some(ComponentName {
                    package: service.package,
                    class: service.class,
                }),
                ..Default::default()
            };
            (conn, intent)
        };
        let code = am.bind_service_as_system(&intent, conn.clone(), BIND_AUTO_CREATE);
        if code <= 0 {
            // executeRunnableJob: a failed bind ends the run; the job stays stored
            atl_diag::answered(INTERFACE, "job service bind failed");
            log(format_args!(
                "jobscheduler: binding {:?} answered {code}",
                intent.component
            ));
            let mut st = self.lock();
            st.runs.retain(|r| r.conn.as_binder() != conn.as_binder());
        } else if !self
            .lock()
            .runs
            .iter()
            .any(|r| r.conn.as_binder() == conn.as_binder())
        {
            // cancelled before the bind was made (stop_cancelled ran between the run's creation and the bind)
            am.unbind_service_as_system(conn);
        }
    }

    /// onServiceConnected -> handleServiceBoundLocked (IJobService.startJob); onServiceDisconnected / onBindingDied end the run as rescheduled (closeAndCleanupJobLocked(true)).
    fn connected(&self, generation: u64, service: Option<&SpIBinder>, dead: bool) {
        let started = {
            let mut st = self.lock();
            let Some(i) = st.run(generation) else { return };
            match (service, dead) {
                (Some(b), false) if st.runs[i].verb == Verb::Binding => {
                    match b.clone().into_interface::<dyn IJobService>() {
                        Ok(svc) => {
                            st.runs[i].verb = Verb::Starting;
                            st.runs[i].service = Some(svc.clone());
                            Some((svc, st.runs[i].params.clone()))
                        }
                        Err(e) => {
                            log(format_args!("jobscheduler: not an IJobService: {e:?}"));
                            drop(st);
                            self.finish(generation, false);
                            return;
                        }
                    }
                }
                (Some(_), false) => None,
                _ => {
                    drop(st);
                    self.finish(generation, true);
                    return;
                }
            }
        };
        if let Some((svc, params)) = started {
            atl_diag::answered(INTERFACE, "startJob");
            if let Err(e) = svc.r#startJob(Some(&params)) {
                log(format_args!("jobscheduler: startJob: {e:?}"));
                self.finish(generation, true);
            }
        }
    }

    /// cancelJobImplLocked for running jobs (JobServiceContext.cancelExecutingJobLocked -> doCancelLocked): runs whose job left the table stop. Still binding: the run ends at once; started: stopJob,
    /// and the run ends when the app acknowledges (handleCancelLocked -> sendStopMessageLocked).
    fn stop_cancelled(&self) {
        let (unbound, stops) = {
            let mut st = self.lock();
            let mut unbound = Vec::new();
            let mut stops = Vec::new();
            let st = &mut *st;
            let table = &st.table;
            st.runs.retain_mut(|r| {
                if table.get(r.uid, r.namespace.as_deref(), r.job_id).is_some()
                    || r.verb == Verb::Stopping
                {
                    return true;
                }
                if r.verb == Verb::Binding {
                    unbound.push(r.conn.clone());
                    return false;
                }
                r.verb = Verb::Stopping;
                stops.push((r.generation, r.params.clone()));
                true
            });
            (unbound, stops)
        };
        if let Some(am) = &self.runner {
            for conn in unbound {
                atl_diag::answered(INTERFACE, "running job cancelled while binding");
                am.unbind_service_as_system(conn);
            }
        }
        for (generation, params) in stops {
            let svc = self
                .lock()
                .runs
                .iter()
                .find(|r| r.generation == generation)
                .and_then(|r| r.service.clone());
            atl_diag::answered(INTERFACE, "stopJob (job cancelled)");
            match svc.map(|s| s.r#stopJob(Some(&params))) {
                Some(Ok(())) => {}
                other => {
                    log(format_args!("jobscheduler: stopJob: {other:?}"));
                    self.finish(generation, false);
                }
            }
        }
    }

    /// closeAndCleanupJobLocked: the run ends, the table decides the job's fate, the service is unbound; work left runs again.
    fn finish(&self, generation: u64, reschedule: bool) {
        let (conn, key, again) = {
            let mut st = self.lock();
            let Some(i) = st.run(generation) else { return };
            let run = st.runs.remove(i);
            let outcome = st.table.finished(
                (run.uid, run.namespace.as_deref(), run.job_id),
                run.executing,
                reschedule,
            );
            let what = match outcome {
                Finished::KeptRescheduled => {
                    "job rescheduled: kept, not run again (backoff not modelled)"
                }
                Finished::KeptPeriodic => {
                    "periodic job: kept, not run again (interval not modelled)"
                }
                Finished::Gone => "run ended after its job was cancelled or replaced",
                Finished::Done | Finished::RunAgain => "job finished",
            };
            atl_diag::answered(INTERFACE, what);
            (
                run.conn,
                (run.uid, run.namespace, run.job_id),
                outcome == Finished::RunAgain,
            )
        };
        if let Some(am) = &self.runner {
            am.unbind_service_as_system(conn);
        }
        if again {
            self.start(key.0, key.1.as_deref(), key.2);
        }
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
        self.lock().table.schedule(
            uid,
            namespace,
            job.clone(),
            work.cloned(),
            |a, b| matches!((a.wire_bytes(), b.wire_bytes()), (Ok(x), Ok(y)) if x == y),
        );
        atl_diag::answered(INTERFACE, method);
        self.start(uid, namespace, job.job_id);
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
            .table
            .cancel(ThreadState::get_calling_uid(), namespace, job_id);
        atl_diag::answered(INTERFACE, "cancel");
        self.0.stop_cancelled();
        Ok(())
    }

    fn r#cancelAll(&self) -> binder::Result<()> {
        self.0
            .lock()
            .table
            .cancel_all(ThreadState::get_calling_uid(), None);
        atl_diag::answered(INTERFACE, "cancelAll");
        self.0.stop_cancelled();
        Ok(())
    }

    fn r#cancelAllInNamespace(&self, namespace: Option<&str>) -> binder::Result<()> {
        self.0
            .lock()
            .table
            .cancel_all(ThreadState::get_calling_uid(), Some(namespace));
        atl_diag::answered(INTERFACE, "cancelAllInNamespace");
        self.0.stop_cancelled();
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
            .table
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
            .table
            .get(uid, namespace, job_id)
            .map(|j| j.info.clone());
        atl_diag::answered(INTERFACE, "getPendingJob");
        Ok(job)
    }
}

/// JobServiceContext's ServiceConnection for one run.
struct JobConnection {
    inner: Weak<Inner>,
    generation: u64,
}

impl Interface for JobConnection {}

impl IServiceConnection for JobConnection {
    fn r#connected(
        &self,
        _name: Option<&ComponentName>,
        service: Option<&SpIBinder>,
        dead: bool,
    ) -> binder::Result<()> {
        if let Some(inner) = self.inner.upgrade() {
            inner.connected(self.generation, service, dead);
        }
        Ok(())
    }
}

/// JobServiceContext.JobCallback: the IJobCallback of one run. A call from a run that has ended finds no run and is ignored (verifyCallerLocked), as AOSP ignores a stale callback.
struct JobCallback {
    inner: Weak<Inner>,
    generation: u64,
}

impl Interface for JobCallback {}

impl JobCallback {
    fn inner(&self) -> Option<Arc<Inner>> {
        self.inner.upgrade()
    }
}

impl IJobCallback for JobCallback {
    /// doAcknowledgeStartMessage -> handleStartedLocked: onStartJob returned; false finishes the job.
    fn r#acknowledgeStartMessage(&self, _job_id: i32, ongoing: bool) -> binder::Result<()> {
        let Some(inner) = self.inner() else {
            return Ok(());
        };
        let done = {
            let mut st = inner.lock();
            let Some(i) = st.run(self.generation) else {
                return Ok(());
            };
            if st.runs[i].verb != Verb::Starting {
                return Ok(());
            }
            st.runs[i].verb = Verb::Executing;
            !ongoing
        };
        atl_diag::answered(INTERFACE, "acknowledgeStartMessage");
        if done {
            inner.finish(self.generation, false);
        }
        Ok(())
    }

    /// doAcknowledgeStopMessage: the run ends (no stopJob is ever sent, so this only follows an app's own call).
    fn r#acknowledgeStopMessage(&self, _job_id: i32, reschedule: bool) -> binder::Result<()> {
        if let Some(inner) = self.inner() {
            inner.finish(self.generation, reschedule);
        }
        Ok(())
    }

    /// doDequeueWork: the next pending item, moved to executing; none left and none executing finishes the job ("last work dequeued").
    fn r#dequeueWork(&self, _job_id: i32) -> binder::Result<Option<JobWorkItem>> {
        let Some(inner) = self.inner() else {
            return Ok(None);
        };
        let (work, finish) = {
            let mut st = inner.lock();
            let Some(i) = st.run(self.generation) else {
                return Ok(None);
            };
            if st.runs[i].verb == Verb::Stopping {
                // doDequeueWork: a job on its way out dispatches no more work
                return Ok(None);
            }
            let (uid, ns, id) = (
                st.runs[i].uid,
                st.runs[i].namespace.clone(),
                st.runs[i].job_id,
            );
            match st.table.dequeue(uid, ns.as_deref(), id) {
                Some(w) => {
                    st.runs[i].executing.push(w.clone());
                    (Some(w), false)
                }
                None => (None, st.runs[i].executing.is_empty()),
            }
        };
        atl_diag::answered(INTERFACE, "dequeueWork");
        if finish {
            inner.finish(self.generation, false);
        }
        Ok(work)
    }

    /// doCompleteWork: true when the item was executing; a stale run answers true (JobServiceContext's comment: so JobParameters.completeWork does not throw).
    fn r#completeWork(&self, _job_id: i32, work_id: i32) -> binder::Result<bool> {
        let Some(inner) = self.inner() else {
            return Ok(true);
        };
        let mut st = inner.lock();
        let Some(i) = st.run(self.generation) else {
            return Ok(true);
        };
        let executing = &mut st.runs[i].executing;
        let found = executing
            .iter()
            .position(|w| w.work_id == work_id)
            .map(|at| executing.remove(at))
            .is_some();
        atl_diag::answered(INTERFACE, "completeWork");
        Ok(found)
    }

    /// doJobFinished.
    fn r#jobFinished(&self, _job_id: i32, reschedule: bool) -> binder::Result<()> {
        if let Some(inner) = self.inner() {
            atl_diag::answered(INTERFACE, "jobFinished");
            inner.finish(self.generation, reschedule);
        }
        Ok(())
    }
}

/// The service's binder, for `addService`. `runner`: the activity service that binds job services; None is the test-only break `--no-job-runner` (jobs stored, never run).
pub fn new_binder(
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    runner: Option<ActivityManager>,
) -> SpIBinder {
    BnJobScheduler::new_binder(
        JobSchedulerService(Arc::new_cyclic(|me| Inner {
            packages,
            permissions,
            state: Mutex::new(State::default()),
            runner,
            me: me.clone(),
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

    /// JobIntentService's run: two items dequeued in order with delivery count 1 (JobStatus.dequeueWorkLocked bumps it from 0), then none; the finished one-off job leaves the table.
    #[test]
    fn dequeue_then_finish() {
        let mut t = JobTable::default();
        t.schedule(10, None, info(7, 0), Some(item()), same);
        t.schedule(10, None, info(7, 0), Some(item()), same);
        let a = t.dequeue(10, None, 7).expect("first");
        let b = t.dequeue(10, None, 7).expect("second");
        assert!(a.work_id < b.work_id);
        assert_eq!((a.delivery_count, b.delivery_count), (1, 1));
        assert!(t.dequeue(10, None, 7).is_none());
        assert_eq!(t.finished((10, None, 7), Vec::new(), false), Finished::Done);
        assert!(t.get(10, None, 7).is_none());
        assert_eq!(t.finished((10, None, 7), Vec::new(), false), Finished::Gone);
    }

    /// Uncompleted items go back to the front and the job runs again; rescheduled and periodic jobs stay without running.
    #[test]
    fn finish_keeps_unfinished_work() {
        let mut t = JobTable::default();
        t.schedule(10, None, info(7, 0), Some(item()), same);
        t.schedule(10, None, info(7, 0), Some(item()), same);
        let a = t.dequeue(10, None, 7).expect("first");
        assert_eq!(
            t.finished((10, None, 7), vec![a.clone()], false),
            Finished::RunAgain
        );
        assert_eq!(t.get(10, None, 7).expect("job").work[0].work_id, a.work_id);
        assert_eq!(
            t.finished((10, None, 7), Vec::new(), true),
            Finished::KeptRescheduled
        );
        let mut periodic = info(8, 0);
        periodic.is_periodic = true;
        t.schedule(10, None, periodic, None, same);
        assert_eq!(
            t.finished((10, None, 8), Vec::new(), false),
            Finished::KeptPeriodic
        );
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
