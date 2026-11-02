// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java, frameworks/base/services/core/java/com/android/server/am/ContentProviderHelper.java, frameworks/base/services/core/java/com/android/server/am/ContentProviderRecord.java, frameworks/base/services/core/java/com/android/server/wm/ActivityTaskSupervisor.java, frameworks/base/services/core/java/com/android/server/wm/ActivityClientController.java
//! IActivityManager + the ActivityTaskManager launch path + IActivityClientController, M2 subset: an expected process attaches (attachApplication -> bindApplication), finishes
//! attaching (finishAttachApplication -> scheduleTransaction(LaunchActivityItem) of its launcher activity), asks for the settings provider (getContentProvider) and its display (getDisplayId).
//! Every other method is counted unimplemented (counters). No process is killed: the broker owns processes; where AOSP kills, this logs and returns.

use std::io::Write;
use std::sync::{Arc, Mutex};

use atl_aidl::aidl::android::app::IActivityClientController::{
    BnActivityClientController, IActivityClientController,
};
use atl_aidl::aidl::android::app::IActivityManager::{EnforcePermission, IActivityManager};
use atl_aidl::aidl::android::app::IApplicationThread::IApplicationThread;
use atl_parcelables::android::app::ContentProviderHolder;
use atl_parcelables::android::app::servertransaction::ClientTransaction;
use atl_parcelables::android::content::Intent;
use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, ProviderInfo, ProviderInfoList,
};
use atl_parcelables::android::content::res::{CompatibilityInfo, Configuration};
use atl_parcelables::codecs::android::app::servertransaction::ClientTransaction::ClientTransactionItem;
use atl_parcelables::parts::{
    ActivityWindowInfo, ComponentInfo, ComponentName, LaunchActivityItem, PackageItemInfo,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::packages::{PackageRecord, Packages, PendingStart};
use crate::{counters, package, permission, settings, token};

const AM: &str = "IActivityManager";
const ACC: &str = "IActivityClientController";

/// Display.DEFAULT_DISPLAY.
const DEFAULT_DISPLAY: i32 = 0;
/// Context.DEVICE_ID_DEFAULT.
const DEVICE_ID_DEFAULT: i32 = 0;
/// ActivityManager.PROCESS_STATE_TOP: the reported state of the process whose activity is launched in front.
const PROCESS_STATE_TOP: i32 = 2;
/// Build.UNKNOWN: the serial apps targeting P+ get (ActivityManagerService attachApplicationLocked buildSerial).
const BUILD_UNKNOWN: &str = "unknown";
/// Intent.ACTION_MAIN / CATEGORY_LAUNCHER: how the launcher starts an app.
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";
/// Intent.FLAG_ACTIVITY_NEW_TASK, set on a launcher start.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;

/// The settings provider (packages/SettingsProvider/AndroidManifest.xml): authority, package, class, process, initOrder; it runs as the system uid.
const SETTINGS_AUTHORITY: &str = "settings";
const SETTINGS_PACKAGE: &str = "com.android.providers.settings";
const SETTINGS_CLASS: &str = "com.android.providers.settings.SettingsProvider";
const SETTINGS_PROCESS: &str = "system";
const SETTINGS_INIT_ORDER: i32 = 100;

/// A process that attached (ProcessRecord, reduced).
struct App {
    pid: i32,
    uid: u32,
    start_seq: i64,
    package: String,
    thread: Strong<dyn IApplicationThread>,
}

#[derive(Default)]
struct State {
    pending: Vec<PendingStart>,
    apps: Vec<App>,
    /// System.identityHashCode(r) stand-in for LaunchActivityItem.ident
    next_ident: i32,
}

pub struct ActivityManager {
    packages: Arc<Packages>,
    state: Mutex<State>,
    controller: SpIBinder,
    settings: SpIBinder,
}

fn log(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "services: {msg}");
}

fn security(msg: &str) -> Status {
    Status::new_exception_str(ExceptionCode::SECURITY, Some(msg))
}

impl ActivityManager {
    /// The service expecting `pending` starts (the broker's spawns).
    pub fn new(packages: Arc<Packages>, pending: Vec<PendingStart>) -> Self {
        let controller = BnActivityClientController::new_binder(
            ActivityClientController,
            BinderFeatures::default(),
        )
        .as_binder();
        ActivityManager {
            packages,
            state: Mutex::new(State {
                pending,
                ..Default::default()
            }),
            controller,
            settings: settings::new_binder(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, package: &str) -> Option<&PackageRecord> {
        self.packages.by_name(package)
    }

    /// realStartActivityLocked (ActivityTaskSupervisor.java:1021-1030): the LaunchActivityItem of the package's launcher activity. Not sent yet (slice 7 needs them): the lifecycle
    /// ResumeActivityItem, results, new intents, saved state; the assist / shareable / fragment tokens stay null.
    fn launch_item(&self, p: &PackageRecord, ident: i32) -> LaunchActivityItem {
        let ai = package::application_info(p);
        let intent = Intent {
            action: Some(ACTION_MAIN.into()),
            categories: vec![Some(CATEGORY_LAUNCHER.into())],
            flags: FLAG_ACTIVITY_NEW_TASK,
            component: Some(ComponentName {
                package: p.name.clone(),
                class: p.launch_activity.clone(),
            }),
            ..Default::default()
        };
        let info = ActivityInfo {
            component: ComponentInfo {
                item: PackageItemInfo {
                    name: Some(p.launch_activity.clone()),
                    package_name: Some(p.name.clone()),
                    ..Default::default()
                },
                application_info: ai,
                process_name: Some(p.process_name().to_owned()),
                enabled: true,
                exported: true,
                ..Default::default()
            },
            ..Default::default()
        };
        LaunchActivityItem {
            activity_token: Some(token::new()),
            ident,
            cur_config: Some(Configuration::default()),
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

    /// ActivityManagerService.attachApplication:4852 -> attachApplicationLocked:4517: a null thread is a SecurityException; the app is the pending start of `start_seq` whose uid is the
    /// caller's (:4556-4563); with none the process is dropped (:4566-4580: killed in AOSP; here logged, the broker owns it). Then bindApplication (:4754) of its package.
    fn r#attachApplication(
        &self,
        thread: Option<&Strong<dyn IApplicationThread>>,
        start_seq: i64,
    ) -> binder::Result<()> {
        let Some(thread) = thread else {
            return Err(security("Invalid application interface"));
        };
        let pid = ThreadState::get_calling_pid();
        let uid = ThreadState::get_calling_uid();
        let mut st = self.state();
        let Some(i) = st
            .pending
            .iter()
            .position(|s| start_seq > 0 && s.start_seq == start_seq && s.uid == uid)
        else {
            log(format_args!(
                "No pending application record for pid {pid} (uid {uid}, startSeq {start_seq}); dropping process"
            ));
            return Ok(());
        };
        let start = st.pending.remove(i);
        let Some(p) = self.record(&start.package) else {
            log(format_args!(
                "pending start {start_seq}: no package {}",
                start.package
            ));
            return Ok(());
        };
        let info = package::application_info(p);
        // what attachApplicationLocked passes for a plain app start (no instrumentation, profiler, backup or sdk sandbox): nulls, the global configuration, the default compat info,
        // an empty provider list (no providers of its own yet), no disabled compat changes
        thread.r#bindApplication(
            Some(p.process_name()),
            Some(&info),
            None,
            None,
            false,
            Some(&ProviderInfoList::default()),
            None,
            None,
            None,
            None,
            None,
            0,
            false,
            false,
            false,
            false,
            Some(&Configuration::default()),
            Some(&CompatibilityInfo::default()),
            None,
            None,
            Some(BUILD_UNKNOWN),
            None,
            None,
            Some(&[]),
            Some(&[]),
            None,
            None,
            0,
            0,
        )?;
        st.apps.push(App {
            pid,
            uid,
            start_seq,
            package: start.package,
            thread: thread.clone(),
        });
        Ok(())
    }

    /// ActivityManagerService.finishAttachApplication:4995 -> finishAttachApplicationInner:4865: the attached app of the caller's pid with the same uid and startSeq (:4874), else
    /// dropped (:4877-4892, killed in AOSP); then the ActivityTaskManager attach (:4905-4909 -> realStartActivityLocked) schedules the launch transaction.
    fn r#finishAttachApplication(&self, start_seq: i64, _on_create_ns: i64) -> binder::Result<()> {
        let pid = ThreadState::get_calling_pid();
        let uid = ThreadState::get_calling_uid();
        let mut st = self.state();
        let Some(app) = st
            .apps
            .iter()
            .find(|a| a.pid == pid && a.uid == uid && a.start_seq == start_seq)
        else {
            log(format_args!(
                "Mismatched or missing ProcessRecord: pid {pid}, uid {uid}, startSeq {start_seq}"
            ));
            return Ok(());
        };
        let (package, thread) = (app.package.clone(), app.thread.clone());
        st.next_ident += 1;
        let ident = st.next_ident;
        drop(st);
        let Some(p) = self.record(&package) else {
            return Ok(());
        };
        let transaction = ClientTransaction {
            items: vec![Some(ClientTransactionItem::LaunchActivity(
                self.launch_item(p, ident),
            ))],
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
                .packages
                .by_name(pkg)
                .is_some_and(|p| permission::app_id(p.uid) == permission::app_id(uid));
            if !owns && !permission::is_system_or_root(uid) {
                return Err(security(&format!(
                    "Given calling package {pkg} does not match caller's uid {uid}"
                )));
            }
        }
        Ok((name == Some(SETTINGS_AUTHORITY)).then(|| self.settings_holder()))
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
