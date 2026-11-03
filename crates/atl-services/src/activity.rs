// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java, frameworks/base/services/core/java/com/android/server/am/ProcessList.java, frameworks/base/services/core/java/com/android/server/am/InstrumentationReporter.java, frameworks/base/services/core/java/com/android/server/am/ContentProviderHelper.java, frameworks/base/services/core/java/com/android/server/am/ContentProviderRecord.java, frameworks/base/services/core/java/com/android/server/wm/ActivityTaskSupervisor.java, frameworks/base/services/core/java/com/android/server/wm/ActivityClientController.java
//! IActivityManager + the ActivityTaskManager launch path + IActivityClientController, subset: a process the services started attaches (attachApplication -> bindApplication), finishes attaching
//! (finishAttachApplication -> scheduleTransaction([LaunchActivityItem, ResumeActivityItem]) of its launcher activity), asks for the settings provider (getContentProvider) and its display (getDisplayId);
//! instrumentation runs (startInstrumentation, addInstrumentationResults, finishInstrumentation, a crash or kill of the instrumented process), and the broadcast calls a test runner makes
//! (broadcastIntentWithFeature, registerReceiverWithFeature: validated as AOSP does, then nothing is delivered).
//! Processes are started and killed through [`ProcessControl`] (the zygote / Process.killProcess seam); every other method is counted unimplemented (counters).
//! Locking: one state lock (AMS's `this` + mProcLock). Nothing blocking runs under it but the control socket's start request (as AOSP starts a process under the lock); a kill, and every call into a watcher
//! (the reporter thread: AOSP posts them for the same reason, "can't call out of the system process with a lock held"), happens after the lock is released.

use std::io::Write;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::app::IActivityClientController::{
    BnActivityClientController, IActivityClientController,
};
use atl_aidl::aidl::android::app::IActivityManager::{EnforcePermission, IActivityManager};
use atl_aidl::aidl::android::app::IApplicationThread::IApplicationThread;
use atl_aidl::aidl::android::app::IInstrumentationWatcher::IInstrumentationWatcher;
use atl_aidl::aidl::android::app::IUiAutomationConnection::IUiAutomationConnection;
use atl_aidl::aidl::android::content::IIntentReceiver::IIntentReceiver;
use atl_broker::control::StartRequest;
use atl_parcelables::android::app::ContentProviderHolder;
use atl_parcelables::android::app::servertransaction::ClientTransaction;
use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, ProviderInfo, ProviderInfoList,
};
use atl_parcelables::android::content::res::{CompatibilityInfo, Configuration};
use atl_parcelables::android::content::{ComponentName, Intent, IntentFilter};
use atl_parcelables::android::os::Bundle;
use atl_parcelables::codecs::android::app::servertransaction::ClientTransaction::ClientTransactionItem;
use atl_parcelables::java::io::FileDescriptor;
use atl_parcelables::parts::{
    ActivityWindowInfo, ComponentInfo, LaunchActivityItem, PackageItemInfo, ResumeActivityItem,
};
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, SpIBinder, Status, Strong,
    ThreadState,
};

use crate::packages::{ActivityDecl, PackageRecord, Packages};
use crate::permission::{self, ROOT_UID, SHELL_UID, SYSTEM_UID};
use crate::process_control::ProcessControl;
use crate::shared_memory::ApplicationSharedMemory;
use crate::{counters, manifest_flags, package, settings, token};

const AM: &str = "IActivityManager";
const ACC: &str = "IActivityClientController";

/// Display.DEFAULT_DISPLAY.
const DEFAULT_DISPLAY: i32 = 0;
/// Context.DEVICE_ID_DEFAULT.
const DEVICE_ID_DEFAULT: i32 = 0;
/// ActivityManager.PROCESS_STATE_TOP: the reported state of the process whose activity is launched in front.
const PROCESS_STATE_TOP: i32 = 2;
/// ActivityManager.PROCESS_STATE_UNKNOWN (ProcessStateEnum.UNKNOWN, -1): the procState of ResumeActivityItem's three-argument constructor, which realStartActivityLocked uses.
const PROCESS_STATE_UNKNOWN: i32 = -1;
/// Build.UNKNOWN: the serial apps targeting P+ get (ActivityManagerService attachApplicationLocked buildSerial).
const BUILD_UNKNOWN: &str = "unknown";
/// Intent.ACTION_MAIN / CATEGORY_LAUNCHER: how the launcher starts an app.
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";
/// Intent.FLAG_ACTIVITY_NEW_TASK, set on a launcher start.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
/// Intent.FLAG_RECEIVER_BOOT_UPGRADE (Intent.java:7647): refused in a broadcast from an app.
const FLAG_RECEIVER_BOOT_UPGRADE: i32 = 0x0200_0000;
/// ActivityManager.BROADCAST_SUCCESS (ActivityManager.java:639).
const BROADCAST_SUCCESS: i32 = 0;
/// The locale of the global configuration until the platform layer supplies the desktop's: what Android uses when neither `persist.sys.locale` nor `ro.product.locale` is set (LocaleList's default,
/// `Locale.US`). A started app needs at least one: ActivityThread.handleBindApplication calls `LocaleList.setDefault(config.getLocales())`, which throws on an empty list.
const DEFAULT_LOCALE: &str = "en-US";
/// ProcessList.startProcessLocked's entryPoint (ProcessList.java:2093): every app process runs ActivityThread.main.
const ACTIVITY_THREAD: &str = "android.app.ActivityThread";
/// ActivityThread.PROC_START_SEQ_IDENT: the argument prefix that carries the startSeq to ActivityThread.main.
const PROC_START_SEQ_IDENT: &str = "seq=";
/// Activity.RESULT_CANCELED: what finishInstrumentation reports for a crashed process (ActivityManagerService.java:3365).
const RESULT_CANCELED: i32 = 0;
/// Instrumentation.REPORT_KEY_IDENTIFIER and the status code of a failed start (reportStartInstrumentationFailureLocked: `Instrumentation.REPORT_KEY_IDENTIFIER`, status -1).
const REPORT_KEY_IDENTIFIER: &str = "id";
const START_FAILURE_STATUS: i32 = -1;
/// ActivityManager.INSTR_FLAG_* (ActivityManager.java:173-206). The hidden-API and test-API flags only relax ART's hidden API enforcement, which the launcher does not turn on.
const INSTR_FLAG_DISABLE_HIDDEN_API_CHECKS: i32 = 1 << 0;
const INSTR_FLAG_DISABLE_ISOLATED_STORAGE: i32 = 1 << 1;
const INSTR_FLAG_DISABLE_TEST_API_CHECKS: i32 = 1 << 2;
const INSTR_FLAG_NO_RESTART: i32 = 1 << 3;
const INSTR_FLAG_ALWAYS_CHECK_SIGNATURE: i32 = 1 << 4;
/// UserHandle.USER_CURRENT / USER_SYSTEM: `am instrument` sends USER_CURRENT, which UserController.handleIncomingUser resolves to the current user, always 0 here.
const USER_CURRENT: i32 = -2;
const USER_SYSTEM: i32 = 0;

/// The global configuration a started app gets (ActivityTaskManagerService's `getGlobalConfiguration`), reduced to what the services know: font scale 1 (the `font_scale` setting's default) and the locale.
/// Screen size, density and the input fields stay undefined (0) until the display and input come from the platform layer.
fn global_configuration() -> Configuration {
    Configuration {
        font_scale: 1.0,
        locale_list: Some(DEFAULT_LOCALE.to_owned()),
        ..Configuration::default()
    }
}

/// ActivityTaskSupervisor.java:1034: the ResumeActivityItem realStartActivityLocked schedules after the launch (andResume), with the same token as the launch.
/// isForward is the launch's (the transition is forward), no compat fake focus.
fn resume_item(token: SpIBinder) -> ResumeActivityItem {
    ResumeActivityItem {
        activity_token: Some(token),
        proc_state: PROCESS_STATE_UNKNOWN,
        is_forward: true,
        should_send_compat_fake_focus: false,
    }
}

/// The settings provider (packages/SettingsProvider/AndroidManifest.xml): authority, package, class, process, initOrder; it runs as the system uid.
const SETTINGS_AUTHORITY: &str = "settings";
const SETTINGS_PACKAGE: &str = "com.android.providers.settings";
const SETTINGS_CLASS: &str = "com.android.providers.settings.SettingsProvider";
const SETTINGS_PROCESS: &str = "system";
const SETTINGS_INIT_ORDER: i32 = 100;

/// ActiveInstrumentation (frameworks/base/services/core/java/com/android/server/am/ActiveInstrumentation.java), reduced: one target process, no profiler, no SDK sandbox.
struct ActiveInstrumentation {
    /// mClass (= mResultClass): the Instrumentation class's component.
    class: ComponentName,
    /// mTargetInfo's package: the package whose code the process runs.
    target_package: String,
    arguments: Option<Bundle>,
    watcher: Option<Strong<dyn IInstrumentationWatcher>>,
    ui_connection: Option<Strong<dyn IUiAutomationConnection>>,
    /// mCurResults: what addInstrumentationResults gathered.
    cur_results: Option<Bundle>,
    no_restart: bool,
}

/// A process the services started (ProcessRecord, reduced); its `thread` is set once it attached.
struct ProcessRecord {
    package: String,
    process_name: String,
    uid: u32,
    /// ProcessRecord.getStartSeq
    start_seq: i64,
    /// the binderd tag [`ProcessControl::start`] returned: the calling pid of this process's transactions
    pid: i32,
    attached: Option<Attached>,
    /// the activity ActivityTaskManager launches once the process has attached (ActivityTaskSupervisor.realStartActivityLocked)
    launch_activity: bool,
    /// ProcessRecord.getActiveInstrumentation
    instr: Option<ActiveInstrumentation>,
}

/// The thread of an attached process and the death link on its binder (AppDeathRecipient, ActivityManagerService.java:4600); the recipient is only held so the link outlives the attach call.
struct Attached {
    thread: Strong<dyn IApplicationThread>,
    _death: DeathRecipient,
}

#[derive(Default)]
struct State {
    processes: Vec<ProcessRecord>,
    /// ProcessList.mProcStartSeqCounter: counts from 0, pre-incremented.
    proc_start_seq_counter: i64,
    /// System.identityHashCode(r) stand-in for LaunchActivityItem.ident
    next_ident: i32,
    /// ActivityManagerService.mApplicationSharedMemoryReadOnlyFd's region
    shared_memory: Option<ApplicationSharedMemory>,
}

/// InstrumentationReporter.Report: a status or the final report for a watcher.
enum Report {
    Status {
        watcher: Strong<dyn IInstrumentationWatcher>,
        name: ComponentName,
        code: i32,
        results: Option<Bundle>,
    },
    Finished {
        watcher: Strong<dyn IInstrumentationWatcher>,
        name: ComponentName,
        code: i32,
        results: Option<Bundle>,
    },
}

/// InstrumentationReporter: delivers reports to watchers on its own thread, in order, so no call into a watcher (a blocking, non-oneway binder call into another process) is made under the state lock.
/// AOSP's thread starts on demand and ends after ten idle seconds; this one lives as long as the services.
struct Reporter {
    queue: Mutex<Sender<Report>>,
}

impl Reporter {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Report>();
        std::thread::Builder::new()
            .name("InstrumentationReporter".into())
            .spawn(move || {
                for report in rx {
                    let (what, result) = match &report {
                        Report::Status {
                            watcher,
                            name,
                            code,
                            results,
                        } => (
                            "status",
                            watcher.r#instrumentationStatus(Some(name), *code, results.as_ref()),
                        ),
                        Report::Finished {
                            watcher,
                            name,
                            code,
                            results,
                        } => (
                            "finished",
                            watcher.r#instrumentationFinished(Some(name), *code, results.as_ref()),
                        ),
                    };
                    if let Err(e) = result {
                        log(format_args!(
                            "Failure reporting {what} to instrumentation watcher: {e}"
                        ));
                    }
                }
            })
            .expect("starting the InstrumentationReporter thread");
        Reporter {
            queue: Mutex::new(tx),
        }
    }

    fn report(&self, report: Report) {
        let queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        // the receiver ends with the process; a failed send means shutdown
        let _ = queue.send(report);
    }
}

struct Inner {
    packages: Arc<Packages>,
    control: Arc<dyn ProcessControl>,
    state: Mutex<State>,
    reporter: Reporter,
    controller: SpIBinder,
    settings: SpIBinder,
}

/// The service; a clone shares it (one is registered with servicemanager, another starts the launcher package).
#[derive(Clone)]
pub struct ActivityManager {
    inner: Arc<Inner>,
}

fn log(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "services: {msg}");
}

fn security(msg: &str) -> Status {
    Status::new_exception_str(ExceptionCode::SECURITY, Some(msg))
}

fn illegal_state(msg: &str) -> Status {
    Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(msg))
}

fn illegal_argument(msg: &str) -> Status {
    Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(msg))
}

impl ActivityManager {
    pub fn new(packages: Arc<Packages>, control: Arc<dyn ProcessControl>) -> Self {
        let controller = BnActivityClientController::new_binder(
            ActivityClientController,
            BinderFeatures::default(),
        )
        .as_binder();
        ActivityManager {
            inner: Arc::new(Inner {
                packages,
                control,
                state: Mutex::new(State::default()),
                reporter: Reporter::new(),
                controller,
                settings: settings::new_binder(),
            }),
        }
    }

    /// Starts the process of `package`'s launcher activity, which launches once the process has attached: what ActivityTaskManager does for a MAIN / LAUNCHER start (startActivity is not
    /// implemented, so atl-system-services calls this for its `--launch` package, the stand-in for the home screen's tap).
    pub fn start_launcher(&self, package: &str) -> Result<(), String> {
        let p = self
            .inner
            .packages
            .by_name(package)
            .ok_or_else(|| format!("no package {package}"))?;
        if p.launch_activity().is_none() {
            return Err(format!("{package} has no launcher activity"));
        }
        let mut st = self.inner.state();
        self.inner
            .start_process_locked(&mut st, p, p.process_name(), true, None)
            .map_err(|e| format!("starting {package}: {e}"))
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Kills processes by pid, after the state lock was released (AOSP's Process.killProcess is a local syscall; here it is a request to atld).
    fn kill(&self, pids: &[i32]) {
        for &pid in pids {
            if let Err(e) = self.control.kill(pid) {
                log(format_args!("killing pid {pid}: {e}"));
            }
        }
    }

    /// ProcessList.startProcessLocked (ProcessList.java:2093-2168) + Process.start: a new startSeq, the pending record, the start request. The process is a record at once (mPendingStarts + mPidsSelfLocked).
    fn start_process_locked(
        &self,
        st: &mut State,
        p: &PackageRecord,
        process_name: &str,
        launch_activity: bool,
        instr: Option<ActiveInstrumentation>,
    ) -> std::io::Result<()> {
        st.proc_start_seq_counter += 1;
        let start_seq = st.proc_start_seq_counter;
        let pid = self.control.start(&StartRequest {
            uid: p.uid,
            process_name: process_name.to_owned(),
            package: p.name.clone(),
            target_sdk: p.target_sdk,
            entry_point: ACTIVITY_THREAD.to_owned(),
            args: vec![format!("{PROC_START_SEQ_IDENT}{start_seq}")],
        })?;
        st.processes.push(ProcessRecord {
            package: p.name.clone(),
            process_name: process_name.to_owned(),
            uid: p.uid,
            start_seq,
            pid,
            attached: None,
            launch_activity,
            instr,
        });
        Ok(())
    }

    /// realStartActivityLocked (ActivityTaskSupervisor.java:1021-1029): the LaunchActivityItem of the package's launcher activity, under `token` (the one its ResumeActivityItem carries).
    /// ActivityInfo.flags are the activity's manifest flags (manifest_flags). Not sent yet (slice 7 needs them): results, new intents, saved state; the assist / shareable / fragment tokens stay null.
    fn launch_item(
        &self,
        p: &PackageRecord,
        activity: &ActivityDecl,
        token: SpIBinder,
        ident: i32,
    ) -> LaunchActivityItem {
        let ai = package::application_info(p, &self.packages);
        let intent = Intent {
            action: Some(ACTION_MAIN.into()),
            categories: vec![Some(CATEGORY_LAUNCHER.into())],
            flags: FLAG_ACTIVITY_NEW_TASK,
            component: Some(ComponentName {
                package: p.name.clone(),
                class: activity.name.clone(),
            }),
            ..Default::default()
        };
        let info = ActivityInfo {
            component: ComponentInfo {
                item: PackageItemInfo {
                    name: Some(activity.name.clone()),
                    package_name: Some(p.name.clone()),
                    ..Default::default()
                },
                application_info: ai,
                process_name: Some(p.process_name().to_owned()),
                enabled: true,
                exported: true,
                ..Default::default()
            },
            flags: manifest_flags::activity_flags(p, activity),
            ..Default::default()
        };
        LaunchActivityItem {
            activity_token: Some(token),
            ident,
            cur_config: Some(global_configuration()),
            override_config: Some(Configuration::default()),
            device_id: DEVICE_ID_DEFAULT,
            proc_state: PROCESS_STATE_TOP,
            is_forward: true,
            activity_client_controller: Some(self.controller.clone()),
            activity_window_info: Some(ActivityWindowInfo::default()),
            intent: Some(intent),
            info: Some(info),
            ..Default::default()
        }
    }

    /// ContentProviderHelper.getContentProviderImpl's result for the settings provider: ContentProviderRecord.newHolder (ContentProviderRecord.java:89-96) with noReleaseNeeded
    /// (:75, the provider runs as the system uid) and no connection object.
    fn settings_holder(&self) -> ContentProviderHolder {
        let app = ApplicationInfo {
            item: PackageItemInfo {
                package_name: Some(SETTINGS_PACKAGE.into()),
                ..Default::default()
            },
            process_name: Some(SETTINGS_PROCESS.into()),
            uid: permission::SYSTEM_UID as i32,
            enabled: true,
            ..Default::default()
        };
        ContentProviderHolder {
            info: ProviderInfo {
                component: ComponentInfo {
                    item: PackageItemInfo {
                        name: Some(SETTINGS_CLASS.into()),
                        package_name: Some(SETTINGS_PACKAGE.into()),
                        ..Default::default()
                    },
                    application_info: app,
                    process_name: Some(SETTINGS_PROCESS.into()),
                    enabled: true,
                    exported: true,
                    ..Default::default()
                },
                authority: Some(SETTINGS_AUTHORITY.into()),
                multiprocess: false,
                init_order: SETTINGS_INIT_ORDER,
                ..Default::default()
            },
            provider: Some(self.settings.clone()),
            connection: None,
            no_release_needed: true,
            local: false,
        }
    }

    /// ActivityManagerService.reportStartInstrumentationFailureLocked (:14804-14815): written to the log, and sent to the watcher as status -1 with the id and the error.
    fn report_start_failure(
        &self,
        watcher: Option<&Strong<dyn IInstrumentationWatcher>>,
        class: &ComponentName,
        report: &str,
    ) {
        log(format_args!("{report}"));
        if let Some(watcher) = watcher {
            self.reporter.report(Report::Status {
                watcher: watcher.clone(),
                name: class.clone(),
                code: START_FAILURE_STATUS,
                results: Some(Bundle::from_strings(&[
                    (REPORT_KEY_IDENTIFIER, "ActivityManagerService"),
                    ("Error", report),
                ])),
            });
        }
    }

    /// forceStopPackageLocked (as finishInstrumentationLocked and startInstrumentation call it): the package's processes leave the table; their pids are to be killed once the lock is
    /// released. A removed process's instrumentation, if any, ends as a crash would end it (handleAppDiedLocked).
    fn force_stop_package_locked(&self, st: &mut State, package: &str) -> Vec<i32> {
        let (stopped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut st.processes)
            .into_iter()
            .partition(|r| r.package == package);
        st.processes = kept;
        let mut pids = Vec::new();
        for mut rec in stopped {
            pids.push(rec.pid);
            if let Some(instr) = rec.instr.take() {
                // the process is being killed with its package: the same CANCELED a crash reports (no further force stop: the package's processes are all gone)
                self.report_finished(instr, RESULT_CANCELED, Some(crash_info()));
            }
        }
        pids
    }

    /// The reporting half of finishInstrumentationLocked (:14854-14915): the final results are what addInstrumentationResults gathered, over-written with `results` (:14867-14878), reported to the
    /// watcher. The UiAutomation connection's shutdown and the app-op reset (:14883-14897) are not modelled: there are no app ops and no UiAutomation service.
    fn report_finished(&self, instr: ActiveInstrumentation, code: i32, results: Option<Bundle>) {
        let Some(watcher) = instr.watcher else {
            return;
        };
        let final_results = match instr.cur_results {
            Some(mut cur) => {
                if let Some(r) = &results
                    && let Err(e) = cur.put_all(r)
                {
                    log(format_args!("instrumentation results not merged: {e:?}"));
                }
                Some(cur)
            }
            None => results,
        };
        self.reporter.report(Report::Finished {
            watcher,
            name: instr.class,
            code,
            results: final_results,
        });
    }

    /// finishInstrumentationLocked: reports, then (not for no-restart) force-stops the package; the pids to kill.
    fn finish_instrumentation_locked(
        &self,
        st: &mut State,
        pid: i32,
        code: i32,
        results: Option<Bundle>,
    ) -> Vec<i32> {
        let Some(rec) = st.processes.iter_mut().find(|r| r.pid == pid) else {
            return Vec::new();
        };
        let Some(instr) = rec.instr.take() else {
            log(format_args!(
                "finishInstrumentation called on non-instrumented: pid {pid}"
            ));
            return Vec::new();
        };
        let (no_restart, package) = (instr.no_restart, rec.package.clone());
        self.report_finished(instr, code, results);
        if no_restart {
            return Vec::new();
        }
        self.force_stop_package_locked(st, &package)
    }

    /// AppDeathRecipient.binderDied -> appDiedLocked -> handleAppDiedLocked (:3347-3367): the record leaves the table, and an instrumentation it ran ends CANCELED with "Process crashed.".
    fn app_died(&self, pid: i32) {
        let pids = {
            let mut st = self.state();
            let Some(i) = st
                .processes
                .iter()
                .position(|r| r.pid == pid && r.attached.is_some())
            else {
                return;
            };
            let mut rec = st.processes.remove(i);
            log(format_args!(
                "process {} (pid {pid}) died",
                rec.process_name
            ));
            match rec.instr.take() {
                Some(instr) => {
                    log(format_args!(
                        "Crash of app {} running instrumentation {}/{}",
                        rec.process_name, instr.class.package, instr.class.class
                    ));
                    let no_restart = instr.no_restart;
                    self.report_finished(instr, RESULT_CANCELED, Some(crash_info()));
                    if no_restart {
                        Vec::new()
                    } else {
                        self.force_stop_package_locked(&mut st, &rec.package)
                    }
                }
                None => Vec::new(),
            }
        };
        self.kill(&pids);
    }

    /// getRecordForAppLOSP (:3369): the attached process whose IApplicationThread is `thread`.
    fn record_for_thread(st: &State, thread: &Strong<dyn IApplicationThread>) -> Option<i32> {
        let binder = thread.as_binder();
        st.processes
            .iter()
            .find(|r| {
                r.attached
                    .as_ref()
                    .is_some_and(|a| a.thread.as_binder() == binder)
            })
            .map(|r| r.pid)
    }
}

/// handleAppDiedLocked's info Bundle (:3362-3364): the crash report of an instrumented process.
fn crash_info() -> Bundle {
    Bundle::from_strings(&[("shortMsg", "Process crashed.")])
}

impl Interface for ActivityManager {}

impl IActivityManager for ActivityManager {
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let perms: &[&str] = match &permission {
            EnforcePermission::Single(p) => std::slice::from_ref(p),
            EnforcePermission::AllOf(p) | EnforcePermission::AnyOf(p) => p,
        };
        if permission::holds(ThreadState::get_calling_uid(), perms) {
            return Ok(());
        }
        Err(security(&format!("{method}: caller lacks {permission:?}")))
    }

    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(AM, method)
    }

    /// ActivityManagerService.attachApplication:4852 -> attachApplicationLocked:4517: a null thread is a SecurityException; the app is the process this service started (:4540-4563: found by
    /// pid, whose uid and startSeq must match, else the pending start of that startSeq and uid); with none the process is dropped (:4566-4580: killed). Then bindApplication (:4754) of its package,
    /// with the instrumentation, if one is to run in it (:4682-4761).
    fn r#attachApplication(
        &self,
        thread: Option<&Strong<dyn IApplicationThread>>,
        start_seq: i64,
    ) -> binder::Result<()> {
        let Some(thread) = thread else {
            return Err(security("Invalid application interface"));
        };
        let inner = &self.inner;
        let pid = ThreadState::get_calling_pid();
        let uid = ThreadState::get_calling_uid();
        let mut st = inner.state();
        let Some(i) = st.processes.iter().position(|r| {
            r.attached.is_none()
                && start_seq > 0
                && r.start_seq == start_seq
                && r.uid == uid
                && r.pid == pid
        }) else {
            log(format_args!(
                "No pending application record for pid {pid} (uid {uid}, startSeq {start_seq}); dropping process"
            ));
            drop(st);
            // AOSP kills it quietly (killProcessQuiet): the control refuses a pid it did not start
            inner.kill(&[pid]);
            return Ok(());
        };
        let Some(p) = inner.packages.by_name(&st.processes[i].package) else {
            log(format_args!(
                "pending start {start_seq}: no package {}",
                st.processes[i].package
            ));
            return Ok(());
        };
        // ActivityManagerService's mApplicationSharedMemoryReadOnlyFd: one region for every app, made when the first one attaches
        let memory_error = |e: &dyn std::fmt::Display| {
            Status::new_exception_str(
                ExceptionCode::ILLEGAL_STATE,
                Some(format!("ApplicationSharedMemory: {e}")),
            )
        };
        if st.shared_memory.is_none() {
            st.shared_memory =
                Some(ApplicationSharedMemory::create().map_err(|e| memory_error(&e))?);
        }
        let app_memory = st
            .shared_memory
            .as_ref()
            .map(|m| m.read_only.try_clone())
            .transpose()
            .map_err(|e| memory_error(&e))?
            .map(FileDescriptor::new);
        let process_name = st.processes[i].process_name.clone();
        // :4690 `appInfo = instr != null ? instr.mTargetInfo : app.info`
        let instr = st.processes[i].instr.as_ref();
        let info = match instr {
            Some(instr) => inner
                .packages
                .by_name(&instr.target_package)
                .map(|t| package::application_info(t, &inner.packages))
                .ok_or_else(|| illegal_state("instrumentation target vanished"))?,
            None => package::application_info(p, &inner.packages),
        };
        // AppDeathRecipient (:4600): the record leaves the table when the process's binder dies
        let weak = Arc::downgrade(inner);
        let mut death = DeathRecipient::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.app_died(pid);
            }
        });
        thread
            .as_binder()
            .link_to_death(&mut death)
            .map_err(|e| illegal_state(&format!("linkToDeath: {e:?}")))?;
        let core_settings = Bundle::default();
        // what attachApplicationLocked passes for a plain app start (no profiler, backup or sdk sandbox): nulls, the global configuration, the default compat info,
        // an empty provider list (no providers of its own yet), no disabled compat changes; coreSettings an empty Bundle, never null (attachApplicationLocked passes
        // mCoreSettingsObserver.getCoreSettingsLocked(), and ActivityThread.updateDebugViewAttributeState reads it unconditionally); no settings are mirrored into it yet.
        // An instrumentation passes its class, arguments, watcher and UiAutomation connection (:4723-4737).
        let (test_name, test_arguments, test_watcher, test_ui) = match instr {
            Some(i) => (
                Some(&i.class),
                i.arguments.as_ref(),
                i.watcher.as_ref(),
                i.ui_connection.as_ref(),
            ),
            None => (None, None, None, None),
        };
        thread.r#bindApplication(
            Some(&process_name),
            Some(&info),
            None,
            None,
            false,
            Some(&ProviderInfoList::default()),
            test_name,
            None,
            test_arguments,
            test_watcher,
            test_ui,
            0,
            false,
            false,
            false,
            false,
            Some(&global_configuration()),
            Some(&CompatibilityInfo::default()),
            None,
            Some(&core_settings),
            Some(BUILD_UNKNOWN),
            None,
            None,
            Some(&[]),
            Some(&[]),
            None,
            app_memory.as_ref(),
            0,
            0,
        )?;
        st.processes[i].attached = Some(Attached {
            thread: thread.clone(),
            _death: death,
        });
        Ok(())
    }

    /// ActivityManagerService.finishAttachApplication:4995 -> finishAttachApplicationInner:4865: the attached app of the caller's pid with the same uid and startSeq (:4874), else
    /// dropped (:4877-4892, killed); then the ActivityTaskManager attach (:4905-4909 -> realStartActivityLocked) schedules the launch transaction of the activity it was started for, if any: LaunchActivityItem, then ResumeActivityItem.
    fn r#finishAttachApplication(&self, start_seq: i64, _on_create_ns: i64) -> binder::Result<()> {
        let inner = &self.inner;
        let pid = ThreadState::get_calling_pid();
        let uid = ThreadState::get_calling_uid();
        let mut st = inner.state();
        let Some(rec) = st.processes.iter().find(|r| {
            r.pid == pid && r.uid == uid && r.start_seq == start_seq && r.attached.is_some()
        }) else {
            log(format_args!(
                "Mismatched or missing ProcessRecord: pid {pid}, uid {uid}, startSeq {start_seq}"
            ));
            drop(st);
            inner.kill(&[pid]);
            return Ok(());
        };
        let (package, launch) = (rec.package.clone(), rec.launch_activity);
        let thread = rec.attached.as_ref().map(|a| a.thread.clone());
        let (Some(thread), true) = (thread, launch) else {
            return Ok(());
        };
        st.next_ident += 1;
        let ident = st.next_ident;
        drop(st);
        let Some(p) = inner.packages.by_name(&package) else {
            return Ok(());
        };
        let Some(activity) = p.launch_activity() else {
            return Ok(());
        };
        // one token for both items: the app keys its activity record by it
        let token = token::new();
        let transaction = ClientTransaction {
            items: vec![
                Some(ClientTransactionItem::LaunchActivity(inner.launch_item(
                    p,
                    activity,
                    token.clone(),
                    ident,
                ))),
                Some(ClientTransactionItem::ResumeActivity(resume_item(token))),
            ],
        };
        thread.r#scheduleTransaction(Some(&transaction))
    }

    /// ActivityManagerService.getContentProvider:7074 -> ContentProviderHelper.getContentProvider:134: a null caller is a SecurityException (:137-141), as is a callingPackage not
    /// of the caller's uid (:145-149); then the provider of `name`, or null when none (getContentProviderImpl). M2 knows one provider: settings.
    fn r#getContentProvider(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        calling_package: Option<&str>,
        name: Option<&str>,
        _user: i32,
        _stable: bool,
    ) -> binder::Result<Option<ContentProviderHolder>> {
        if caller.is_none() {
            return Err(security(&format!(
                "null IApplicationThread when getting content provider {name:?}"
            )));
        }
        let uid = ThreadState::get_calling_uid();
        if let Some(pkg) = calling_package {
            let owns = self
                .inner
                .packages
                .by_name(pkg)
                .is_some_and(|p| permission::app_id(p.uid) == permission::app_id(uid));
            if !owns && !permission::is_system_or_root(uid) {
                return Err(security(&format!(
                    "Given calling package {pkg} does not match caller's uid {uid}"
                )));
            }
        }
        Ok((name == Some(SETTINGS_AUTHORITY)).then(|| self.inner.settings_holder()))
    }

    /// ActivityManagerService.startInstrumentation (:14452-14644), user 0 only. The PackageManager lookups (:14472-14500) read the same package table the PackageManager service answers from.
    /// Not modelled, each counted when asked for: no-restart (:14639 instrumentWithoutRestart), the SDK sandbox flags (:14566), a target process list other than the default process
    /// (:14589-14596; the first listed process is started, the instrumentation does not follow into the others). Not enforced: enforceNotIsolatedCaller (:14456, no isolated uids),
    /// the DISABLE_HIDDEN_API_CHECKS permission (:14551-14563: the flags change nothing, ART's hidden API enforcement is off), the app op for DISABLE_ISOLATED_STORAGE (:14633).
    /// Signatures: nothing here verifies APK signatures, so checkSignatures (:14509) is SIGNATURE_MATCH for every pair of known packages and the signature-denial branch (:14511-14535) cannot fire.
    fn r#startInstrumentation(
        &self,
        class_name: Option<&ComponentName>,
        _profile_file: Option<&str>,
        flags: i32,
        arguments: Option<&Bundle>,
        watcher: Option<&Strong<dyn IInstrumentationWatcher>>,
        connection: Option<&Strong<dyn IUiAutomationConnection>>,
        user_id: i32,
        _abi_override: Option<&str>,
    ) -> binder::Result<bool> {
        let inner = &self.inner;
        let Some(class_name) = class_name else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("startInstrumentation: null className"),
            ));
        };
        let calling_uid = ThreadState::get_calling_uid();
        // UserController.handleIncomingUser (:14459): the current user is user 0; no other user exists
        if user_id != USER_CURRENT && user_id != USER_SYSTEM {
            return Err(security(&format!(
                "Call from pid {} to user {user_id}: no such user",
                ThreadState::get_calling_pid()
            )));
        }
        // :14462-14464 refuse possible leaked file descriptors
        if arguments.is_some_and(Bundle::has_file_descriptors) {
            return Err(illegal_argument("File descriptors passed in Bundle"));
        }
        let no_restart = flags & INSTR_FLAG_NO_RESTART != 0;
        if flags & INSTR_FLAG_NO_RESTART != 0 {
            return Err(counters::unimplemented(
                AM,
                "startInstrumentation (INSTR_FLAG_NO_RESTART)",
            ));
        }
        if flags >> 5 != 0 {
            return Err(counters::unimplemented(
                AM,
                "startInstrumentation (INSTR_FLAG_INSTRUMENT_SDK_SANDBOX)",
            ));
        }
        for (flag, what) in [
            (
                INSTR_FLAG_DISABLE_HIDDEN_API_CHECKS,
                "INSTR_FLAG_DISABLE_HIDDEN_API_CHECKS",
            ),
            (
                INSTR_FLAG_DISABLE_TEST_API_CHECKS,
                "INSTR_FLAG_DISABLE_TEST_API_CHECKS",
            ),
            (
                INSTR_FLAG_DISABLE_ISOLATED_STORAGE,
                "INSTR_FLAG_DISABLE_ISOLATED_STORAGE",
            ),
            (
                INSTR_FLAG_ALWAYS_CHECK_SIGNATURE,
                "INSTR_FLAG_ALWAYS_CHECK_SIGNATURE",
            ),
        ] {
            if flags & flag != 0 {
                counters::count(AM, &format!("startInstrumentation ({what} ignored)"));
            }
        }

        let mut st = inner.state();
        // :14472-14486 getInstrumentationInfoAsUser, getApplicationInfo of its target
        let Some((_, decl)) = inner
            .packages
            .instrumentation(&class_name.package, &class_name.class)
        else {
            inner.report_start_failure(
                watcher,
                class_name,
                &format!(
                    "Unable to find instrumentation info for: {}/{}",
                    class_name.package, class_name.class
                ),
            );
            return Ok(false);
        };
        let Some(target) = inner.packages.by_name(&decl.target_package) else {
            inner.report_start_failure(
                watcher,
                class_name,
                &format!(
                    "Unable to find instrumentation target package: {}",
                    decl.target_package
                ),
            );
            return Ok(false);
        };
        // :14513 (BUILD IS_DEBUGGABLE false): only root, shell and the system uid start an instrumentation, else one already running instrumentation does (hasActiveInstrumentationLocked(callingPid))
        let calling_pid = ThreadState::get_calling_pid();
        let from_instrumentation = st
            .processes
            .iter()
            .any(|r| r.pid == calling_pid && r.instr.is_some());
        if ![ROOT_UID, SHELL_UID, SYSTEM_UID].contains(&calling_uid) && !from_instrumentation {
            let msg = format!(
                "Permission Denial: instrumentation test {}/{} from pid={calling_pid}, uid={calling_uid}, pkgName={} not allowed because it's not started from SHELL",
                class_name.package,
                class_name.class,
                inner
                    .packages
                    .by_app_id(permission::app_id(calling_uid))
                    .next()
                    .map_or("null", |p| p.name.as_str())
            );
            inner.report_start_failure(watcher, class_name, &msg);
            return Err(security(&msg));
        }
        // :14583-14596 targetProcesses: null = the application's process, "*" = every process of the package, else a comma list whose first is the one started
        let default_process = match decl.target_processes.as_deref() {
            None => target.process_name().to_owned(),
            Some("*") => {
                counters::count(AM, "startInstrumentation (targetProcesses=*)");
                target.process_name().to_owned()
            }
            Some(list) => {
                let mut names = list.split(',');
                let first = names.next().unwrap_or_default().to_owned();
                if names.next().is_some() {
                    counters::count(AM, "startInstrumentation (more than one target process)");
                }
                first
            }
        };
        let instr = ActiveInstrumentation {
            class: class_name.clone(),
            target_package: target.name.clone(),
            arguments: arguments.cloned(),
            watcher: watcher.cloned(),
            ui_connection: connection.cloned(),
            cur_results: None,
            no_restart,
        };
        // :14613-14622 forceStopPackageLocked of the target, addAppLocked -> startProcessLocked
        let kill = inner.force_stop_package_locked(&mut st, &target.name);
        let started =
            inner.start_process_locked(&mut st, target, &default_process, false, Some(instr));
        drop(st);
        inner.kill(&kill);
        started.map_err(|e| illegal_state(&format!("starting {}: {e}", target.name)))?;
        Ok(true)
    }

    /// ActivityManagerService.addInstrumentationResults (:14831-14850): the results of the instrumentation running in `target`'s process join its current results (:14815-14829, putAll); a process with no
    /// instrumentation is logged and ignored. A results Bundle with a file descriptor is refused first (:14834-14836, IllegalArgumentException).
    fn r#addInstrumentationResults(
        &self,
        target: Option<&Strong<dyn IApplicationThread>>,
        results: Option<&Bundle>,
    ) -> binder::Result<()> {
        if results.is_some_and(Bundle::has_file_descriptors) {
            return Err(illegal_argument("File descriptors passed in Intent"));
        }
        let mut st = self.inner.state();
        let Some(pid) = target.and_then(|t| Inner::record_for_thread(&st, t)) else {
            log(format_args!(
                "addInstrumentationResults: no app for the thread"
            ));
            return Ok(());
        };
        let Some(instr) = st
            .processes
            .iter_mut()
            .find(|r| r.pid == pid)
            .and_then(|r| r.instr.as_mut())
        else {
            log(format_args!(
                "finishInstrumentation called on non-instrumented: pid {pid}"
            ));
            return Ok(());
        };
        if let Some(results) = results {
            match &mut instr.cur_results {
                None => instr.cur_results = Some(results.clone()),
                Some(cur) => cur.put_all(results).map_err(|e| {
                    illegal_state(&format!("instrumentation results not merged: {e:?}"))
                })?,
            }
        }
        Ok(())
    }

    /// ActivityManagerService.finishInstrumentation (:14917-14932) -> finishInstrumentationLocked: the final report goes to the watcher, and the instrumented package is stopped
    /// (killed through the process control) unless it was started with no-restart. Results with a file descriptor are refused first (:14921-14923, IllegalArgumentException).
    fn r#finishInstrumentation(
        &self,
        target: Option<&Strong<dyn IApplicationThread>>,
        result_code: i32,
        results: Option<&Bundle>,
    ) -> binder::Result<()> {
        if results.is_some_and(Bundle::has_file_descriptors) {
            return Err(illegal_argument("File descriptors passed in Intent"));
        }
        let inner = &self.inner;
        let pids = {
            let mut st = inner.state();
            let Some(pid) = target.and_then(|t| Inner::record_for_thread(&st, t)) else {
                log(format_args!("finishInstrumentation: no app for the thread"));
                return Ok(());
            };
            inner.finish_instrumentation_locked(&mut st, pid, result_code, results.cloned())
        };
        inner.kill(&pids);
        Ok(())
    }

    /// ActivityManagerService.broadcastIntentWithFeature:14402 -> BroadcastController.broadcastIntentWithFeature:710-754: the Intent is verified as verifyBroadcastLocked does (:1878-1905: a null
    /// Intent is a NullPointerException (`intent.getFlags()`), a file descriptor in the extras and FLAG_RECEIVER_BOOT_UPGRADE are IllegalArgumentExceptions), then broadcastIntentLocked would queue it for the
    /// receivers. M2 / CTS minimum: no receiver is registered (registerReceiver is not implemented) and no manifest receiver is resolved, so every broadcast reaches nobody and the result is
    /// BROADCAST_SUCCESS, as broadcastIntentLocked returns for a broadcast with no receivers; counted. Not modelled: the isolated-caller check (:715), the permission regimes of the options Bundle (:726),
    /// the permission and app-op filters, sticky storage, the ordered result to `resultTo`.
    fn r#broadcastIntentWithFeature(
        &self,
        _caller: Option<&Strong<dyn IApplicationThread>>,
        _calling_feature_id: Option<&str>,
        intent: Option<&Intent>,
        _resolved_type: Option<&str>,
        _result_to: Option<&Strong<dyn IIntentReceiver>>,
        _result_code: i32,
        _result_data: Option<&str>,
        _result_extras: Option<&Bundle>,
        _required_permissions: Option<&[Option<String>]>,
        _excluded_permissions: Option<&[Option<String>]>,
        _excluded_packages: Option<&[Option<String>]>,
        _app_op: i32,
        _options: Option<&Bundle>,
        _serialized: bool,
        _sticky: bool,
        _user_id: i32,
    ) -> binder::Result<i32> {
        let Some(intent) = intent else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("broadcastIntent: null Intent"),
            ));
        };
        if intent.has_file_descriptors() {
            return Err(illegal_argument("File descriptors passed in Intent"));
        }
        if intent.flags & FLAG_RECEIVER_BOOT_UPGRADE != 0 {
            return Err(illegal_argument(
                "Can't use FLAG_RECEIVER_BOOT_UPGRADE here",
            ));
        }
        counters::count(AM, "broadcastIntentWithFeature (no receivers: dropped)");
        Ok(BROADCAST_SUCCESS)
    }

    /// ActivityManagerService.registerReceiverWithFeature -> BroadcastController.registerReceiverWithFeature:249 -> registerReceiverWithFeatureTraced:296: the caller's process is looked up by its thread
    /// (unknown: logged, null, :312-315); a caller package the process does not run is a SecurityException (:316-320; core uids, below 10000, are exempt: UserHandle.isCore). M2 / CTS minimum, the
    /// counterpart of broadcastIntentWithFeature's: the registration is not kept (no broadcast is delivered to a registered receiver, so none is lost by that) and there is no sticky broadcast to answer
    /// with, so the result is null; counted. The filter was decoded, so a filter with parts the codec does not model was refused before it got here.
    fn r#registerReceiverWithFeature(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        caller_package: Option<&str>,
        _calling_feature_id: Option<&str>,
        _receiver_id: Option<&str>,
        _receiver: Option<&Strong<dyn IIntentReceiver>>,
        _filter: Option<&IntentFilter>,
        _required_permission: Option<&str>,
        _user_id: i32,
        _flags: i32,
    ) -> binder::Result<Option<Intent>> {
        let st = self.inner.state();
        let Some(rec) = caller
            .and_then(|t| Inner::record_for_thread(&st, t))
            .and_then(|pid| st.processes.iter().find(|r| r.pid == pid))
        else {
            log(format_args!(
                "registerReceiverWithFeature: no app for the caller"
            ));
            return Ok(None);
        };
        if permission::app_id(rec.uid) >= permission::FIRST_APPLICATION_UID
            && caller_package != Some(rec.package.as_str())
        {
            return Err(security(&format!(
                "Given caller package {caller_package:?} is not running in process {}",
                rec.process_name
            )));
        }
        counters::count(
            AM,
            "registerReceiverWithFeature (registration not kept: no broadcast is delivered)",
        );
        Ok(None)
    }
}

/// IActivityClientController, handed to the app in LaunchActivityItem (ActivityTaskSupervisor.java:1028 activityClientController).
struct ActivityClientController;

impl Interface for ActivityClientController {}

impl IActivityClientController for ActivityClientController {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(ACC, method)
    }

    /// ActivityClientController.getDisplayId:626-635: the root task's display, DEFAULT_DISPLAY when invalid or when the token has no task. M2 has one display.
    fn r#getDisplayId(&self, _token: Option<&SpIBinder>) -> binder::Result<i32> {
        Ok(DEFAULT_DISPLAY)
    }
}
