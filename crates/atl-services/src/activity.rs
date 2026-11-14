// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java, frameworks/base/services/core/java/com/android/server/am/ProcessList.java, frameworks/base/services/core/java/com/android/server/am/InstrumentationReporter.java, frameworks/base/services/core/java/com/android/server/am/ContentProviderHelper.java, frameworks/base/services/core/java/com/android/server/am/ContentProviderRecord.java, frameworks/base/services/core/java/com/android/server/wm/ActivityTaskSupervisor.java, frameworks/base/services/core/java/com/android/server/wm/ActivityClientController.java
//! IActivityManager + the ActivityTaskManager launch path + IActivityClientController, subset: a process the services started attaches (attachApplication -> bindApplication), finishes attaching
//! (finishAttachApplication -> scheduleTransaction([LaunchActivityItem, ResumeActivityItem]) of its launcher activity), asks for the settings provider (getContentProvider) and its display (getDisplayId);
//! instrumentation runs (startInstrumentation, addInstrumentationResults, finishInstrumentation, a crash or kill of the instrumented process), and broadcasts, P1 (BroadcastController's IActivityManager
//! half: registerReceiverWithFeature / unregisterReceiver, non-ordered broadcastIntentWithFeature to registered receivers and explicit-component manifest receivers, stickies, unbroadcastIntent;
//! the rules are broadcast.rs, the sends broadcast_queue.rs). A manifest receiver whose process is not running gets it started headless (no activity launch); its deliveries wait for the attach.
//! Processes are started and killed through [`ProcessControl`] (the zygote / Process.killProcess seam); a crash report (handleApplicationCrash) is logged; every other method is counted unimplemented (counters).
//! PendingIntent records (getIntentSenderWithFeature, cancelIntentSender) are pending_intent.rs's; IIntentSender.send is counted, not modelled.
//! Locking: one state lock (AMS's `this` + mProcLock). Nothing blocking runs under it but the control socket's start request (as AOSP starts a process under the lock); a kill, and every call into a watcher
//! (the reporter thread: AOSP posts them for the same reason, "can't call out of the system process with a lock held"), happens after the lock is released.

use std::io::Write;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use atl_aidl::aidl::android::app::IActivityClientController::{
    BnActivityClientController, IActivityClientController,
};
use atl_aidl::aidl::android::app::IActivityManager::{EnforcePermission, IActivityManager};
use atl_aidl::aidl::android::app::IApplicationThread::IApplicationThread;
use atl_aidl::aidl::android::app::IInstrumentationWatcher::IInstrumentationWatcher;
use atl_aidl::aidl::android::app::IRequestFinishCallback::IRequestFinishCallback;
use atl_aidl::aidl::android::app::IUiAutomationConnection::IUiAutomationConnection;
use atl_aidl::aidl::android::content::IIntentReceiver::IIntentReceiver;
use atl_aidl::aidl::android::content::IIntentSender::IIntentSender;
use atl_broker::control::StartRequest;
use atl_parcelables::android::app::servertransaction::ClientTransaction;
use atl_parcelables::android::app::{
    ActivityManager_PendingIntentInfo, ActivityManager_RunningAppProcessInfo,
    ActivityManager_TaskDescription, ApplicationErrorReport_ParcelableCrashInfo,
    ContentProviderHolder,
};
use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, ProviderInfo, ProviderInfoList,
};
use atl_parcelables::android::content::res::{CompatibilityInfo, Configuration};
use atl_parcelables::android::content::{ComponentName, Intent, IntentFilter};
use atl_parcelables::android::graphics::Rect;
use atl_parcelables::android::os::Bundle;
use atl_parcelables::android::util::MergedConfiguration;
use atl_parcelables::android::window::SizeConfigurationBuckets;
use atl_parcelables::codecs::android::app::servertransaction::ClientTransaction::ClientTransactionItem;
use atl_parcelables::java::io::FileDescriptor;
use atl_parcelables::parts::{
    ActivityConfigurationChangeItem, ActivityRelaunchItem, ActivityWindowInfo, ComponentInfo,
    ConfigurationChangeItem, LaunchActivityItem, PackageItemInfo, ResumeActivityItem,
};
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, SpIBinder, Status, Strong,
    ThreadState,
};

use crate::activity_start::{self, Refusal};
use crate::activity_task::{ActivityTasks, Back};
use crate::broadcast::{self, BroadcastFilter, Broadcasts, IntentKey, StickyRefusal};
use crate::broadcast_queue::{self, Job};
use crate::config_diff::{self, Config, Delivery};
use crate::packages::{PackageRecord, Packages, Permissions};
use crate::pending_intent::{self, PendingIntents};
use crate::pending_intent_table::Key;
use crate::process_control::ProcessControl;
use crate::running_processes;
use crate::shared_memory::ApplicationSharedMemory;
use crate::tasks::{TaskId, TaskSize, Tasks, initial_size};
use crate::ui_agents::UiAgents;
use crate::{package, settings};
use atl_services_types::display_model::DisplayModel;
use atl_services_types::permission::{self, ROOT_UID, SHELL_UID, SYSTEM_UID};
use atl_services_types::size_buckets::SizeBuckets;
use atl_services_wire::ui_agent::IUiAgent;

const AM: &str = "IActivityManager";
const ACC: &str = "IActivityClientController";

/// Display.DEFAULT_DISPLAY.
pub(crate) const DEFAULT_DISPLAY: i32 = 0;
/// Context.DEVICE_ID_DEFAULT.
const DEVICE_ID_DEFAULT: i32 = 0;
/// ActivityManager.PROCESS_STATE_TOP: the reported state of the process whose activity is launched in front.
const PROCESS_STATE_TOP: i32 = 2;
/// ActivityManager.PROCESS_STATE_UNKNOWN (ProcessStateEnum.UNKNOWN, -1): the procState of ResumeActivityItem's three-argument constructor, which realStartActivityLocked uses.
const PROCESS_STATE_UNKNOWN: i32 = -1;
/// Build.UNKNOWN: the serial apps targeting P+ get (ActivityManagerService attachApplicationLocked buildSerial).
const BUILD_UNKNOWN: &str = "unknown";
/// bindApplication's disabledCompatChanges (AppCompatCallbacks.install in handleBindApplication): Toast.CHANGE_TEXT_TOASTS_IN_THE_SYSTEM (Toast.java:124, 147798919). Enabled (AOSP's default for targetSdk > Q), a text toast goes to enqueueTextToast and SystemUI's ToastUI draws it.
/// There is no SystemUI here, so every app takes AOSP's own disabled-change path (owner 2026-11-05, kept 2026-11-06): Toast.makeText builds the view (Toast.java:523, ToastPresenter.getTextToastView); Toast.show's else branch (:199-215) sends enqueueToast with the app's TN;
/// notification.rs answers TN.show(windowToken), and TN.handleShow (:758-792) adds the TYPE_TOAST window through ToastPresenter.show in the app. App-visible difference: Toast.getView() is non-null for text toasts. Delete this when a system UI renders enqueueTextToast.
const DISABLED_COMPAT_CHANGES: [i64; 1] = [147_798_919];
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
/// UserHandle.USER_CURRENT_OR_SELF.
const USER_CURRENT_OR_SELF: i32 = -3;

/// UserController.handleIncomingUser for the one user: the current-user aliases resolve to USER_SYSTEM; USER_ALL and explicit users stay.
fn incoming_user(user_id: i32) -> i32 {
    match user_id {
        USER_CURRENT | USER_CURRENT_OR_SELF => USER_SYSTEM,
        u => u,
    }
}

/// Configuration.ORIENTATION_PORTRAIT / ORIENTATION_LANDSCAPE (Configuration.java:713-714) and DisplayMetrics.DENSITY_DEFAULT (160): the density a dpi is measured against.
const ORIENTATION_PORTRAIT: i32 = 1;
const ORIENTATION_LANDSCAPE: i32 = 2;
const DENSITY_DEFAULT: f32 = 160.0;

/// The global configuration's uiMode, the one source the `uimode` service answers from (uimode.rs): UI_MODE_TYPE_NORMAL (1) | UI_MODE_NIGHT_NO (0x10), what UiModeManagerService.updateConfigurationLocked
/// writes for a normal device with the default night mode (config_defaultNightMode = MODE_NIGHT_NO). Following the desktop's dark appearance is a later setting.
pub(crate) const GLOBAL_UI_MODE: i32 = 0x11;

/// The global configuration a started app gets (ActivityTaskManagerService's `getGlobalConfiguration`), from the display model: font scale 1 (the `font_scale` setting's default), the locale,
/// and the display's density and size. The size is DisplayContent.computeScreenAppConfiguration:2462-2468 for the one display: dp = px / (dpi / 160), rounded; portrait when width <= height.
/// The input fields stay undefined (0) until the platform layer supplies input. Shared with window.rs (the MergedConfiguration of a relayout).
pub(crate) fn global_configuration(display: &DisplayModel) -> Configuration {
    let density = display.dpi as f32 / DENSITY_DEFAULT;
    let width_dp = (display.width as f32 / density + 0.5) as i32;
    let height_dp = (display.height as f32 / density + 0.5) as i32;
    Configuration {
        font_scale: 1.0,
        locale_list: Some(DEFAULT_LOCALE.to_owned()),
        density_dpi: display.dpi,
        screen_width_dp: width_dp,
        screen_height_dp: height_dp,
        smallest_screen_width_dp: width_dp.min(height_dp),
        orientation: if width_dp <= height_dp {
            ORIENTATION_PORTRAIT
        } else {
            ORIENTATION_LANDSCAPE
        },
        ui_mode: GLOBAL_UI_MODE,
        // DisplayContent.computeScreenConfiguration:2460/2475: the display's rotation as both
        window_configuration: atl_parcelables::parts::WindowConfiguration {
            rotation: display.rotation,
            display_rotation: display.rotation,
            ..Default::default()
        },
        ..Configuration::default()
    }
}

/// WindowConfiguration.ROTATION_UNDEFINED (WindowConfiguration.java:100): an override that leaves the rotation to its parent (WindowConfiguration.updateFrom skips it).
const ROTATION_UNDEFINED: i32 = -1;

/// WindowConfiguration.WINDOWING_MODE_FREEFORM (WindowConfiguration.java:113): a Task the user sizes, one per toplevel window.
const WINDOWING_MODE_FREEFORM: i32 = 5;

/// The override of a Task of `size` on `display` (TaskFragment.computeConfigResourceOverrides, TaskFragment.java:2408, for a FREEFORM Task): the bounds from the origin, the mode, and
/// the dp of the bounds. A floating Task is not restricted to the stable (inset) area (TaskFragment.java:2485), so the dp come from the bounds; the smallest width is min(w, h) / density (:2548-2552).
/// Density stays the display's: a resize changes only screenWidthDp/HeightDp and what derives from them. screenLayout is left undefined (not computed yet).
pub(crate) fn task_override(display: &DisplayModel, size: TaskSize) -> Configuration {
    let density = display.dpi as f32 / DENSITY_DEFAULT;
    let width_dp = (size.width as f32 / density + 0.5) as i32;
    let height_dp = (size.height as f32 / density + 0.5) as i32;
    let rect = |right: i32, bottom: i32| Rect {
        left: 0,
        top: 0,
        right,
        bottom,
    };
    Configuration {
        screen_width_dp: width_dp,
        screen_height_dp: height_dp,
        smallest_screen_width_dp: width_dp.min(height_dp),
        orientation: if width_dp <= height_dp {
            ORIENTATION_PORTRAIT
        } else {
            ORIENTATION_LANDSCAPE
        },
        window_configuration: atl_parcelables::parts::WindowConfiguration {
            bounds: rect(size.width, size.height),
            app_bounds: Some(rect(size.width, size.height)),
            max_bounds: rect(display.width, display.height),
            windowing_mode: WINDOWING_MODE_FREEFORM,
            rotation: ROTATION_UNDEFINED,
            display_rotation: ROTATION_UNDEFINED,
            ..Default::default()
        },
        ..Configuration::default()
    }
}

/// The configuration of a Task of `size` on `display`: the global configuration with the Task's override (the fields the override sets replace the global ones).
pub(crate) fn task_configuration(display: &DisplayModel, size: TaskSize) -> Configuration {
    let over = task_override(display, size);
    let mut config = global_configuration(display);
    config.screen_width_dp = over.screen_width_dp;
    config.screen_height_dp = over.screen_height_dp;
    config.smallest_screen_width_dp = over.smallest_screen_width_dp;
    config.orientation = over.orientation;
    let rotations = (
        config.window_configuration.rotation,
        config.window_configuration.display_rotation,
    );
    config.window_configuration = over.window_configuration;
    // the override leaves them undefined: the parent's stay
    (
        config.window_configuration.rotation,
        config.window_configuration.display_rotation,
    ) = rotations;
    config
}

/// The plain form of a configuration: the fields config_diff reads, as the parcelable holds them (undefined stays as the parcel has it).
impl From<&Configuration> for Config {
    fn from(c: &Configuration) -> Self {
        let rect = |r: &Rect| config_diff::Rect4 {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        };
        let w = &c.window_configuration;
        Config {
            font_scale: c.font_scale,
            mcc: c.mcc,
            mnc: c.mnc,
            locale_list: c.locale_list.clone(),
            touchscreen: c.touchscreen,
            keyboard: c.keyboard,
            keyboard_hidden: c.keyboard_hidden,
            hard_keyboard_hidden: c.hard_keyboard_hidden,
            navigation: c.navigation,
            navigation_hidden: c.navigation_hidden,
            orientation: c.orientation,
            screen_layout: c.screen_layout,
            color_mode: c.color_mode,
            ui_mode: c.ui_mode,
            screen_width_dp: c.screen_width_dp,
            screen_height_dp: c.screen_height_dp,
            smallest_screen_width_dp: c.smallest_screen_width_dp,
            density_dpi: c.density_dpi,
            assets_seq: c.assets_seq,
            font_weight_adjustment: c.font_weight_adjustment,
            grammatical_gender: c.grammatical_gender,
            window: config_diff::Window {
                bounds: rect(&w.bounds),
                app_bounds: w.app_bounds.as_ref().map(rect),
                max_bounds: rect(&w.max_bounds),
                windowing_mode: w.windowing_mode,
                activity_type: w.activity_type,
                always_on_top: w.always_on_top,
                rotation: w.rotation,
                display_rotation: w.display_rotation,
            },
        }
    }
}

/// ActivityTaskSupervisor.java:1034: the ResumeActivityItem realStartActivityLocked schedules after the launch (andResume), with the same token as the launch.
/// isForward is the launch's (the transition is forward), no compat fake focus.
pub(crate) fn resume_item(token: SpIBinder) -> ResumeActivityItem {
    ResumeActivityItem {
        activity_token: Some(token),
        proc_state: PROCESS_STATE_UNKNOWN,
        is_forward: true,
        should_send_compat_fake_focus: false,
    }
}

/// The LaunchActivityItem of `p`'s `activity` (ActivityTaskSupervisor.realStartActivityLocked's launch): `intent` as the start sent it, the global configuration, the controller every activity names. Built by activity_task when the machine's Launch step is sent.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_item(
    packages: &Packages,
    display: &DisplayModel,
    controller: &SpIBinder,
    p: &PackageRecord,
    activity: &ActivityInfo,
    token: SpIBinder,
    ident: i32,
    intent: Intent,
) -> LaunchActivityItem {
    let info = package::activity_info(p, packages, activity, package::STOCK_PM_FLAGS);
    LaunchActivityItem {
        activity_token: Some(token),
        ident,
        cur_config: Some(global_configuration(display)),
        override_config: Some(Configuration::default()),
        device_id: DEVICE_ID_DEFAULT,
        proc_state: PROCESS_STATE_TOP,
        is_forward: true,
        activity_client_controller: Some(controller.clone()),
        activity_window_info: Some(ActivityWindowInfo::default()),
        intent: Some(intent),
        info: Some(info),
        ..Default::default()
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
    /// the Task of the launch, made at the process start (start_process_locked); None for a process with no launch
    task: Option<TaskId>,
    /// ProcessRecord.getActiveInstrumentation
    instr: Option<ActiveInstrumentation>,
    /// finishAttachApplication handed its waiting broadcasts to the sender (BroadcastQueueImpl.onApplicationAttachedLocked); until then deliveries queue
    broadcasts_attached: bool,
}

/// The thread of an attached process and the death link on its binder (AppDeathRecipient, ActivityManagerService.java:4600); the recipient is only held so the link outlives the attach call.
struct Attached {
    thread: Strong<dyn IApplicationThread>,
    /// the process's pid in its own sandbox ([`ProcessControl::program_pid`], read at attach); None when the lookup failed (logged)
    program_pid: Option<i32>,
    _death: DeathRecipient,
}

#[derive(Default)]
struct State {
    processes: Vec<ProcessRecord>,
    /// ProcessList.mProcStartSeqCounter: counts from 0, pre-incremented.
    proc_start_seq_counter: i64,
    /// ActivityManagerService.mApplicationSharedMemoryReadOnlyFd's region
    shared_memory: Option<ApplicationSharedMemory>,
    /// BroadcastController's registrations and stickies, and the deliveries waiting for a process to attach
    broadcasts: Broadcasts<ReceiverHandle, Intent, Job>,
}

/// An IIntentReceiver as a registration key: equal when the binder is (mRegisteredReceivers is keyed by `receiver.asBinder()`).
#[derive(Clone)]
struct ReceiverHandle(Strong<dyn IIntentReceiver>);

impl PartialEq for ReceiverHandle {
    fn eq(&self, o: &Self) -> bool {
        self.0.as_binder() == o.0.as_binder()
    }
}

/// What the broadcast rules read of an Intent (broadcast::IntentKey); the categories as a set. resolvedType falls back to the Intent's own type (Intent.resolveTypeIfNeeded without a resolver).
fn intent_key(intent: &Intent, resolved_type: Option<&str>) -> IntentKey {
    let mut categories: Vec<String> = intent.categories.iter().flatten().cloned().collect();
    categories.sort();
    categories.dedup();
    IntentKey {
        action: intent.action.clone(),
        data: intent.data.clone(),
        r#type: intent.r#type.clone(),
        identifier: intent.identifier.clone(),
        package: intent.package.clone(),
        component: intent
            .component
            .as_ref()
            .map(|c| (c.package.clone(), c.class.clone())),
        categories,
        resolved_type: resolved_type
            .map(str::to_owned)
            .or_else(|| intent.r#type.clone()),
    }
}

/// A registered IntentFilter in the rules' form: actions, categories, schemes, types (mDataTypes).
fn filter_of(f: &IntentFilter) -> broadcast::Filter {
    let flat = |v: &Vec<Option<String>>| v.iter().flatten().cloned().collect::<Vec<_>>();
    broadcast::Filter {
        actions: flat(&f.actions),
        categories: f.categories.as_ref().map(flat),
        schemes: f.data_schemes.as_ref().map(flat),
        types: f.data_types.as_ref().map(flat),
    }
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
    permissions: Arc<Permissions>,
    control: Arc<dyn ProcessControl>,
    state: Mutex<State>,
    reporter: Reporter,
    /// the activity lifecycle: records, launches and starts (activity_task.rs); the client controller forwards its acks here
    atm: Arc<ActivityTasks>,
    settings: SpIBinder,
    /// the Tasks: a launch that starts a Task creates it here and binds its activity's token to it (tasks.rs)
    tasks: Arc<Tasks<SpIBinder>>,
    /// what each activity reported (ReportedBuckets), shared with the activity client controller
    buckets: Arc<ReportedBuckets>,
    /// the broadcast sender thread (broadcast_queue.rs)
    broadcast_queue: broadcast_queue::Queue,
    /// the PendingIntent records (pending_intent.rs)
    pending: PendingIntents,
}

/// The service; a clone shares it (one is registered with servicemanager, another starts the launcher package).
#[derive(Clone)]
pub struct ActivityManager {
    inner: Arc<Inner>,
}

pub(crate) fn log(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "services: {msg}");
}

pub(crate) fn security(msg: &str) -> Status {
    Status::new_exception_str(ExceptionCode::SECURITY, Some(msg))
}

fn illegal_state(msg: &str) -> Status {
    Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(msg))
}

fn illegal_argument(msg: &str) -> Status {
    Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(msg))
}

/// onBackPressed's effect: a known record's callback gets requestFinish. Unknown token or no callback: nothing sent, counted (AOSP returns the same way).
/// A failed send is logged, not returned: the call is oneway, so no caller hears of it. Returns whether requestFinish was sent.
fn back_pressed(known: bool, callback: Option<&dyn IRequestFinishCallback>) -> bool {
    let (true, Some(callback)) = (known, callback) else {
        atl_diag::answered(ACC, "onBackPressed: no such activity");
        return false;
    };
    if let Err(e) = callback.requestFinish() {
        log(format_args!("onBackPressed: requestFinish: {e:?}"));
    }
    true
}

/// What an unhandled Back on a Task's launcher root does: the system default behind ActivityClientController.shouldMoveTaskToBack. One value for every app now; a per-app setting chooses it later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RootBack {
    /// moveTaskToBack, Android 12+ (owner 2026-11-12): the window is minimized, the app keeps running.
    #[default]
    Minimize,
    /// requestFinish, as before Android 12: the root finishes, its window closes and with the last window the process ends.
    Close,
}

/// The policy applied to onBackPressed's choice: under [`RootBack::Close`] a move to the back finishes instead.
fn under_policy(back: Back, root: RootBack) -> Back {
    match (back, root) {
        (Back::TaskToBack { .. }, RootBack::Close) => Back::Finish,
        (back, _) => back,
    }
}

impl ActivityManager {
    /// `tasks` is the table the window service and the registry read: the launch binds each activity it starts to its Task there. `agents` are the apps' UI agents: a root Back minimizes through them.
    /// `back_finish` false is the test-only break (`--no-back-finish`): see [`ActivityClientController::back_finish`]. `root_back`: see [`RootBack`].
    pub fn new(
        packages: Arc<Packages>,
        permissions: Arc<Permissions>,
        control: Arc<dyn ProcessControl>,
        tasks: Arc<Tasks<SpIBinder>>,
        agents: Arc<UiAgents>,
        back_finish: bool,
        root_back: RootBack,
    ) -> Self {
        let buckets = Arc::new(ReportedBuckets::default());
        ActivityManager {
            inner: Arc::new_cyclic(|inner| {
                let controller = BnActivityClientController::new_binder(
                    ActivityClientController {
                        buckets: Arc::clone(&buckets),
                        inner: inner.clone(),
                        agents,
                        back_finish,
                        root_back,
                    },
                    BinderFeatures::default(),
                )
                .as_binder();
                Inner {
                    atm: Arc::new(ActivityTasks::new(
                        Arc::clone(&packages),
                        Arc::clone(&permissions),
                        Arc::clone(&tasks),
                        controller.clone(),
                    )),
                    packages,
                    permissions,
                    control,
                    state: Mutex::new(State::default()),
                    reporter: Reporter::new(),
                    settings: settings::new_binder(),
                    tasks,
                    buckets,
                    broadcast_queue: broadcast_queue::Queue::new(),
                    pending: PendingIntents::default(),
                }
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
        if p.launcher().is_none() {
            return Err(format!("{package} has no launcher activity"));
        }
        let mut st = self.inner.state();
        self.inner
            .start_process_locked(&mut st, p, p.process_name(), true, None)
            .map_err(|e| format!("starting {package}: {e}"))
    }

    /// startActivity with no caller app thread (`am start`, a home screen): checked by activity_start::check_shell; accepted -> the package's process is started through the
    /// control and its launcher launched on attach (ActivityTaskManagerService.startProcessAsync, then realStartActivityLocked from attachApplication). Ok: the START code.
    pub(crate) fn start_without_caller(
        &self,
        calling_uid: u32,
        flags: i32,
        component: Option<(&str, &str)>,
    ) -> Result<i32, Status> {
        let p = component.and_then(|(package, _)| self.inner.packages.by_name(package));
        let launcher = p
            .and_then(|p| p.launcher())
            .and_then(|a| a.component.item.name.as_deref());
        let mut st = self.inner.state();
        let running = p.is_some_and(|p| st.processes.iter().any(|r| r.package == p.name()));
        match activity_start::check_shell(calling_uid, flags, component, launcher, running) {
            Ok(()) => {}
            Err(Refusal::Code { code, .. }) => return Ok(code),
            Err(Refusal::Security(msg)) => return Err(security(msg)),
        }
        let Some(p) = p else {
            return Ok(activity_start::START_CLASS_NOT_FOUND);
        };
        self.inner
            .start_process_locked(&mut st, p, p.process_name(), true, None)
            .map_err(|e| {
                Status::new_exception_str(
                    ExceptionCode::ILLEGAL_STATE,
                    Some(format!("starting {}: {e}", p.name())),
                )
            })?;
        log(format_args!(
            "activity start of {} from uid {calling_uid} (no caller app): process started",
            p.name()
        ));
        Ok(activity_start::START_SUCCESS)
    }

    /// The lifecycle the activity_task service (IActivityTaskManager) is built on.
    pub fn activity_tasks(&self) -> Arc<ActivityTasks> {
        Arc::clone(&self.inner.atm)
    }

    /// The display changed (registry SET_DISPLAY): the new global configuration to every attached process, as ActivityTaskManagerService.updateGlobalConfigurationLocked:4807-4813 does
    /// (WindowProcessController.dispatchConfiguration:1773: ConfigurationChangeItem, device 0). Needed: a Task's override leaves density to the global configuration (task_override), so without it
    /// an activity's merged configuration and the Application's resources keep the old density. Returns the processes told.
    pub(crate) fn global_configuration_changed(&self) -> usize {
        let config = global_configuration(&self.inner.tasks.display());
        let threads: Vec<_> = self
            .inner
            .state()
            .processes
            .iter()
            .filter_map(|p| p.attached.as_ref().map(|a| a.thread.clone()))
            .collect();
        let mut told = 0;
        for thread in threads {
            let transaction = ClientTransaction {
                items: vec![Some(ClientTransactionItem::ConfigurationChange(Box::new(
                    ConfigurationChangeItem {
                        configuration: config.clone(),
                        device_id: 0,
                    },
                )))],
            };
            match thread.r#scheduleTransaction(Some(&transaction)) {
                Ok(()) => told += 1,
                Err(e) => log(format_args!(
                    "scheduleTransaction (global configuration): {e}"
                )),
            }
        }
        told
    }

    /// Registry set_bounds changed `task`'s size.
    pub(crate) fn task_resized(&self, task: TaskId) {
        self.inner.task_resized(task);
    }

    /// An activity of `task` is relaunching (WindowState.inRelaunchingActivity): its windows wait, a layout now would run in the old instance on the old configuration.
    pub(crate) fn task_relaunching(&self, task: TaskId) -> bool {
        self.inner.task_relaunching(task)
    }

    /// The Task of the process `pid` (its launch's, made at the process start); None for a process with no launch.
    pub(crate) fn task_of_process(&self, pid: i32) -> Option<TaskId> {
        self.inner
            .state()
            .processes
            .iter()
            .find(|r| r.pid == pid)
            .and_then(|r| r.task)
    }
}

impl Inner {
    /// ActivityRecord.ensureActivityConfiguration for each activity of `task`, at the Task's size now. Reported is recorded before the send, as AOSP's mLastReportedConfiguration.
    /// Each activity is a record of the lifecycle machine (activity_task Owner): one started by startActivity gets the change as the launcher one does; the steps after a relaunch come from its phase.
    fn task_resized(&self, task: TaskId) {
        let Some(size) = self.tasks.size(task) else {
            return;
        };
        let display = self.tasks.display();
        let over = Configuration {
            seq: self.tasks.config_seq(task),
            ..task_override(&display, size)
        };
        let new = Config::from(&task_configuration(&display, size));
        let task_rect = || Rect {
            left: 0,
            top: 0,
            right: size.width,
            bottom: size.height,
        };
        let window_info = || {
            Some(ActivityWindowInfo {
                is_embedded: false,
                task_bounds: task_rect(),
                task_fragment_bounds: task_rect(),
            })
        };
        let mut sends = Vec::new();
        {
            for (token, entry) in self.atm.config_records(task) {
                let mut relaunching = entry.relaunching;
                let buckets = self.buckets.get(&token);
                let items = match config_diff::delivery(
                    &entry.reported,
                    &new,
                    buckets.as_ref(),
                    entry.declared,
                    entry.relaunching,
                ) {
                    Delivery::Nothing => continue,
                    Delivery::Wait => {
                        atl_diag::answered(AM, "configuration change waits for the relaunch");
                        continue;
                    }
                    Delivery::Send => {
                        atl_diag::answered(
                            AM,
                            "configuration change sent (ActivityConfigurationChangeItem)",
                        );
                        vec![Some(ClientTransactionItem::ActivityConfigurationChange(
                            Box::new(ActivityConfigurationChangeItem {
                                activity_token: Some(token.clone()),
                                configuration: Some(over.clone()),
                                activity_window_info: window_info(),
                            }),
                        ))]
                    }
                    Delivery::Relaunch {
                        changes,
                        preserve_window,
                    } => {
                        atl_diag::answered(
                            AM,
                            "configuration change relaunch (ActivityRelaunchItem)",
                        );
                        relaunching = true;
                        // the lifecycle items after the relaunch come from the machine: Resume only for a resumed activity
                        let mut items = vec![Some(ClientTransactionItem::ActivityRelaunch(
                            Box::new(ActivityRelaunchItem {
                                activity_token: Some(token.clone()),
                                config: Some(MergedConfiguration {
                                    global: global_configuration(&display),
                                    override_config: over.clone(),
                                    merged: task_configuration(&display, size),
                                }),
                                activity_window_info: window_info(),
                                config_changes: changes as i32,
                                preserve_window,
                            }),
                        ))];
                        items.extend(self.atm.relaunch_items(&token));
                        items
                    }
                };
                self.atm.update_owner(&token, |o| {
                    o.reported = new.clone();
                    o.relaunching = relaunching;
                });
                sends.push((entry.thread.clone(), ClientTransaction { items }));
            }
        }
        for (thread, transaction) in sends {
            if let Err(e) = thread.r#scheduleTransaction(Some(&transaction)) {
                log(format_args!(
                    "scheduleTransaction (configuration change): {e}"
                ));
            }
        }
    }

    /// ActivityRecord.finishRelaunching: the relaunch of `token` ended, so what its Task's size did meanwhile is delivered now, and its windows, held back meanwhile, get the size unless another relaunch began.
    fn relaunch_finished(&self, token: &SpIBinder) {
        self.atm.update_owner(token, |o| o.relaunching = false);
        let Some(task) = self.tasks.task_of_token(Some(token)) else {
            return;
        };
        self.task_resized(task);
        if !self.task_relaunching(task) {
            crate::window::notify_resized(&self.tasks, task);
        }
    }

    fn task_relaunching(&self, task: TaskId) -> bool {
        self.atm.task_relaunching(task)
    }

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
            package: p.name().to_owned(),
            target_sdk: p.app().target_sdk_version,
            entry_point: ACTIVITY_THREAD.to_owned(),
            args: vec![format!("{PROC_START_SEQ_IDENT}{start_seq}")],
            instruction_set: p.instruction_set.clone(),
            rights: atl_broker::rights_of(self.permissions.granted_names(p.uid).as_slice()),
        })?;
        // the launch's Task exists before the process attaches: the composer asks for its bounds and package at its window's install (registry TASK_INFO)
        let task = p.launcher().filter(|_| launch_activity).map(|a| {
            let size = initial_size(&self.tasks.display(), &crate::packages::bounds_input(a));
            self.tasks.new_task(p.uid, p.name(), size)
        });
        st.processes.push(ProcessRecord {
            package: p.name().to_owned(),
            process_name: process_name.to_owned(),
            uid: p.uid,
            start_seq,
            pid,
            attached: None,
            launch_activity,
            task,
            instr,
            broadcasts_attached: false,
        });
        Ok(())
    }

    /// realStartActivityLocked (ActivityTaskSupervisor.java:1021-1029): the LaunchActivityItem of the package's launcher activity, under `token` (the one its ResumeActivityItem carries).
    /// ActivityInfo.flags are the activity's manifest flags (manifest_flags). Not sent yet (slice 7 needs them): results, new intents, saved state; the assist / shareable / fragment tokens stay null.
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
            Self::broadcasts_died(st, rec.pid);
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
            if let Some(a) = &rec.attached {
                self.atm.process_died(&a.thread.as_binder());
            }
            Self::broadcasts_died(&mut st, pid);
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

    /// BroadcastQueueImpl.onApplicationCleanupLocked + the ReceiverList cleanup of a dead process: its registrations and waiting deliveries go; dropped deliveries are counted.
    fn broadcasts_died(st: &mut State, pid: i32) {
        for _ in 0..st.broadcasts.process_died(pid) {
            atl_diag::answered(AM, "broadcast dropped: its process died before attaching");
        }
    }

    /// checkPermission(permission, pid, uid) against the uid's granted permission states.
    fn holds(&self, uid: u32, perm: &str) -> bool {
        self.permissions.check(uid, Some(perm))
    }

    /// The thread of `pid` once finishAttachApplication drained its waiting deliveries; None before.
    fn thread_of(st: &State, pid: i32) -> Option<Strong<dyn IApplicationThread>> {
        st.processes
            .iter()
            .find(|r| r.pid == pid && r.broadcasts_attached)
            .and_then(|r| r.attached.as_ref().map(|a| a.thread.clone()))
    }

    /// One delivery for process `pid`: to the sender when it has attached, else queued until it does (BroadcastProcessQueue's pending cold start).
    fn deliver_locked(&self, st: &mut State, pid: i32, job: Job) {
        match Self::thread_of(st, pid) {
            Some(thread) => self.broadcast_queue.enqueue(thread, job),
            None => st.broadcasts.push_pending(pid, job),
        }
    }

    /// The manifest receiver of an explicit-component broadcast (broadcastIntentLocked's collectReceiverComponents for a component, :1966-1982, then BroadcastSkipPolicy.shouldSkip and
    /// BroadcastQueueImpl.scheduleReceiverColdLocked:992 / Warm:1066): its process, started headless when not running (`launch_activity` false), gets scheduleReceiver.
    #[allow(clippy::too_many_arguments)]
    fn deliver_to_component_locked(
        &self,
        st: &mut State,
        intent: &Intent,
        package: &str,
        class: &str,
        sender_uid: u32,
        required: &[String],
        result: (i32, Option<String>, Option<Bundle>),
        user: i32,
    ) {
        let Some(p) = self.packages.by_name(package) else {
            atl_diag::answered(
                AM,
                "broadcast to a component of an unknown package: no receiver",
            );
            return;
        };
        let Some(decl) = p.receiver(class).filter(|r| r.component.enabled) else {
            atl_diag::answered(
                AM,
                "broadcast to a component that is no enabled <receiver>: no receiver",
            );
            return;
        };
        let component = format!("{package}/{class}");
        let target = broadcast::ManifestReceiver {
            component: &component,
            permission: decl.permission.as_deref(),
            exported: decl.component.exported,
            uid: p.uid,
        };
        if let Some(why) = broadcast::skip_manifest(
            &target,
            sender_uid,
            required,
            |perm| self.holds(sender_uid, perm),
            |perm| self.holds(p.uid, perm),
        ) {
            atl_diag::answered(
                AM,
                "broadcast skipped: Permission Denial (manifest receiver)",
            );
            log(format_args!("{why}"));
            return;
        }
        let process_name = decl
            .component
            .process_name
            .as_deref()
            .unwrap_or(p.process_name())
            .to_owned();
        // the stored receiver for STOCK_PM_FLAGS (ComputerEngine.queryIntentReceivers' ResolveInfo.activityInfo)
        let info = package::activity_info(p, &self.packages, decl, package::STOCK_PM_FLAGS);
        // the receiver's Intent carries its component (BroadcastRecord.getReceiverIntent: setComponent for a ResolveInfo)
        let mut receiver_intent = intent.clone();
        receiver_intent.component = Some(ComponentName {
            package: p.name().to_owned(),
            class: class.to_owned(),
        });
        let job = Job::Manifest {
            intent: receiver_intent,
            info: Box::new(info),
            result_code: result.0,
            data: result.1,
            extras: result.2,
            user,
        };
        let running = st
            .processes
            .iter()
            .find(|r| r.process_name == process_name && r.uid == p.uid)
            .map(|r| r.pid);
        let pid = match running {
            Some(pid) => pid,
            None => {
                if let Err(e) = self.start_process_locked(st, p, &process_name, false, None) {
                    // BroadcastQueueImpl:1037-1050: the start failed, the delivery fails
                    atl_diag::answered(AM, "broadcast dropped: receiver process start failed");
                    log(format_args!(
                        "Unable to launch app {process_name} for broadcast {component}: {e}"
                    ));
                    return;
                }
                atl_diag::answered(AM, "broadcast cold start (scheduleReceiverColdLocked)");
                st.processes.last().map(|r| r.pid).unwrap_or_default()
            }
        };
        self.deliver_locked(st, pid, job);
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
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .inner
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(security(&format!("{method}: caller lacks {permission:?}")))
    }

    /// ActivityManagerService.checkPermissionForDevice (:6193-6200) -> checkComponentPermission: the granted permission states of `uid` (Permissions::check); one device.
    /// Was unimplemented, whose empty reply reads 0 = PERMISSION_GRANTED: every check passed.
    fn r#checkPermissionForDevice(
        &self,
        permission: Option<&str>,
        _pid: i32,
        uid: i32,
        _device_id: i32,
    ) -> binder::Result<i32> {
        /// PackageManager.PERMISSION_GRANTED / PERMISSION_DENIED.
        const GRANTED: i32 = 0;
        const DENIED: i32 = -1;
        let Ok(uid) = u32::try_from(uid) else {
            return Ok(DENIED);
        };
        Ok(if self.inner.permissions.check(uid, permission) {
            GRANTED
        } else {
            DENIED
        })
    }

    /// ActivityManagerService.handleApplicationCrash (AppErrors shows and records the report in AOSP): an app's uncaught exception as its process dies (RuntimeInit's
    /// KillApplicationHandler, which kills the process itself afterwards). Logged here with the process it came from; nothing else, as the process is already going down.
    fn r#handleApplicationCrash(
        &self,
        app: Option<&SpIBinder>,
        crash: Option<&ApplicationErrorReport_ParcelableCrashInfo>,
    ) -> binder::Result<()> {
        let Some(crash) = crash else {
            return Ok(());
        };
        let who = {
            let st = self.inner.state();
            st.processes
                .iter()
                .find(|r| {
                    r.attached
                        .as_ref()
                        .is_some_and(|a| app.is_some_and(|b| a.thread.as_binder() == *b))
                })
                .map_or_else(
                    || "an unknown process".to_owned(),
                    |r| format!("{} (pid {})", r.process_name, r.pid),
                )
        };
        log(format_args!(
            "crash report from {who}: {}: {}; thrown at {}.{}({}:{})",
            crash.exception_class_name.as_deref().unwrap_or("?"),
            crash.exception_message.as_deref().unwrap_or(""),
            crash.throw_class_name.as_deref().unwrap_or("?"),
            crash.throw_method_name.as_deref().unwrap_or("?"),
            crash.throw_file_name.as_deref().unwrap_or("?"),
            crash.throw_line_number,
        ));
        Ok(())
    }

    /// ActivityManagerService.addPackageDependency (WebViewFactory registers on its provider package): AMS records the package so the caller dies when it is updated or removed.
    /// Packages here change only at atld start (install sessions), never under a running process, so the record would have no reader: accepted and dropped, as AOSP also returns void.
    fn r#addPackageDependency(&self, _package_name: Option<&str>) -> binder::Result<()> {
        Ok(())
    }

    /// ActivityManagerService.getRunningAppProcesses: the attached processes the caller may see (running_processes). Not enforced: enforceNotIsolatedCaller (no isolated uids).
    fn r#getRunningAppProcesses(
        &self,
    ) -> binder::Result<Option<Vec<Option<ActivityManager_RunningAppProcessInfo>>>> {
        let uid = ThreadState::get_calling_uid();
        let caller = running_processes::Caller {
            uid,
            all_users: self
                .inner
                .holds(uid, "android.permission.INTERACT_ACROSS_USERS_FULL"),
            all_uids: self.inner.holds(uid, "android.permission.REAL_GET_TASKS"),
        };
        let calling_pid = ThreadState::get_calling_pid();
        let st = self.inner.state();
        let procs = st.processes.iter().filter_map(|r| {
            let a = r.attached.as_ref()?;
            Some(running_processes::RunningProcess {
                process_name: &r.process_name,
                package: &r.package,
                uid: r.uid,
                pid: running_processes::pid_seen_by(r.pid, a.program_pid, calling_pid),
            })
        });
        Ok(running_processes::visible(procs, &caller).map(|list| {
            list.into_iter()
                .map(|p| {
                    Some(ActivityManager_RunningAppProcessInfo {
                        process_name: Some(p.process_name.to_owned()),
                        pid: p.pid,
                        uid: p.uid as i32,
                        pkg_list: Some(vec![Some(p.package.to_owned())]),
                        importance: running_processes::IMPORTANCE_FOREGROUND,
                        process_state: running_processes::PROCESS_STATE_TOP,
                        ..Default::default()
                    })
                })
                .collect()
        }))
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
        // asked before the state lock (a round trip to atld, as kills are); a pid that is no started process fails here and is dropped below
        let program_pid = inner
            .control
            .program_pid(pid)
            .inspect_err(|e| log(format_args!("program pid of {pid}: {e}")))
            .ok();
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
                .map(|t| package::application_info(t, &inner.packages, package::STOCK_PM_FLAGS))
                .ok_or_else(|| illegal_state("instrumentation target vanished"))?,
            None => package::application_info(p, &inner.packages, package::STOCK_PM_FLAGS),
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
        let core_settings = match atl_services_types::gl_driver::selection(
            inner.packages.all().iter().map(|p| (p.name(), p.gl_driver)),
        ) {
            Some((pkgs, values)) => Bundle::from_strings(&[
                (atl_services_types::gl_driver::SELECTION_PKGS, pkgs.as_str()),
                (
                    atl_services_types::gl_driver::SELECTION_VALUES,
                    values.as_str(),
                ),
            ]),
            None => Bundle::default(),
        };
        // what attachApplicationLocked passes for a plain app start (no profiler, backup or sdk sandbox): nulls, the global configuration, the default compat info,
        // the process's providers (generateApplicationProvidersLocked; ActivityThread installs them before Application.onCreate), no disabled compat changes; coreSettings never null (empty unless an app takes ANGLE) (attachApplicationLocked passes
        // mCoreSettingsObserver.getCoreSettingsLocked(), and ActivityThread.updateDebugViewAttributeState reads it unconditionally); its keys are the GL driver selection (gl_driver.rs), nothing else.
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
        let providers = ProviderInfoList {
            list: package::process_providers(
                p,
                &inner.packages,
                &process_name,
                package::STOCK_PM_FLAGS,
            )
            .into_iter()
            .map(Some)
            .collect(),
        };
        thread.r#bindApplication(
            Some(&process_name),
            Some(&info),
            None,
            None,
            false,
            Some(&providers),
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
            Some(&global_configuration(&inner.tasks.display())),
            Some(&CompatibilityInfo::default()),
            None,
            Some(&core_settings),
            Some(BUILD_UNKNOWN),
            None,
            None,
            Some(&DISABLED_COMPAT_CHANGES),
            Some(&[]),
            None,
            app_memory.as_ref(),
            0,
            0,
        )?;
        st.processes[i].attached = Some(Attached {
            thread: thread.clone(),
            program_pid,
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
        let st = inner.state();
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
        let (package, launch, task) = (rec.package.clone(), rec.launch_activity, rec.task);
        let Some(thread) = rec.attached.as_ref().map(|a| a.thread.clone()) else {
            return Ok(());
        };
        drop(st);
        if launch
            && let Some(task) = task
            && let Some(p) = inner.packages.by_name(&package)
            && let Some(activity) = p.launcher()
        {
            // the launch is the lifecycle's: the process's Task (made at its start), the record pushed, its Launch + Resume sent (activity_task.rs). The app starts with the global configuration and no override.
            inner
                .atm
                .launch_launcher(thread.clone(), uid, &package, activity, task);
        }
        // :4933 mBroadcastQueue.onApplicationAttachedLocked, after the activity attach: the deliveries that waited for this process, in order
        let mut st = inner.state();
        for job in st.broadcasts.take_pending(pid) {
            inner.broadcast_queue.enqueue(thread.clone(), job);
        }
        // from here deliveries go straight to the sender: none can pass the drained ones
        if let Some(r) = st.processes.iter_mut().find(|r| r.pid == pid) {
            r.broadcasts_attached = true;
        }
        Ok(())
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

    /// ContentProviderHelper.publishContentProviders:731: an unknown caller is a SecurityException (:742). ActivityThread.installContentProviders calls it before Application.onCreate; a failure there rethrows and kills the app.
    /// Not kept: nothing here serves another process's provider yet (getContentProvider knows settings only); a process finds its own in ActivityThread's mProviderMap without asking.
    fn r#publishContentProviders(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        _providers: Option<&[Option<ContentProviderHolder>]>,
    ) -> binder::Result<()> {
        let known = caller.is_some_and(|t| {
            let st = self.inner.state();
            Inner::record_for_thread(&st, t).is_some()
        });
        if !known {
            return Err(security(
                "Unable to find app for caller when publishing content providers",
            ));
        }
        atl_diag::answered(
            AM,
            "publishContentProviders (not kept: no cross-process lookup)",
        );
        Ok(())
    }

    /// ActivityManagerService.startInstrumentation (:14452-14644), user 0 only. The PackageManager lookups (:14472-14500) read the same package table the PackageManager service answers from.
    /// Not modelled, each counted when asked for: no-restart (:14639 instrumentWithoutRestart), the SDK sandbox flags (:14566), a target process list other than the default process
    /// (:14589-14596; the first listed process is started, the instrumentation does not follow into the others). Not enforced: enforceNotIsolatedCaller (:14456, no isolated uids),
    /// the DISABLE_HIDDEN_API_CHECKS permission (:14551-14563: the flags change nothing, ART's hidden API enforcement is off), the app op for DISABLE_ISOLATED_STORAGE (:14633).
    /// Not modelled: the signature check between the instrumentation and its target (:14503-14523: checkSignatures, then a SecurityException unless root on a debuggable build).
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
            // unimplemented sub-cases: the reply hook counts startInstrumentation `unimplemented` with the cause
            atl_diag::set_cause("INSTR_FLAG_NO_RESTART");
            return Err(binder::StatusCode::UNKNOWN_TRANSACTION.into());
        }
        if flags >> 5 != 0 {
            atl_diag::set_cause("INSTR_FLAG_INSTRUMENT_SDK_SANDBOX");
            return Err(binder::StatusCode::UNKNOWN_TRANSACTION.into());
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
                atl_diag::answered(AM, &format!("startInstrumentation ({what} ignored)"));
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
        let target_package = decl.target_package.as_deref().unwrap_or_default();
        let Some(target) = inner.packages.by_name(target_package) else {
            inner.report_start_failure(
                watcher,
                class_name,
                &format!("Unable to find instrumentation target package: {target_package}"),
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
                    .map_or("null", |p| p.name())
            );
            inner.report_start_failure(watcher, class_name, &msg);
            return Err(security(&msg));
        }
        // :14583-14596 targetProcesses: null = the application's process, "*" = every process of the package, else a comma list whose first is the one started
        let default_process = match decl.target_processes.as_deref() {
            None => target.process_name().to_owned(),
            Some("*") => {
                atl_diag::answered(AM, "startInstrumentation (targetProcesses=*)");
                target.process_name().to_owned()
            }
            Some(list) => {
                let mut names = list.split(',');
                let first = names.next().unwrap_or_default().to_owned();
                if names.next().is_some() {
                    atl_diag::answered(AM, "startInstrumentation (more than one target process)");
                }
                first
            }
        };
        let instr = ActiveInstrumentation {
            class: class_name.clone(),
            target_package: target.name().to_owned(),
            arguments: arguments.cloned(),
            watcher: watcher.cloned(),
            ui_connection: connection.cloned(),
            cur_results: None,
            no_restart,
        };
        // :14613-14622 forceStopPackageLocked of the target, addAppLocked -> startProcessLocked
        let kill = inner.force_stop_package_locked(&mut st, target.name());
        let started =
            inner.start_process_locked(&mut st, target, &default_process, false, Some(instr));
        drop(st);
        inner.kill(&kill);
        started.map_err(|e| illegal_state(&format!("starting {}: {e}", target.name())))?;
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

    /// getIntentSenderWithFeature (:5467-5581), the caller owning the record. AOSP's order: the intents (:5492-5541), the owner's package (:5559-5572), then getIntentSender (PendingIntentController.java:120), whose records are pending_intent.rs. The activity-result kind goes to ActivityTaskManager (:5574): counted, not modelled.
    /// bOptions are send-time data here, not part of the key. The user is the key's only: one user.
    #[allow(clippy::too_many_arguments)]
    fn r#getIntentSenderWithFeature(
        &self,
        kind: i32,
        package: Option<&str>,
        feature_id: Option<&str>,
        token: Option<&SpIBinder>,
        result_who: Option<&str>,
        request_code: i32,
        intents: Option<&[Option<Intent>]>,
        resolved_types: Option<&[Option<String>]>,
        flags: i32,
        _options: Option<&Bundle>,
        user_id: i32,
    ) -> binder::Result<Option<Strong<dyn IIntentSender>>> {
        let uid = ThreadState::get_calling_uid();
        if let Some(intents) = intents {
            if intents.is_empty() {
                return Err(illegal_argument("Intents array length must be >= 1"));
            }
            for intent in intents.iter().flatten() {
                if kind == pending_intent::INTENT_SENDER_BROADCAST
                    && intent.flags & FLAG_RECEIVER_BOOT_UPGRADE != 0
                {
                    return Err(illegal_argument(
                        "Can't use FLAG_RECEIVER_BOOT_UPGRADE here",
                    ));
                }
            }
            if resolved_types.is_some_and(|t| t.len() != intents.len()) {
                return Err(illegal_argument(
                    "Intent array length does not match resolvedTypes length",
                ));
            }
        }
        let package = package.unwrap_or_default();
        let owned =
            permission::is_system_or_root(uid) || self.inner.packages.is_same_app(package, uid);
        if !owned {
            return Err(security(&format!(
                "Permission Denial: getIntentSender() from uid={uid} is not allowed to send as package {package}"
            )));
        }
        if kind == pending_intent::INTENT_SENDER_ACTIVITY_RESULT {
            atl_diag::set_cause("activity result");
            return Err(binder::StatusCode::UNKNOWN_TRANSACTION.into());
        }
        let request = intents.and_then(|v| v.last()).and_then(Option::as_ref);
        let resolved = resolved_types
            .and_then(|v| v.last())
            .and_then(Option::as_deref);
        let key = Key {
            r#type: kind,
            package: package.to_owned(),
            feature_id: feature_id.map(str::to_owned),
            activity: token.cloned(),
            who: result_who.map(str::to_owned),
            request_code,
            intent: request.map(|i| intent_key(i, None)),
            resolved_type: resolved.map(str::to_owned),
            flags: 0,
            user_id,
        };
        let sender = self.inner.pending.get_or_create(key, uid, flags)?;
        atl_diag::answered(AM, "getIntentSenderWithFeature");
        Ok(sender)
    }

    /// getInfoForIntentSender (ActivityManagerService.java:5685-5701): a record this service holds reports its key's package and FLAG_IMMUTABLE, its creator uid and its type; any other sender
    /// (null, or not a record here) null / INVALID_UID / false / INTENT_SENDER_UNKNOWN. Not modelled: package visibility (filterAppAccess), so the package and uid are never filtered.
    fn r#getInfoForIntentSender(
        &self,
        sender: Option<&Strong<dyn IIntentSender>>,
    ) -> binder::Result<Option<ActivityManager_PendingIntentInfo>> {
        let record = sender.and_then(|s| self.inner.pending.info(&s.as_binder()));
        let info = match record {
            Some(r) => {
                atl_diag::answered(AM, "getInfoForIntentSender");
                ActivityManager_PendingIntentInfo {
                    creator_package: Some(r.package),
                    creator_uid: r.uid as i32,
                    immutable: r.flags & pending_intent::FLAG_IMMUTABLE != 0,
                    intent_sender_type: r.r#type,
                }
            }
            None => {
                atl_diag::answered(
                    AM,
                    "getInfoForIntentSender (no PendingIntent of this service)",
                );
                ActivityManager_PendingIntentInfo {
                    creator_package: None,
                    creator_uid: permission::INVALID_UID,
                    immutable: false,
                    intent_sender_type: pending_intent::INTENT_SENDER_UNKNOWN,
                }
            }
        };
        Ok(Some(info))
    }

    /// cancelIntentSender (:5668 -> PendingIntentController.java:262-283): a record this service holds, cancelled by its creator's app id (AOSP compares the package's uid, :269-271). Any other sender: nothing (:263-264).
    fn r#cancelIntentSender(
        &self,
        sender: Option<&Strong<dyn IIntentSender>>,
    ) -> binder::Result<()> {
        let Some(sender) = sender else {
            return Ok(());
        };
        let binder = sender.as_binder();
        let Some(owner) = self.inner.pending.info(&binder).map(|r| r.uid) else {
            atl_diag::answered(AM, "cancelIntentSender (no PendingIntent of this service)");
            return Ok(());
        };
        let uid = ThreadState::get_calling_uid();
        if permission::app_id(owner) != permission::app_id(uid) {
            return Err(security(&format!(
                "Permission Denial: cancelIntentSender() from uid={uid} is not allowed to cancel the PendingIntent of uid={owner}"
            )));
        }
        self.inner.pending.cancel(&binder);
        atl_diag::answered(AM, "cancelIntentSender");
        Ok(())
    }

    /// ActivityManagerService.broadcastIntentWithFeature:14402 -> BroadcastController.broadcastIntentWithFeature:710-754: the Intent is verified as verifyBroadcastLocked does (:1878-1905: a null
    /// Intent is a NullPointerException (`intent.getFlags()`), a file descriptor in the extras and FLAG_RECEIVER_BOOT_UPGRADE are IllegalArgumentExceptions), then broadcastIntentLocked:
    /// a sticky is checked and stored (:1415-1495), an explicit component reaches its manifest receiver (:1516-1518, unless FLAG_RECEIVER_REGISTERED_ONLY), an intent without one the matching registered
    /// receivers (:1520-1545); each passes BroadcastSkipPolicy (broadcast.rs) and is queued for its process (BroadcastQueueImpl, broadcast_queue.rs). Result BROADCAST_SUCCESS.
    /// Not modelled, each counted when used: ordered delivery and `resultTo` (P2: sent unordered, no result), implicit manifest receivers (P2). Not modelled: the isolated-caller check (:715), the options
    /// Bundle, excluded permissions / packages, app ops, protected-broadcast sender checks.
    fn r#broadcastIntentWithFeature(
        &self,
        _caller: Option<&Strong<dyn IApplicationThread>>,
        _calling_feature_id: Option<&str>,
        intent: Option<&Intent>,
        resolved_type: Option<&str>,
        result_to: Option<&Strong<dyn IIntentReceiver>>,
        result_code: i32,
        result_data: Option<&str>,
        result_extras: Option<&Bundle>,
        required_permissions: Option<&[Option<String>]>,
        _excluded_permissions: Option<&[Option<String>]>,
        _excluded_packages: Option<&[Option<String>]>,
        _app_op: i32,
        _options: Option<&Bundle>,
        serialized: bool,
        sticky: bool,
        user_id: i32,
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
        if serialized || result_to.is_some() {
            atl_diag::answered(
                AM,
                "broadcastIntentWithFeature ordered / resultTo (P2: sent unordered, no result)",
            );
        }
        let inner = &self.inner;
        let pid = ThreadState::get_calling_pid();
        let uid = ThreadState::get_calling_uid();
        let user = incoming_user(user_id);
        let required: Vec<String> = required_permissions
            .unwrap_or_default()
            .iter()
            .flatten()
            .cloned()
            .collect();
        if sticky {
            let holds = inner.holds(uid, permission::BROADCAST_STICKY);
            match broadcast::sticky_send(
                holds,
                pid,
                uid,
                !required.is_empty(),
                intent.component.is_some(),
            ) {
                Ok(()) => {}
                Err(StickyRefusal::Security(msg)) => {
                    log(format_args!("{msg}"));
                    return Err(security(&msg));
                }
                Err(StickyRefusal::CantHavePermission) => {
                    log(format_args!(
                        "Can't broadcast sticky intent and enforce permissions {required:?}"
                    ));
                    return Ok(broadcast::BROADCAST_STICKY_CANT_HAVE_PERMISSION);
                }
            }
        }
        let key = intent_key(intent, resolved_type);
        let result = (
            result_code,
            result_data.map(str::to_owned),
            result_extras.cloned(),
        );
        let mut st = inner.state();
        if sticky {
            st.broadcasts
                .put_sticky(user, key.clone(), intent.clone(), uid);
        }
        let registered_only = intent.flags & broadcast::FLAG_RECEIVER_REGISTERED_ONLY != 0;
        match &intent.component {
            Some(c) if !registered_only => inner.deliver_to_component_locked(
                &mut st,
                intent,
                &c.package,
                &c.class,
                uid,
                &required,
                result.clone(),
                user,
            ),
            Some(_) => {}
            None if !registered_only => {
                atl_diag::answered(
                    AM,
                    "implicit broadcast: manifest receivers not resolved (P2)",
                );
            }
            None => {}
        }
        if intent.component.is_none() {
            for (receiver, rpid, ruid, bf) in st.broadcasts.receivers(&key, user) {
                if let Some(why) = broadcast::skip_registered(
                    &bf,
                    uid,
                    ruid,
                    &required,
                    |p| inner.holds(uid, p),
                    |p| inner.holds(ruid, p),
                ) {
                    atl_diag::answered(AM, why.split(':').next().unwrap_or("skipped"));
                    log(format_args!("{why}"));
                    continue;
                }
                let job = Job::Registered {
                    receiver: receiver.0,
                    intent: intent.clone(),
                    result_code: result.0,
                    data: result.1.clone(),
                    extras: result.2.clone(),
                    initial_sticky: false,
                    user,
                };
                inner.deliver_locked(&mut st, rpid, job);
            }
        }
        Ok(BROADCAST_SUCCESS)
    }

    /// BroadcastController.unbroadcastIntent:1756-1790: the caller needs BROADCAST_STICKY (:1766-1772, SecurityException), then the sticky with an equal Intent (filterEquals) goes.
    fn r#unbroadcastIntent(
        &self,
        _caller: Option<&Strong<dyn IApplicationThread>>,
        intent: Option<&Intent>,
        user_id: i32,
    ) -> binder::Result<()> {
        let pid = ThreadState::get_calling_pid();
        let uid = ThreadState::get_calling_uid();
        if !self.inner.holds(uid, permission::BROADCAST_STICKY) {
            let msg = format!(
                "Permission Denial: unbroadcastIntent() from pid={pid}, uid={uid} requires {}",
                permission::BROADCAST_STICKY
            );
            log(format_args!("{msg}"));
            return Err(security(&msg));
        }
        let Some(intent) = intent else {
            return Ok(());
        };
        if intent.has_file_descriptors() {
            return Err(illegal_argument("File descriptors passed in Intent"));
        }
        let key = intent_key(intent, None);
        self.inner
            .state()
            .broadcasts
            .remove_sticky(incoming_user(user_id), &key);
        Ok(())
    }

    /// BroadcastController.unregisterReceiver:618 -> unregisterReceiverTraced:647: the receiver's registrations go (no current ordered broadcast exists to finish: P1 has none).
    fn r#unregisterReceiver(
        &self,
        receiver: Option<&Strong<dyn IIntentReceiver>>,
    ) -> binder::Result<()> {
        if let Some(r) = receiver {
            let removed = self
                .inner
                .state()
                .broadcasts
                .unregister(&ReceiverHandle(r.clone()));
            if !removed {
                atl_diag::answered(AM, "unregisterReceiver: not registered");
            }
        }
        Ok(())
    }

    /// ActivityManagerService.finishReceiver: a manifest receiver of a non-ordered broadcast reports done (scheduleReceiver's assumeDelivered is false). Nothing waits on it in P1 (no timeouts, no
    /// ordered queue): counted.
    fn r#finishReceiver(
        &self,
        _who: Option<&SpIBinder>,
        _result_code: i32,
        _result_data: Option<&str>,
        _map: Option<&Bundle>,
        _abort_broadcast: bool,
        _flags: i32,
    ) -> binder::Result<()> {
        atl_diag::answered(AM, "finishReceiver (non-ordered: nothing waits)");
        Ok(())
    }

    /// ActivityManagerService.registerReceiverWithFeature -> BroadcastController.registerReceiverWithFeature:249 -> registerReceiverWithFeatureTraced:296: the caller's process is looked up by its thread
    /// (unknown: logged, null, :312-315); a caller package the process does not run is a SecurityException (:316-320; core uids, below 10000, are exempt: UserHandle.isCore). Then the export flags
    /// (:428-472, broadcast::register_export), the matching stickies (:378-395, :474-509), the first of which is returned; a null receiver stops there (the sticky read). Else the registration is kept
    /// (:530-579) and every matching sticky is queued to it as an initial sticky (:582-604); the first one the receiver may see (exported, or the sticky's sender is its uid / system) is returned.
    /// The filter was decoded, so a filter with parts the codec does not model was refused before it got here. Not modelled: the protected-broadcast list (a missing export flag at targetSdk >= 34 is
    /// counted, not refused), instant apps, MAX_RECEIVERS_ALLOWED_PER_APP, priorities.
    fn r#registerReceiverWithFeature(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        caller_package: Option<&str>,
        _calling_feature_id: Option<&str>,
        _receiver_id: Option<&str>,
        receiver: Option<&Strong<dyn IIntentReceiver>>,
        filter: Option<&IntentFilter>,
        required_permission: Option<&str>,
        user_id: i32,
        flags: i32,
    ) -> binder::Result<Option<Intent>> {
        let inner = &self.inner;
        let mut st = inner.state();
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
        let (pid, uid, package) = (rec.pid, rec.uid, rec.package.clone());
        let Some(filter) = filter else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("registerReceiver: null IntentFilter"),
            ));
        };
        let target_sdk = inner
            .packages
            .by_name(&package)
            .map_or(0, |p| p.app().target_sdk_version);
        let export = broadcast::register_export(flags, receiver.is_some(), target_sdk)
            .map_err(|m| illegal_argument(&m))?;
        if export.flag_missing_unchecked {
            atl_diag::answered(
                AM,
                "registerReceiver without RECEIVER_EXPORTED / NOT_EXPORTED at targetSdk >= 34 (protected-broadcast list not loaded: taken as exported)",
            );
        }
        let user = incoming_user(user_id);
        let filter = filter_of(filter);
        let stickies = st.broadcasts.stickies(&filter, user);
        let Some(receiver) = receiver else {
            return Ok(stickies.into_iter().next().map(|s| s.intent));
        };
        let bf = BroadcastFilter {
            filter,
            package,
            required_permission: required_permission.map(str::to_owned),
            exported: export.exported,
        };
        let handle = ReceiverHandle(receiver.clone());
        if !st
            .broadcasts
            .register(handle, pid, uid, user, bf)
            .map_err(|m| illegal_argument(&m))?
        {
            atl_diag::answered(AM, "registerReceiver: filter already registered");
        }
        let mut first = None;
        for s in stickies {
            if first.is_none()
                && (export.exported
                    || s.calling_uid == uid
                    || permission::is_system_or_root(s.calling_uid))
            {
                first = Some(s.intent.clone());
            }
            let job = Job::Registered {
                receiver: receiver.clone(),
                intent: s.intent,
                result_code: 0,
                data: None,
                extras: None,
                initial_sticky: true,
                user: broadcast::USER_ALL,
            };
            inner.deliver_locked(&mut st, pid, job);
        }
        Ok(first)
    }
}

/// The size buckets each activity reported (ActivityRecord.setSizeConfigurations, ActivityClientController.java:333-342), by the activity's token. A report of none forgets the activity's.
#[derive(Default)]
struct ReportedBuckets {
    entries: Mutex<Vec<(SpIBinder, SizeBuckets)>>,
}

/// The parcelable's thresholds, as size_buckets.rs reads them.
fn buckets_of(b: &SizeConfigurationBuckets) -> SizeBuckets {
    SizeBuckets {
        horizontal: b.horizontal.clone(),
        vertical: b.vertical.clone(),
        smallest: b.smallest.clone(),
        screen_layout_size: b.screen_layout_size.clone(),
        screen_layout_long_set: b.screen_layout_long_set,
    }
}

impl ReportedBuckets {
    /// Keeps `buckets` for `token`, replacing what it reported before.
    fn set(&self, token: &SpIBinder, buckets: Option<SizeBuckets>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(t, _)| t != token);
        if let Some(buckets) = buckets {
            entries.push((token.clone(), buckets));
        }
    }

    /// What `token` reported last; None when it reported none.
    fn get(&self, token: &SpIBinder) -> Option<SizeBuckets> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .find(|(t, _)| t == token)
            .map(|(_, b)| b.clone())
    }
}

/// IActivityClientController, handed to the app in LaunchActivityItem (ActivityTaskSupervisor.java:1028 activityClientController). `buckets` holds what each activity reports. `inner` is weak: Inner owns this binder.
struct ActivityClientController {
    buckets: Arc<ReportedBuckets>,
    inner: Weak<Inner>,
    /// the apps' UI agents, by uid: a move to the back minimizes the app's window through its agent
    agents: Arc<UiAgents>,
    /// false: onBackPressed sends no requestFinish (`--no-back-finish`, the dialog gate's back sub-gate's deliberate break); a Back then never finishes an activity.
    back_finish: bool,
    /// what an unhandled Back on a launcher root does
    root_back: RootBack,
}

impl ActivityClientController {
    /// moveActivityTaskToBack for the app of `uid`: its UI agent minimizes the window (the desktop's "back": the Task stays, behind the others). Logged as `onBackPressed: moveTaskToBack uid=<uid>`.
    /// No agent (headless, or not registered yet): counted, nothing else; the activity stays.
    fn task_to_back(&self, uid: u32) {
        log(format_args!("onBackPressed: moveTaskToBack uid={uid}"));
        let Some(agent) = self
            .agents
            .get(uid)
            .and_then(|b| b.into_interface::<dyn IUiAgent>().ok())
        else {
            atl_diag::answered(ACC, "onBackPressed: moveTaskToBack, no UI agent");
            return;
        };
        atl_diag::answered(ACC, "onBackPressed: moveTaskToBack");
        if let Err(e) = agent.minimize() {
            log(format_args!("onBackPressed: minimize: {e:?}"));
        }
    }
}

impl Interface for ActivityClientController {}

impl IActivityClientController for ActivityClientController {
    /// ActivityClientController.activityResumed:200-206 -> ActivityRecord.activityResumedLocked:6173, which finds the record by token, then clears its saved state, sets its splash-exit
    /// animation and handles size compat. None of that state exists here yet: the resume is logged by activity_task (`services: activity resumed <class> at=<stamp>`), which run-apk reads as onResume of the last one.
    fn r#activityResumed(
        &self,
        token: Option<&SpIBinder>,
        _handle_splash_screen_exit: bool,
    ) -> binder::Result<()> {
        if let Some(token) = token {
            let known = self
                .inner
                .upgrade()
                .is_some_and(|inner| inner.atm.resumed(token));
            if !known {
                atl_diag::answered(ACC, "activityResumed: no such activity");
            }
        }
        Ok(())
    }

    /// ActivityClientController.activityPaused (synchronous): the pause ack moves the record on, and what that unblocks is sent by activity_task. Counted when no record has the token.
    fn r#activityPaused(&self, token: Option<&SpIBinder>) -> binder::Result<()> {
        let known = match (token, self.inner.upgrade()) {
            (Some(token), Some(inner)) => inner.atm.paused(token),
            _ => false,
        };
        if !known {
            atl_diag::answered(ACC, "activityPaused: no such activity");
        }
        Ok(())
    }

    /// ActivityClientController.activityStopped (oneway): the stop ack. Counted when no record has the token.
    fn r#activityStopped(
        &self,
        token: Option<&SpIBinder>,
        _state: Option<&Bundle>,
        _persistent_state: Option<&binder::PersistableBundle>,
        _description: Option<&atl_parcelables::java::lang::CharSequence>,
    ) -> binder::Result<()> {
        let known = match (token, self.inner.upgrade()) {
            (Some(token), Some(inner)) => inner.atm.stopped(token),
            _ => false,
        };
        if !known {
            atl_diag::answered(ACC, "activityStopped: no such activity");
        }
        Ok(())
    }

    /// ActivityClientController.activityDestroyed (oneway): nothing to do, the record left the stack when its Destroy was sent. Counted.
    fn r#activityDestroyed(&self, _token: Option<&SpIBinder>) -> binder::Result<()> {
        atl_diag::answered(ACC, "activityDestroyed (the record left at its Destroy)");
        Ok(())
    }

    /// ActivityClientController.finishActivity (synchronous): the record is finishing with its result (code, data), true when the token names one. finishTask (finishAndRemoveTask) is not modelled: refused, counted.
    fn r#finishActivity(
        &self,
        token: Option<&SpIBinder>,
        code: i32,
        data: Option<&Intent>,
        finish_task: i32,
    ) -> binder::Result<bool> {
        let (Some(token), Some(inner)) = (token, self.inner.upgrade()) else {
            atl_diag::answered(ACC, "finishActivity: no token");
            return Ok(false);
        };
        if finish_task != 0 {
            atl_diag::answered(ACC, "finishActivity (finishTask not modelled: refused)");
            return Ok(false);
        }
        Ok(inner.atm.finish(token, code, data.cloned()))
    }

    /// ActivityClientController.onBackPressed:1720-1747 (oneway, AIDL). The app calls it only for a Back it left unhandled (Activity.onBackInvoked after its own callbacks, OnBackPressedDispatcher
    /// and dialogs declined it). The launcher root of its Task moves the Task back (shouldMoveTaskToBack, :1736-1739): its window is minimized and the app keeps running, unless the policy is
    /// [`RootBack::Close`]. Any other record gets requestFinish (:1744), and the app finishes it. No task organizer (:1728-1735).
    fn r#onBackPressed(
        &self,
        token: Option<&SpIBinder>,
        callback: Option<&Strong<dyn IRequestFinishCallback>>,
    ) -> binder::Result<()> {
        let inner = self.inner.upgrade();
        let back = match (token, &inner) {
            (Some(token), Some(inner)) => inner.atm.back(token),
            _ => None,
        };
        match back.map(|b| under_policy(b, self.root_back)) {
            Some(Back::TaskToBack { uid }) => self.task_to_back(uid),
            Some(Back::Finish) if !self.back_finish => {
                atl_diag::refused(ACC, "onBackPressed (--no-back-finish)");
            }
            known => {
                back_pressed(known.is_some(), callback.map(|c| &**c));
            }
        }
        Ok(())
    }

    /// ActivityClientController.getDisplayId:626-635: the root task's display, DEFAULT_DISPLAY when invalid or when the token has no task. M2 has one display.
    fn r#getDisplayId(&self, _token: Option<&SpIBinder>) -> binder::Result<i32> {
        Ok(DEFAULT_DISPLAY)
    }

    /// ActivityClientController.activityRelaunched (ActivityThread.reportRelaunch): the relaunch ended. Oneway, so a token-less call is only counted.
    fn r#activityRelaunched(&self, token: Option<&SpIBinder>) -> binder::Result<()> {
        let (Some(token), Some(inner)) = (token, self.inner.upgrade()) else {
            atl_diag::answered(ACC, "activityRelaunched: no token");
            return Ok(());
        };
        atl_diag::answered(ACC, "activityRelaunched");
        inner.relaunch_finished(token);
        Ok(())
    }

    /// ActivityClientController.reportSizeConfigurations:333-342: the activity's size buckets, kept under its token for the size changes after it (filterDiff reads them there). The app sends it oneway, so a token-less report is only counted.
    fn r#reportSizeConfigurations(
        &self,
        token: Option<&SpIBinder>,
        sizes: Option<&SizeConfigurationBuckets>,
    ) -> binder::Result<()> {
        let Some(token) = token else {
            atl_diag::answered(ACC, "reportSizeConfigurations: no token");
            return Ok(());
        };
        atl_diag::answered(ACC, "reportSizeConfigurations (kept per activity)");
        self.buckets.set(token, sizes.map(buckets_of));
        Ok(())
    }

    /// ActivityClientController.setTaskDescription: Activity.onApplyThemeResource sends it for every themed activity, so the launch fails without it. The values are read (the codec
    /// checks them) and not kept: the task description is for the recents, which ATL does not have yet. Counted, so the run shows the call.
    fn r#setTaskDescription(
        &self,
        _token: Option<&SpIBinder>,
        _values: Option<&ActivityManager_TaskDescription>,
    ) -> binder::Result<()> {
        atl_diag::answered(ACC, "setTaskDescription (not kept: no recents)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use binder::StatusCode;

    use super::*;

    /// Counts requestFinish calls; `fail` answers each with DEAD_OBJECT (the app died before the oneway call landed).
    struct Callback {
        calls: AtomicU32,
        fail: bool,
    }

    impl Callback {
        fn new(fail: bool) -> Self {
            Callback {
                calls: AtomicU32::new(0),
                fail,
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl Interface for Callback {}

    impl IRequestFinishCallback for Callback {
        fn r#requestFinish(&self) -> binder::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(StatusCode::DEAD_OBJECT.into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn unknown_token_sends_nothing() {
        let cb = Callback::new(false);
        assert!(!back_pressed(false, Some(&cb)));
        assert_eq!(cb.calls(), 0);
    }

    #[test]
    fn known_token_sends_one_request_finish() {
        let cb = Callback::new(false);
        assert!(back_pressed(true, Some(&cb)));
        assert_eq!(cb.calls(), 1);
    }

    #[test]
    fn failed_send_is_logged_not_a_panic() {
        let cb = Callback::new(true);
        assert!(back_pressed(true, Some(&cb)));
        assert_eq!(cb.calls(), 1);
    }

    #[test]
    fn no_callback_sends_nothing() {
        assert!(!back_pressed(true, None));
    }

    /// Minimize keeps the move to the back; Close turns it into the pre-Android-12 finish; a plain finish stays a finish under both.
    #[test]
    fn root_back_policy() {
        let back = Back::TaskToBack { uid: 10_001 };
        assert_eq!(under_policy(back, RootBack::Minimize), back);
        assert_eq!(under_policy(back, RootBack::Close), Back::Finish);
        assert_eq!(under_policy(Back::Finish, RootBack::Minimize), Back::Finish);
        assert_eq!(under_policy(Back::Finish, RootBack::Close), Back::Finish);
    }
}
