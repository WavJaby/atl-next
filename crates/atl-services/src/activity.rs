// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java, frameworks/base/services/core/java/com/android/server/am/ProcessList.java, frameworks/base/services/core/java/com/android/server/am/InstrumentationReporter.java, frameworks/base/services/core/java/com/android/server/am/ContentProviderHelper.java, frameworks/base/services/core/java/com/android/server/am/ContentProviderRecord.java, frameworks/base/services/core/java/com/android/server/wm/ActivityTaskSupervisor.java, frameworks/base/services/core/java/com/android/server/wm/ActivityClientController.java
//! IActivityManager + the ActivityTaskManager launch path + IActivityClientController, subset: a process the services started attaches (attachApplication -> bindApplication), finishes attaching
//! (finishAttachApplication -> scheduleTransaction([LaunchActivityItem, ResumeActivityItem]) of its launcher activity), asks for the settings provider (getContentProvider) and its display (getDisplayId);
//! instrumentation runs (startInstrumentation, addInstrumentationResults, finishInstrumentation, a crash or kill of the instrumented process), and broadcasts, P1 (BroadcastController's IActivityManager
//! half: registerReceiverWithFeature / unregisterReceiver, non-ordered broadcastIntentWithFeature to registered receivers and explicit-component manifest receivers, stickies, unbroadcastIntent;
//! the rules are broadcast.rs, the sends broadcast_queue.rs). A manifest receiver whose process is not running gets it started headless (no activity launch); its deliveries wait for the attach.
//! Processes are started and killed through [`ProcessControl`] (the zygote / Process.killProcess seam); a crash report (handleApplicationCrash) is logged; every other method is counted unimplemented (counters).
//! PendingIntent records (getIntentSenderWithFeature, cancelIntentSender) are pending_intent.rs's; IIntentSender.send of a BROADCAST record is send_pending_broadcast (below), of a SERVICE record start_service_as (active_services.rs); the activity kinds are counted, not modelled.
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
use atl_aidl::aidl::android::app::IServiceConnection::IServiceConnection;
use atl_aidl::aidl::android::app::IUiAutomationConnection::IUiAutomationConnection;
use atl_aidl::aidl::android::content::IIntentReceiver::IIntentReceiver;
use atl_aidl::aidl::android::content::IIntentSender::IIntentSender;
use atl_broker::control::StartRequest;
use atl_parcelables::android::app::servertransaction::ClientTransaction;
use atl_parcelables::android::app::{
    ActivityManager_MemoryInfo, ActivityManager_PendingIntentInfo,
    ActivityManager_ProcessErrorStateInfo, ActivityManager_RunningAppProcessInfo,
    ActivityManager_TaskDescription, ApplicationErrorReport_ParcelableCrashInfo,
    ContentProviderHolder, Notification,
};
use atl_parcelables::android::content::pm::{ActivityInfo, ProviderInfo, ProviderInfoList};
use atl_parcelables::android::content::res::{CompatibilityInfo, Configuration};
use atl_parcelables::android::content::{ComponentName, Intent, IntentFilter};
use atl_parcelables::android::graphics::Rect;
use atl_parcelables::android::os::Bundle;
use atl_parcelables::android::util::MergedConfiguration;
use atl_parcelables::android::window::SizeConfigurationBuckets;
use atl_parcelables::codecs::android::app::servertransaction::ClientTransaction::ClientTransactionItem;
use atl_parcelables::codecs::java::util::Map::Value as MapValue;
use atl_parcelables::java::io::FileDescriptor;
use atl_parcelables::java::util::Map;
use atl_parcelables::parts::{
    ActivityConfigurationChangeItem, ActivityRelaunchItem, ActivityWindowInfo,
    ConfigurationChangeItem, LaunchActivityItem, ResumeActivityItem,
};
use atl_services_types::hw_timeouts::{
    HwTimeouts, KILL_TASK_PROCESSES_TIMEOUT, PROC_START_TIMEOUT_BASE_MS,
};
use binder::{
    BinderFeatures, DeathRecipient, ExceptionCode, IBinder, Interface, SpIBinder, Status, Strong,
    ThreadState,
};

use crate::activity_start::{self, Refusal};
use crate::activity_task::{ActivityTasks, Back};
use crate::am_handler::AmHandler;
use crate::broadcast::{
    self, BroadcastFilter, Broadcasts, ExportRefusal, IntentKey, StickyRefusal,
};
use crate::broadcast_dispatch::{Outcome, ProcKey};
use crate::broadcast_queue;
use crate::config_diff::{self, Config, Delivery};
pub use crate::desktop_bounds::OrientationReshape;
use crate::desktop_bounds::{self, ActivityInput, reshaped_size};
use crate::memory_levels;
use crate::package;
use crate::packages::{PackageRecord, PackageTable, Packages, Permissions};
use crate::pattern_matcher;
use crate::pending_intent::{self, IntentSink, PendingIntents};
use crate::pending_intent_table::Key;
use crate::process_control::ProcessControl;
use crate::running_processes;
use crate::shared_memory::ApplicationSharedMemory;
use crate::tasks::{TaskId, TaskSize, Tasks};
use crate::ui_agents::{AppProcess, UiAgents};
use crate::uid_state::{self, ProcessState, Settle, UidView};
use atl_services_types::display_model::DisplayModel;
use atl_services_types::packages::SCREEN_ORIENTATION_UNSPECIFIED;
use atl_services_types::permission::{self, ROOT_UID, SHELL_UID, SYSTEM_UID};
use atl_services_types::size_buckets::SizeBuckets;
use atl_services_wire::ui_agent::IUiAgent;

#[path = "active_services.rs"]
mod active_services;
#[path = "broadcasts_am.rs"]
mod broadcasts_am;

use broadcasts_am::{Dispatch, SystemStep};

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
/// There is no SystemUI here, so every app takes AOSP's own disabled-change path (maintainer 2026-11-05, kept 2026-11-06): Toast.makeText builds the view (Toast.java:523, ToastPresenter.getTextToastView); Toast.show's else branch (:199-215) sends enqueueToast with the app's TN;
/// notification.rs answers TN.show(windowToken), and TN.handleShow (:758-792) adds the TYPE_TOAST window through ToastPresenter.show in the app. App-visible difference: Toast.getView() is non-null for text toasts. Delete this when a system UI renders enqueueTextToast.
const DISABLED_COMPAT_CHANGES: [i64; 1] = [147_798_919];
/// Intent.FLAG_RECEIVER_BOOT_UPGRADE (Intent.java:7647): refused in a broadcast from an app.
const FLAG_RECEIVER_BOOT_UPGRADE: i32 = 0x0200_0000;
/// ActivityManager.BROADCAST_SUCCESS (ActivityManager.java:639).
const BROADCAST_SUCCESS: i32 = 0;
/// The locale of the global configuration when the host gives no UI language (`--locale` absent): what Android uses when neither `persist.sys.locale` nor `ro.product.locale` is set (LocaleList's default,
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

/// The global configuration a started app gets (ActivityTaskManagerService's `getGlobalConfiguration`), from the display model: font scale 1 (the `font_scale` setting's default), the locale (`locale`, the host's UI language tag; None: DEFAULT_LOCALE),
/// and the display's density and size. The size is DisplayContent.computeScreenAppConfiguration:2462-2468 for the one display: dp = px / (dpi / 160), rounded; portrait when width <= height.
/// The input fields stay undefined (0) until the platform layer supplies input. Shared with window.rs (the MergedConfiguration of a relayout).
pub(crate) fn global_configuration(display: &DisplayModel, locale: Option<&str>) -> Configuration {
    let density = display.dpi as f32 / DENSITY_DEFAULT;
    let width_dp = (display.width as f32 / density + 0.5) as i32;
    let height_dp = (display.height as f32 / density + 0.5) as i32;
    Configuration {
        font_scale: 1.0,
        locale_list: Some(locale.unwrap_or(DEFAULT_LOCALE).to_owned()),
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
        // DisplayContent.computeScreenConfiguration:2486-2488: bounds = maxBounds = the display; computeScreenAppConfiguration:2458-2460: appBounds = the non-decor frame (no decor here
        // = the display) and the display's rotation as both. A process's resources (application context) carry this: ViewConfiguration's drawing-cache limit is 4 x maxBounds area
        // (ViewConfiguration.java:485), so empty maxBounds -> limit 0 -> every software-layer view "not displayed" (AB2's settings WebView).
        window_configuration: atl_parcelables::parts::WindowConfiguration {
            bounds: display_rect(display),
            app_bounds: Some(display_rect(display)),
            max_bounds: display_rect(display),
            rotation: display.rotation,
            display_rotation: display.rotation,
            ..Default::default()
        },
        ..Configuration::default()
    }
}

/// The display's logical rect from the origin (DisplayContent.computeScreenConfiguration's mTmpRect).
fn display_rect(display: &DisplayModel) -> Rect {
    Rect {
        left: 0,
        top: 0,
        right: display.width,
        bottom: display.height,
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

/// `task`'s override as sent to its activities and windows (launch, relayout, resized, configuration change): task_override at `size`, stamped with the Task's configuration seq.
pub(crate) fn sent_task_override<C: Clone + PartialEq>(
    tasks: &Tasks<C>,
    display: &DisplayModel,
    task: TaskId,
    size: TaskSize,
) -> Configuration {
    Configuration {
        seq: tasks.config_seq(task),
        ..task_override(display, size)
    }
}

/// The configuration of a Task of `size` on `display`: the global configuration with the Task's override (the fields the override sets replace the global ones).
pub(crate) fn task_configuration(
    display: &DisplayModel,
    locale: Option<&str>,
    size: TaskSize,
) -> Configuration {
    let over = task_override(display, size);
    let mut config = global_configuration(display, locale);
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

/// The configuration of the process `pid` (WindowProcessController.getConfiguration): the configuration of the Task it follows ([`Tasks::process_task`], its activity launched last, kept by
/// activity_task), else the global one. The one source of bindApplication's, LaunchActivityItem.curConfig, ConfigurationChangeItem and the global part of a window's MergedConfiguration.
/// activityType stays undefined and seq the global's (0), as WindowProcessController.resolveOverrideConfiguration leaves them.
pub(crate) fn process_configuration<C: Clone + PartialEq>(
    tasks: &Tasks<C>,
    pid: i32,
) -> Configuration {
    let display = tasks.display();
    match tasks.process_task(pid).and_then(|t| tasks.size(t)) {
        Some(size) => task_configuration(&display, tasks.locale(), size),
        None => global_configuration(&display, tasks.locale()),
    }
}

/// WindowProcessController.dispatchConfiguration:1770: a process configuration as its ConfigurationChangeItem (device 0: the default display's).
pub(crate) fn configuration_change_item(configuration: Configuration) -> ClientTransactionItem {
    ClientTransactionItem::ConfigurationChange(Box::new(ConfigurationChangeItem {
        configuration,
        device_id: DEVICE_ID_DEFAULT,
    }))
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

/// The LaunchActivityItem of `p`'s `activity` (ActivityTaskSupervisor.realStartActivityLocked's launch): `intent` as the start sent it, the controller every activity names. Built by activity_task when the machine's Launch step is sent.
/// `process_config` = curConfig, the process's configuration (prepareConfigurationForLaunchingActivity:1868, [`process_configuration`]), already following this activity;
/// `task_override` = the activity's merged override configuration (ActivityTaskSupervisor.java:1001 `r.getMergedOverrideConfiguration()`): its freeform Task's, so onCreate lays out at the Task's size, not the display's.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_item(
    packages: &Packages,
    process_config: Configuration,
    task_override: Configuration,
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
        cur_config: Some(process_config),
        override_config: Some(task_override),
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

/// ContentProviderRecord, reduced: a provider some caller asked for, from its process start until its process dies.
struct ProviderRecord {
    package: String,
    class: String,
    /// the process that hosts it (ProviderInfo.processName)
    process: String,
    /// resolved for the stock PackageManager flags (package::process_providers)
    info: ProviderInfo,
    /// IContentProvider, once publishContentProviders delivered it
    provider: Option<SpIBinder>,
    /// the publishing process
    pid: Option<i32>,
    /// callers told at publication (ContentProviderConnection.waiting)
    waiters: Vec<Strong<dyn IApplicationThread>>,
}

impl ProviderRecord {
    /// ContentProviderRecord.newHolder (ContentProviderRecord.java:89-96), no connection; noReleaseNeeded as its constructor (:75-76): a root or system uid provider, Settings app excepted.
    fn holder(&self) -> ContentProviderHolder {
        let uid = self.info.component.application_info.uid as u32;
        ContentProviderHolder {
            info: self.info.clone(),
            provider: self.provider.clone(),
            connection: None,
            no_release_needed: permission::is_system_or_root(uid)
                && self.package != SETTINGS_APP_PACKAGE,
            local: false,
        }
    }
}

/// A caller waiting on a provider that will not be published: told publish status false after the lock (Inner::deliver).
struct LostWaiter {
    thread: Strong<dyn IApplicationThread>,
    holder: ContentProviderHolder,
    authority: String,
}

/// What a state change leaves for after the state lock: the pids to kill, and the waiters to tell (Inner::deliver).
#[derive(Default)]
struct Effects {
    pids: Vec<i32>,
    lost: Vec<LostWaiter>,
}

/// The Settings app, the exception to a system provider's noReleaseNeeded (ContentProviderRecord.java:76).
const SETTINGS_APP_PACKAGE: &str = "com.android.settings";

/// PMS resolveContentProvider, user 0: the first package with a provider of `authority` (ProviderInfo.authority is `;`-separated), resolved as process_providers does for its process.
fn resolve_provider<'a>(
    packages: &'a Packages,
    authority: &str,
) -> Option<(&'a PackageRecord, ProviderInfo)> {
    let has = |d: &ProviderInfo| {
        d.authority
            .as_deref()
            .is_some_and(|a| a.split(';').any(|x| x == authority))
    };
    packages.all().iter().find_map(|p| {
        let process = p
            .providers()
            .find(|d| has(d))?
            .component
            .process_name
            .clone()?;
        package::process_providers(p, packages, &process, package::STOCK_PM_FLAGS)
            .into_iter()
            .find(|d| has(d))
            .map(|d| (p, d))
    })
}

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
    /// finishAttachApplication ran: the process takes its deliveries and service calls (the attach is complete; its thread alone is not enough)
    attach_finished: bool,
    /// elapsed ms of the last holding change ([`Inner::touch_all_locked`]): the headless grace runs from here while nothing holds the process (uid_state::ends)
    touched: u64,
    /// why the activity service ended this process; None while it lives. Read by its death (services_died: restart or not)
    kill_reason: Option<KillReason>,
}

/// Why the activity service ended a process it started: a window's close (removeTask, M29), or the empty-headless end rule (uid_state::ends).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KillReason {
    WindowClosed,
    EmptyHeadless,
}

/// AMC DEFAULT_SERVICE_START_FOREGROUND_TIMEOUT_MS (ActivityManagerConstants.java:318, 30 s): a startForegroundService's service must call startForeground within it.
const SERVICE_START_FOREGROUND_TIMEOUT_MS: u64 = 30_000;
/// AMC DEFAULT_SERVICE_START_FOREGROUND_ANR_DELAY_MS (ActivityManagerConstants.java:320, 10 s): the ANR follows a startForeground timeout this long after its stop (AS :7640).
const SERVICE_START_FOREGROUND_ANR_DELAY_MS: u64 = 10_000;

/// The activity service's delayed messages (am_handler.rs), keyed by what each concerns.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Timer {
    /// removeTask's kill (KILL_TASK_PROCESSES_TIMEOUT): the Task's processes end unless its destroy came first
    KillTask(TaskId),
    /// a uid's settle sweep (uid_state, OomAdjuster idleUids at bgTime + 60 s)
    UidSettle(u32),
    /// an empty headless process's grace, by ProcessRecord.start_seq
    EmptyProcess(i64),
    /// a service's restart (service_records.rs `Restart.at`, ActiveServices.scheduleServiceRestartLocked's restarter): one per component, re-armed by each scheduled restart
    Restart((String, String)),
    /// a process's start timeout (PROC_START_TIMEOUT_BASE_MS x the multiplier), by ProcessRecord.start_seq: a process still unattached when it fires failed to attach; the one place an unattached process ends
    ProcessStart(i64),
    /// a broadcast delivery's timeout (BROADCAST_FG / BG_TIMEOUT): the delivery (pid, broadcast, receiver index); broadcasts_am.rs ignores a delivery that ended
    Broadcast { pid: i32, id: u64, index: usize },
    /// a cold start's backstop (hw_timeouts: PENDING_COLD_START_ABANDON_BASE_MS x the multiplier): the start of process `key` named by its token; releases the broadcast queue's slot, ends no process
    ColdStart { key: ProcKey, token: u64 },
    /// a temp allowlist grant's expiry, by uid (uid_state `on_temp_expired`): the uid settles at the deadline that returns (`UidSettle`)
    TempExpiry(u32),
    /// a started foreground service's startForeground deadline (SERVICE_START_FOREGROUND_TIMEOUT_MS), by component; armed when the start is delivered (AS :6199-6204)
    FgsStart((String, String)),
    /// a startForeground timeout's ANR, 10 s after the stop: the host pid of the process that ran the service (AS :7640)
    FgsAnr(i32),
    /// a launch held behind its package's first-start boot broadcasts, released (due now) by BOOT_COMPLETED's final result: the launch runs here, off the state lock; by start_seq
    HeldLaunch(i64),
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
    /// BroadcastController's registrations and stickies
    broadcasts: Broadcasts<ReceiverHandle, Intent>,
    /// every broadcast's delivery state and the process queues (broadcast_dispatch.rs); the deliveries that wait for a process are its
    dispatcher: Dispatch,
    /// ActiveServices' service records (service_records.rs)
    services: active_services::Services,
    /// ProviderMap: the providers asked for, starting or published
    providers: Vec<ProviderRecord>,
    /// the uid state (uid_state.rs): each uid's launch Tasks, idle flag and settle sweep
    uids: uid_state::Uids,
    /// the launch Tasks counted in `uids`, with their uid: a Task leaves once (task_gone_locked)
    open_tasks: std::collections::BTreeMap<TaskId, u32>,
    /// PackageFreezer's mFrozenPackages: a package's freeze count; a frozen package starts nothing (start_process_locked)
    frozen: std::collections::BTreeMap<String, u32>,
    /// the packages that got the boot broadcasts at a first process start this run (D2 clause 2, broadcast::first_start_boot)
    booted: std::collections::BTreeSet<String>,
    /// processes (by start_seq) whose activity launch waits for their package's first-start BOOT_COMPLETED to finish (broadcasts_am.rs release_held_launches_locked)
    held_launches: std::collections::BTreeSet<i64>,
}

/// An IIntentReceiver as a registration key: equal when the binder is (mRegisteredReceivers is keyed by `receiver.asBinder()`). `System` is the services' own resultTo (AOSP's in-process
/// IIntentReceiver.Stub in UserController), run from the dispatcher's result, never a binder call.
#[derive(Clone)]
enum ReceiverHandle {
    Binder(Strong<dyn IIntentReceiver>),
    System(SystemStep),
}

impl PartialEq for ReceiverHandle {
    fn eq(&self, o: &Self) -> bool {
        match (self, o) {
            (Self::Binder(a), Self::Binder(b)) => a.as_binder() == b.as_binder(),
            (Self::System(a), Self::System(b)) => a == b,
            _ => false,
        }
    }
}

/// What the broadcast rules read of an Intent (broadcast::IntentKey); the categories as a set. resolvedType falls back to the Intent's own type (Intent.resolveTypeIfNeeded without a resolver).
pub(crate) fn intent_key(intent: &Intent, resolved_type: Option<&str>) -> IntentKey {
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

/// An IntentFilter (registered, or a manifest `<intent-filter>`) in the rules' form: actions, categories, schemes, scheme-specific parts, authorities, paths, types (mDataTypes).
pub(crate) fn filter_of(f: &IntentFilter) -> broadcast::Filter {
    let flat = |v: &Vec<Option<String>>| v.iter().flatten().cloned().collect::<Vec<_>>();
    broadcast::Filter {
        actions: flat(&f.actions),
        categories: f.categories.as_ref().map(flat),
        schemes: f.data_schemes.as_ref().map(flat),
        ssps: non_empty(&f.data_scheme_specific_parts, pattern_of),
        authorities: non_empty(&f.data_authorities, |a| broadcast::Authority {
            host: a.host.clone().unwrap_or_default(),
            wild: a.wild,
            port: a.port,
        }),
        paths: non_empty(&f.data_paths, pattern_of),
        types: f.data_types.as_ref().map(flat),
    }
}

/// The codec reads a count of 0 as an empty list; AOSP writes null and an empty list alike (IntentFilter.writeToParcel), so empty is null.
fn non_empty<T, U>(v: &[T], map: impl Fn(&T) -> U) -> Option<Vec<U>> {
    (!v.is_empty()).then(|| v.iter().map(map).collect())
}

/// A parcelled PatternMatcher. A null pattern is not a valid filter (AOSP's matchPattern throws on it); it reads as "".
fn pattern_of(p: &atl_parcelables::parts::PatternMatcher) -> pattern_matcher::Pattern {
    pattern_matcher::Pattern {
        pattern: p.pattern.clone().unwrap_or_default(),
        kind: p.kind,
        parsed: p.parsed_pattern.clone().unwrap_or_default(),
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

/// The paths a PendingIntent record sends through (pending_intent.rs): a BROADCAST record's send_broadcast and a SERVICE record's start, as the record's creator. Weak, so the records do not keep the service alive.
struct IntentRoute(Weak<Inner>);

impl IntentSink for IntentRoute {
    fn send_as(
        &self,
        intent: &Intent,
        uid: u32,
        user_id: i32,
        finished: Option<&Strong<dyn IIntentReceiver>>,
        options: &crate::pending_intent::SendOptions,
        caller: u32,
    ) -> binder::Result<()> {
        let inner = self
            .0
            .upgrade()
            .ok_or_else(|| illegal_state("PendingIntent send: the service is gone"))?;
        // BroadcastController :965-988: a temp allowlist request needs one of its permissions from the real caller
        if options.temp_requested
            && !crate::pending_intent::may_request_temp_allowlist(|p| inner.holds(caller, p))
        {
            atl_diag::refused(
                AM,
                "PendingIntent broadcast temp allowlist (permission denied)",
            );
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!(
                    "Permission Denial: broadcast temp allowlist from uid {caller} requires CHANGE_DEVICE_IDLE_TEMP_WHITELIST or START_ACTIVITIES_FROM_BACKGROUND or START_FOREGROUND_SERVICES_FROM_BACKGROUND"
                )),
            ));
        }
        // the finishedReceiver belongs to the process that called send (PendingIntentRecord.sendInner's resultToApp)
        let caller_pid = ThreadState::get_calling_pid();
        ActivityManager { inner }.send_pending_broadcast(
            intent,
            uid,
            user_id,
            finished.map(|h| (h, caller_pid)),
            options.grant,
        )
    }

    fn start_service_as(
        &self,
        intent: &Intent,
        uid: u32,
        _user_id: i32,
        foreground: bool,
        options: &crate::pending_intent::SendOptions,
        caller: u32,
    ) -> binder::Result<()> {
        let inner = self
            .0
            .upgrade()
            .ok_or_else(|| illegal_state("PendingIntent send: the service is gone"))?;
        // PendingIntentRecord :604-613: only a FOREGROUND_SERVICE send grants its options' allowlist, to the creator, and only for a caller that may request one (tempAllowlistForPendingIntentLocked :15723-15733)
        let grant = match options.grant {
            Some(grant)
                if foreground
                    && crate::pending_intent::may_request_temp_allowlist(|p| {
                        inner.holds(caller, p)
                    }) =>
            {
                Some(grant)
            }
            Some(_) if foreground => {
                atl_diag::answered(
                    AM,
                    "PendingIntent temp allowlist (caller holds no permission)",
                );
                None
            }
            _ => None,
        };
        inner.start_service_as(intent, uid, foreground, grant);
        Ok(())
    }
}

struct Inner {
    packages: Arc<PackageTable>,
    permissions: Arc<Permissions>,
    control: Arc<dyn ProcessControl>,
    /// `ro.hw_timeout_multiplier`: scales the process-start timer here and the broadcast timers in the dispatcher (hw_timeouts.rs)
    hw: HwTimeouts,
    state: Mutex<State>,
    reporter: Reporter,
    /// the activity lifecycle: records, launches and starts (activity_task.rs); the client controller forwards its acks here
    atm: Arc<ActivityTasks>,
    /// the Tasks: a launch that starts a Task creates it here and binds its activity's token to it (tasks.rs)
    tasks: Arc<Tasks<SpIBinder>>,
    /// what each activity reported (ReportedBuckets), shared with the activity client controller
    buckets: Arc<ReportedBuckets>,
    /// the broadcast sender thread (broadcast_queue.rs)
    broadcast_queue: broadcast_queue::Queue,
    /// the PendingIntent records (pending_intent.rs)
    pending: PendingIntents,
    /// the sender of the bound-service calls (active_services.rs)
    service_calls: active_services::Outbox,
    /// false is the test-only break (`--no-bind-service`): every bindService answers 0 (no such service), so ServiceConnection and the job runner never connect
    bind_services: bool,
    /// Removed Tasks whose processes wait for the top's activityDestroyed or the timeout (Task.mKillProcessesOnDestroyed)
    kill_armed: Mutex<std::collections::BTreeSet<TaskId>>,
    /// the activity service's delayed messages (am_handler.rs): removeTask's kill, the uid settle sweep, an empty headless process's grace
    handler: AmHandler<Timer, Timer>,
    /// the clock of the delayed messages: uid_state's times are elapsed ms from here ([`Inner::now_ms`])
    origin: std::time::Instant,
    /// the empty-headless grace, ms (`--empty-process-grace-ms`): a process nothing holds ends this long after its last holding change, with its uid idle
    empty_grace_ms: u64,
    /// `--window-close-stops-services`: a window's close ends its processes' services without a restart (AOSP restarts STICKY ones)
    window_close_stops_services: bool,
    /// the foreground service notifications (ServiceRecord.postNotification): posted and cancelled as the app's own (notification.rs)
    notices: Arc<dyn crate::notification::ForegroundNotices>,
    /// bindApplication's `services` (mAppBindArgs, [`common_services`]), built at the first bind
    common_services: std::sync::OnceLock<Map>,
}

/// ActivityManagerService.getCommonServicesLocked (:2793-2833, the non-isolated list; no isolated processes here), by Context constant value. ActivityThread puts the map in ServiceManager's
/// cache, so an app reaches these without a servicemanager lookup -- the only way for those its domain may not `find` (window, display: system_api_service, not app_api_service).
const COMMON_SERVICES: [&str; 21] = [
    "package",
    "permissionmgr",
    "window",
    "alarm",
    "display",
    "network_management",
    "connectivity",
    "accessibility",
    "input_method",
    "input",
    "graphicsstats",
    "appops",
    "content",
    "jobscheduler",
    "notification",
    "vibrator",
    "account",
    "power",
    "user",
    "mount",
    "platform_compat",
];

/// The map of [`COMMON_SERVICES`] as addServiceToMap fills it: ServiceManager.getService of each, an absent one skipped. Built once at the first bind, as mAppBindArgs is: atl-system-services
/// registers every service before `services: ready`, and no process starts before that, so no later registration is missed.
fn common_services() -> Map {
    Map {
        entries: COMMON_SERVICES
            .iter()
            .filter_map(|&name| {
                binder::check_service(name).map(|b| {
                    (
                        MapValue::String(Some(name.to_owned())),
                        MapValue::Binder(Some(b)),
                    )
                })
            })
            .collect(),
    }
}

/// The service; a clone shares it (one is registered with servicemanager, another starts the launcher package).
#[derive(Clone)]
pub struct ActivityManager {
    inner: Arc<Inner>,
}

/// A freeze of a package (PackageFreezer): while it lives the package starts nothing; its drop counts the freeze down (PackageFreezer.close :110-121).
pub struct Frozen {
    inner: Arc<Inner>,
    package: String,
}

impl Drop for Frozen {
    fn drop(&mut self) {
        let mut st = self.inner.state();
        if let Some(count) = st.frozen.get_mut(&self.package) {
            *count -= 1;
            if *count == 0 {
                st.frozen.remove(&self.package);
            }
        }
    }
}

impl crate::install::PackageStops for ActivityManager {
    fn freeze(&self, package: &str) -> Box<dyn Send> {
        Box::new(self.freeze_package(package))
    }
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
    /// moveTaskToBack, Android 12+ (maintainer 2026-11-12): the window is minimized, the app keeps running.
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
    /// `back_finish` false is the test-only break (`--no-back-finish`): see [`ActivityClientController::back_finish`]. `root_back`: see [`RootBack`]. `orientation_reshape`: see [`OrientationReshape`]. `bind_services` false is the test-only
    /// break `--no-bind-service`: every bindService answers 0. `hw`: the multiplier of the process-start and broadcast timers ([`HwTimeouts`]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        packages: Arc<PackageTable>,
        permissions: Arc<Permissions>,
        control: Arc<dyn ProcessControl>,
        tasks: Arc<Tasks<SpIBinder>>,
        agents: Arc<UiAgents>,
        back_finish: bool,
        root_back: RootBack,
        orientation_reshape: OrientationReshape,
        bind_services: bool,
        empty_grace_ms: u64,
        window_close_stops_services: bool,
        notices: Arc<dyn crate::notification::ForegroundNotices>,
        hw: HwTimeouts,
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
                        orientation_reshape,
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
                    hw,
                    state: Mutex::new(State {
                        dispatcher: Dispatch::with_hw(hw),
                        ..State::default()
                    }),
                    reporter: Reporter::new(),
                    tasks,
                    buckets,
                    broadcast_queue: broadcast_queue::Queue::new(),
                    pending: PendingIntents::new(Arc::new(IntentRoute(inner.clone()))),
                    service_calls: active_services::Outbox::new(),
                    bind_services,
                    kill_armed: Mutex::default(),
                    handler: {
                        let inner = inner.clone();
                        AmHandler::spawn("ActivityHandler", move |timer: Timer| {
                            if let Some(inner) = inner.upgrade() {
                                inner.on_timer(timer);
                            }
                        })
                        .expect("starting the ActivityHandler thread")
                    },
                    origin: std::time::Instant::now(),
                    empty_grace_ms,
                    window_close_stops_services,
                    notices,
                    common_services: std::sync::OnceLock::new(),
                }
            }),
        }
    }

    /// Starts the process of `package`'s launcher activity, which launches once the process has attached: what ActivityTaskManager does for a MAIN / LAUNCHER start (startActivity is not
    /// implemented, so atl-system-services calls this for its `--launch` package, the stand-in for the home screen's tap).
    pub fn start_launcher(&self, package: &str) -> Result<(), String> {
        let packages = self.inner.packages.snapshot();
        let p = packages
            .by_name(package)
            .ok_or_else(|| format!("no package {package}"))?;
        if packages.launcher(p).is_none() {
            return Err(format!("{package} has no launcher activity"));
        }
        let mut st = self.inner.state();
        self.inner
            .start_process_locked(&mut st, p, p.process_name(), true, None)
            .map_err(|e| format!("starting {package}: {e}"))
    }

    /// startActivity with no caller app thread (`am start`, a home screen): checked by activity_start::check_shell; accepted -> the package's process is started through the
    /// control and its launcher launched on attach (ActivityTaskManagerService.startProcessAsync, then realStartActivityLocked from attachApplication). Ok: the START code.
    /// The services' own bind (the job runner: JobServiceContext.executeRunnableJob's bindServiceAsUser): bindServiceLocked as the system uid. bindService's code: 1 bound, 0 no such service, -1 denied.
    pub fn bind_service_as_system(
        &self,
        intent: &Intent,
        conn: Strong<dyn IServiceConnection>,
        flags: i64,
    ) -> i32 {
        self.inner
            .bind_service_as_system(intent, conn, flags)
            .code()
    }

    /// unbindService of a connection [`Self::bind_service_as_system`] made; false when it had no bindings.
    pub fn unbind_service_as_system(&self, conn: Strong<dyn IServiceConnection>) -> bool {
        let mut st = self.inner.state();
        self.inner.unbind_service_locked(&mut st, conn)
    }

    pub(crate) fn start_without_caller(
        &self,
        calling_uid: u32,
        flags: i32,
        component: Option<(&str, &str)>,
    ) -> Result<i32, Status> {
        let packages = self.inner.packages.snapshot();
        let p = component.and_then(|(package, _)| packages.by_name(package));
        let launcher = p
            .and_then(|p| packages.launcher(p))
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
    /// PackageFreezer.freezePackage (:65-97): the count and the package's stop share one hold of the state lock, so no start can land between them. The kills and the
    /// waiters' answers follow once the lock is released. The returned guard's drop counts the freeze down.
    pub fn freeze_package(&self, package: &str) -> Frozen {
        let fx = {
            let mut st = self.inner.state();
            *st.frozen.entry(package.to_owned()).or_insert(0) += 1;
            self.inner.force_stop_package_locked(&mut st, package)
        };
        self.inner.deliver(fx);
        Frozen {
            inner: Arc::clone(&self.inner),
            package: package.to_owned(),
        }
    }

    pub fn activity_tasks(&self) -> Arc<ActivityTasks> {
        Arc::clone(&self.inner.atm)
    }

    /// processinfo's view (process_info.rs): each pid's process as the caller (binderd tag `calling_tag`) numbers it, read at one instant; None where no attached process of its sandbox is.
    /// Only the caller's own records can match (pid_seen_by answers 0 for another sandbox's), so only those have their holdings read.
    pub fn process_states(&self, calling_tag: i32, pids: &[i32]) -> Vec<Option<ProcessState>> {
        let st = self.inner.state();
        let own: Vec<(i32, Option<i32>, ProcessState)> = st
            .processes
            .iter()
            .filter(|r| r.pid == calling_tag)
            .filter_map(|r| {
                let a = r.attached.as_ref()?;
                let state = uid_state::process_state(&self.inner.holdings_locked(&st, r));
                Some((r.pid, a.program_pid, state))
            })
            .collect();
        pids.iter()
            .map(|&pid| running_processes::state_of_pid(own.iter().copied(), calling_tag, pid))
            .collect()
    }

    /// AMInternal.getUidProcessState and the uid observer history, for the shortcut and appops services (uid_state::UidView): the best state of `uid`'s attached processes, read at one instant.
    pub fn uid_view(&self, uid: u32) -> UidView {
        let st = self.inner.state();
        let state = st
            .processes
            .iter()
            .filter(|r| r.uid == uid && r.attached.is_some())
            .map(|r| uid_state::process_state(&self.inner.holdings_locked(&st, r)))
            .min();
        UidView {
            state,
            top_since: st.uids.top_since(uid),
            restricted_since: st.uids.restricted_since(uid),
            now: self.inner.now_ms(),
        }
    }

    /// The display changed (registry REPORT_MONITOR): every attached process gets its configuration again, as ActivityTaskManagerService.updateGlobalConfigurationLocked:4807-4813 does
    /// (each WindowProcessController's onConfigurationChanged -> dispatchConfiguration): the new global one, or its Task's on the new display ([`process_configuration`]). Needed: a Task's
    /// override leaves density to the global configuration (task_override), so without it an activity's merged configuration and the Application's resources keep the old density. Returns the processes told.
    pub(crate) fn global_configuration_changed(&self) -> usize {
        let pids: Vec<i32> = self
            .inner
            .state()
            .processes
            .iter()
            .filter(|p| p.attached.is_some())
            .map(|p| p.pid)
            .collect();
        self.inner.dispatch_process_configuration(&pids)
    }

    /// forceStopPackageLocked for a reason outside a binder call of this service (PMS's PACKAGE_CHANGED without DONT_KILL_APP): the package's processes leave the table and are killed.
    pub fn force_stop_package(&self, package: &str) {
        let fx = {
            let mut st = self.inner.state();
            self.inner.force_stop_package_locked(&mut st, package)
        };
        self.inner.deliver(fx);
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

    /// The process `task` was launched in ([`Self::task_of_process`]'s inverse): the process whose window shows the Task. None when none lives.
    pub(crate) fn process_of_task(&self, task: TaskId) -> Option<AppProcess> {
        self.inner.process_of_task(task)
    }

    /// ActivityTaskSupervisor.removeTask(killProcess=true), the desktop window's close (registry REMOVE_TASK): every activity of `task` finishes; its processes end once the shown top
    /// reported its destroy, at most [`KILL_TASK_PROCESSES_TIMEOUT`] later, or at once when the top was not shown. cleanUpServices is not separate: the kill ends the process's services
    /// (Q-B1: a foreground service does not keep it, M29).
    pub(crate) fn remove_task(&self, task: TaskId) {
        if !self.inner.atm.remove_task(task) {
            self.inner.kill_task_processes(task);
            return;
        }
        // AOSP posts the timeout only for a Task not armed yet (ActivityTaskSupervisor :1865-1869, !mKillProcessesOnDestroyed): a repeat removeTask keeps the first deadline
        let newly_armed = self
            .inner
            .kill_armed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(task);
        if newly_armed {
            let timer = Timer::KillTask(task);
            self.inner.handler.post(
                timer.clone(),
                timer,
                std::time::Instant::now() + KILL_TASK_PROCESSES_TIMEOUT,
            );
        }
    }
}

/// ApplicationInfo.FLAG_SYSTEM | FLAG_PERSISTENT (ApplicationInfo.java:189, :213; ActivityManagerService.PERSISTENT_MASK): the processes the empty rule never ends, and the callers' persistent check.
const PERSISTENT_MASK: i32 = crate::packages::FLAG_SYSTEM | 1 << 3;

impl Inner {
    /// activityDestroyed of `token`: false when no destroying record has it. The last record of a Task gone ends its processes: after a removeTask (the wait ends early, as
    /// killTaskProcessesOnDestroyedIfNeeded), and after the app finished its last activity itself (M29: ATL's stand-in for the low-memory killer ending a cached process).
    fn activity_destroyed(&self, token: &SpIBinder) -> bool {
        match self.atm.destroyed(token) {
            Some((task, true)) => self.kill_task_processes(task),
            Some((_, false)) => self.touch_all(),
            None => return false,
        }
        true
    }

    /// The timer of a removeTask: the processes end now unless the destroy came first (KILL_TASK_PROCESSES_TIMEOUT_MSG).
    fn kill_task_processes_on_timeout(&self, task: TaskId) {
        let armed = self
            .kill_armed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&task);
        if armed {
            log(format_args!(
                "remove task {}: no destroy within {KILL_TASK_PROCESSES_TIMEOUT:?} at={}",
                task.raw(),
                atl_diag::wall_stamp()
            ));
            self.kill_task_processes(task);
        }
    }

    /// ActivityTaskSupervisor.killTaskProcessesIfPossible: the processes of the Task's package and uid end, unless one of them holds an activity in another Task (then none ends).
    /// A foreground service does not keep them (D1 = M29: the process ends with its last window; the FGS notification's Stop action is notifications N6, not on main). Killed after the state lock is released.
    fn kill_task_processes(&self, task: TaskId) {
        self.kill_armed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&task);
        self.handler.remove(&Timer::KillTask(task));
        self.task_gone(task);
        let (Some(package), Some(uid)) = (self.tasks.package(task), self.tasks.owner(task)) else {
            return;
        };
        let pids: Vec<i32> = {
            let mut st = self.state();
            let held = st
                .processes
                .iter()
                .filter(|r| r.uid == uid && r.package == package)
                .any(|r| {
                    r.attached
                        .as_ref()
                        .is_some_and(|a| self.atm.holds_other_task(task, &a.thread.as_binder()))
                });
            if held {
                log(format_args!(
                    "task {} removed: a process of {package} holds another Task, kept",
                    task.raw()
                ));
                return;
            }
            let mut pids = Vec::new();
            for r in st
                .processes
                .iter_mut()
                .filter(|r| r.uid == uid && r.package == package)
            {
                r.kill_reason = Some(KillReason::WindowClosed);
                pids.push(r.pid);
            }
            pids
        };
        log(format_args!(
            "task {} removed: killing {package} pids {pids:?} at={}",
            task.raw(),
            atl_diag::wall_stamp()
        ));
        self.kill(&pids);
    }
    /// WindowProcessController.dispatchConfiguration:1743-1771 to each attached process of `pids`: its configuration now ([`process_configuration`]) as a ConfigurationChangeItem. Returns the processes told.
    fn dispatch_process_configuration(&self, pids: &[i32]) -> usize {
        let threads: Vec<_> = {
            let st = self.state();
            pids.iter()
                .filter_map(|pid| {
                    let p = st.processes.iter().find(|p| p.pid == *pid)?;
                    Some((*pid, p.attached.as_ref()?.thread.clone()))
                })
                .collect()
        };
        let mut told = 0;
        for (pid, thread) in threads {
            let transaction = ClientTransaction {
                items: vec![Some(configuration_change_item(process_configuration(
                    &self.tasks,
                    pid,
                )))],
            };
            match thread.r#scheduleTransaction(Some(&transaction)) {
                Ok(()) => told += 1,
                Err(e) => log(format_args!(
                    "scheduleTransaction (process configuration): {e}"
                )),
            }
        }
        told
    }

    /// ActivityRecord.ensureActivityConfiguration for each activity of `task`, at the Task's size now. Reported is recorded before the send, as AOSP's mLastReportedConfiguration.
    /// Each activity is a record of the lifecycle machine (activity_task Owner): one started by startActivity gets the change as the launcher one does; the steps after a relaunch come from its phase.
    /// First the processes that follow `task` get their new configuration: the activity's configuration change reaches its process's listener before the activity's own item (AOSP queues both).
    fn task_resized(&self, task: TaskId) {
        let Some(size) = self.tasks.size(task) else {
            return;
        };
        let told = self.dispatch_process_configuration(&self.tasks.processes_following(task));
        if told > 0 {
            log(format_args!(
                "process configuration of task {} sent to {told} process(es)",
                task.raw()
            ));
        }
        let display = self.tasks.display();
        let over = sent_task_override(&self.tasks, &display, task, size);
        let new = Config::from(&task_configuration(&display, self.tasks.locale(), size));
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
                                    // ActivityRecord.getProcessGlobalConfiguration:8617: the process's
                                    global: process_configuration(&self.tasks, entry.pid),
                                    override_config: over.clone(),
                                    merged: task_configuration(&display, self.tasks.locale(), size),
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

    /// The process `task` was launched in (the oldest live record whose launch made it): the process whose window shows the Task. None when none lives.
    fn process_of_task(&self, task: TaskId) -> Option<AppProcess> {
        self.state()
            .processes
            .iter()
            .find(|r| r.task == Some(task))
            .map(|r| AppProcess {
                uid: r.uid,
                pid: r.pid,
            })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Elapsed ms on the uid state's clock (origin = Inner construction): uid_state's deadlines are in these units. The services' clock is the same one (services_now).
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// now_ms as service_records.rs times (i64; only differences and order are read).
    fn services_now(&self) -> i64 {
        i64::try_from(self.now_ms()).unwrap_or(i64::MAX)
    }

    /// Replaces the pending message of `timer` with one due at `at_ms` (now_ms units).
    fn arm(&self, timer: Timer, at_ms: u64) {
        self.handler.remove(&timer);
        self.handler.post(
            timer.clone(),
            timer,
            self.origin + std::time::Duration::from_millis(at_ms),
        );
    }

    /// Arms the uid's settle sweep at the deadline uid_state returned, when it started one.
    fn arm_settle(&self, uid: u32, at: Option<u64>) {
        if let Some(at) = at {
            self.arm(Timer::UidSettle(uid), at);
        }
    }

    /// Every process's headless grace restarts now, with its timer re-armed. Conservative: a holding's release is not traced to its process, so every process is touched (a later end, never an earlier one).
    fn touch_all_locked(&self, st: &mut State) {
        let now = self.now_ms();
        for r in st.processes.iter_mut() {
            r.touched = now;
            self.arm(Timer::EmptyProcess(r.start_seq), now + self.empty_grace_ms);
        }
    }

    fn touch_all(&self) {
        let mut st = self.state();
        self.touch_all_locked(&mut st);
    }

    /// A launch Task left: its uid loses its active reason, once (kill_task_processes and a death can both see the same Task).
    fn task_gone_locked(&self, st: &mut State, task: TaskId) {
        if let Some(uid) = st.open_tasks.remove(&task) {
            let at = st.uids.on_task_removed(uid, self.now_ms());
            self.arm_settle(uid, at);
            self.uid_gone_locked(st, uid);
        }
    }

    /// Clears `uid`'s foreground settle once no attached process of `uid` remains (AppOpsUidStateTrackerImpl.updateUidProcState :242-243, commitUidPendingState :402-405).
    fn uid_gone_locked(&self, st: &mut State, uid: u32) {
        if !st
            .processes
            .iter()
            .any(|r| r.uid == uid && r.attached.is_some())
        {
            st.uids.on_uid_gone(uid);
        }
    }

    fn task_gone(&self, task: TaskId) {
        let mut st = self.state();
        self.task_gone_locked(&mut st, task);
    }

    /// The delayed-message thread's dispatch, with no lock held: each timer's own action.
    fn on_timer(&self, timer: Timer) {
        match timer {
            Timer::KillTask(task) => self.kill_task_processes_on_timeout(task),
            Timer::UidSettle(uid) => self.settle_uid(uid),
            Timer::EmptyProcess(seq) => self.end_headless(|r| r.start_seq == seq),
            Timer::Restart(component) => self.restart_service(component),
            Timer::ProcessStart(start_seq) => self.process_start_timed_out(start_seq),
            Timer::Broadcast { pid, id, index } => self.broadcast_timeout(pid, id, index),
            Timer::ColdStart { key, token } => self.cold_start_timeout(&key, token),
            Timer::TempExpiry(uid) => self.temp_expired(uid),
            Timer::FgsStart(component) => self.foreground_timeout(component),
            Timer::FgsAnr(pid) => self.foreground_anr(pid),
            Timer::HeldLaunch(seq) => self.launch_attached(seq),
        }
    }

    /// The launch ActivityTaskManager makes for an attached process started with one (realStartActivityLocked): its Task's launcher record pushed and sent (activity_task.rs). No lock held; a
    /// process that died or launched meanwhile is a no-op.
    fn launch_attached(&self, start_seq: i64) {
        let packages = self.packages.snapshot();
        let st = self.state();
        let Some(rec) = st
            .processes
            .iter()
            .find(|r| r.start_seq == start_seq && r.launch_activity)
        else {
            return;
        };
        let (Some(thread), Some(task)) =
            (rec.attached.as_ref().map(|a| a.thread.clone()), rec.task)
        else {
            return;
        };
        let (pid, uid, package) = (rec.pid, rec.uid, rec.package.clone());
        drop(st);
        if let Some(p) = packages.by_name(&package)
            && let Some(activity) = packages.launcher(p)
        {
            // the launch is the lifecycle's: the process's Task (made at its start), the record pushed, its Launch + Resume sent (activity_task.rs). The app was bound with its Task's configuration (bind_configuration).
            self.atm
                .launch_launcher(thread, pid, uid, &package, activity, task);
        }
    }

    /// The uid's settle sweep (idleUids :3996-4008): BecameIdle stops the uid's started services whose start mode is not NORMAL (stopInBackgroundLocked, active_services.rs), then
    /// ends the uid's headless processes whose grace has run.
    fn settle_uid(&self, uid: u32) {
        let now = self.now_ms();
        let mut st = self.state();
        let became_idle = st.uids.settle(uid, now) == Settle::BecameIdle;
        if became_idle {
            self.stop_in_background_locked(&mut st, uid);
        }
        drop(st);
        if became_idle {
            self.end_headless(|r| r.uid == uid);
        }
    }

    /// AMS tempAllowlistUidLocked (:15746-15765) for `uid`: the grant makes the uid active until its expiry, which is armed (a later grant's expiry replaces it).
    fn grant_temp_locked(&self, st: &mut State, uid: u32, grant: uid_state::TempGrant) {
        let now = self.now_ms();
        if let Some(at) = st
            .uids
            .on_temp_allowlist(uid, now, grant.duration_ms, grant.fgs)
        {
            atl_diag::answered(AM, "temp allowlist granted (tempAllowlistUidLocked)");
            self.arm(Timer::TempExpiry(uid), at);
        }
    }

    /// A grant's expiry fired: the uid's settle sweep is armed at the deadline `on_temp_expired` returns, when no other reason holds the uid.
    fn temp_expired(&self, uid: u32) {
        let now = self.now_ms();
        let mut st = self.state();
        atl_diag::answered(AM, "temp allowlist expired");
        if let Some(at) = st.uids.on_temp_expired(uid, now) {
            self.arm(Timer::UidSettle(uid), at);
        }
    }

    /// What holds the process (uid_state::Holdings): a visible Task, a started service or BIND_AUTO_CREATE binding (service_records.rs held_by, a waiting start included), a manifest
    /// delivery in flight or waiting for it (the dispatcher's queue). A foreground service holds its process (uid_state `ends`: a process with an FGS is never empty).
    fn holdings_locked(&self, st: &State, r: &ProcessRecord) -> uid_state::Holdings {
        let held = st.services.held_by(r.pid, &r.process_name, r.uid);
        let key = ProcKey {
            name: r.process_name.clone(),
            uid: r.uid,
        };
        uid_state::Holdings {
            task: r
                .attached
                .as_ref()
                .is_some_and(|a| self.atm.hosts_activity(&a.thread.as_binder())),
            fgs: !st.services.foreground_of_pid(r.pid).is_empty(),
            started_service: held.started,
            auto_create_binding: held.bound,
            receiver: st.dispatcher.holds(&key),
            provider: st
                .providers
                .iter()
                .any(|p| p.package == r.package && p.process == r.process_name),
            instrumented: r.instr.is_some(),
            persistent: self.persistent_app(&r.package),
        }
    }

    /// The package's manifest FLAG_PERSISTENT: its processes are never ended by the empty rule.
    fn persistent_app(&self, package: &str) -> bool {
        let packages = self.packages.snapshot();
        packages
            .by_name(package)
            .is_some_and(|p| p.app().flags & PERSISTENT_MASK == PERSISTENT_MASK)
    }

    /// The empty-headless end rule (uid_state::ends) over the processes `only` selects: nothing holds the process, its uid is idle, and its grace has run since its last touch.
    /// Each one ended is marked EmptyHeadless, logged and counted, then killed after the state lock. A window's process is never here: its Task holds it, and its uid is active.
    fn end_headless(&self, only: impl Fn(&ProcessRecord) -> bool) {
        let now = self.now_ms();
        let victims: Vec<(String, i32)> = {
            let mut st = self.state();
            let due: Vec<usize> = (0..st.processes.len())
                .filter(|&i| {
                    let r = &st.processes[i];
                    r.kill_reason.is_none()
                        && only(r)
                        && uid_state::ends(
                            &self.holdings_locked(&st, r),
                            st.uids.is_idle(r.uid),
                            r.touched,
                            self.empty_grace_ms,
                            now,
                        )
                })
                .collect();
            due.into_iter()
                .map(|i| {
                    let r = &mut st.processes[i];
                    r.kill_reason = Some(KillReason::EmptyHeadless);
                    (r.process_name.clone(), r.pid)
                })
                .collect()
        };
        for (name, _) in &victims {
            atl_diag::answered(AM, "headless process ended: empty, uid idle, grace run");
            log(format_args!(
                "process {name} ended: empty headless (grace {} ms)",
                self.empty_grace_ms
            ));
        }
        let pids: Vec<i32> = victims.iter().map(|(_, pid)| *pid).collect();
        self.kill(&pids);
    }

    /// Kills processes by pid, after the state lock was released (AOSP's Process.killProcess is a local syscall; here it is a request to atld).
    fn kill(&self, pids: &[i32]) {
        for &pid in pids {
            if let Err(e) = self.control.kill(pid) {
                log(format_args!("killing pid {pid}: {e}"));
            }
        }
    }

    /// The effects of a state change, once the state lock is released: the lost providers' waiters are told (ContentProviderRecord.java:217-219), then the processes are killed
    /// (AMS handleProcessStartOrKillTimeoutLocked: the provider cleanup comes before killLocked).
    fn deliver(&self, fx: Effects) {
        for w in fx.lost {
            if let Err(e) = w.thread.r#notifyContentProviderPublishStatus(
                Some(&w.holder),
                Some(&w.authority),
                0,
                false,
            ) {
                log(format_args!(
                    "notifyContentProviderPublishStatus {}: {e}",
                    w.authority
                ));
            }
        }
        self.kill(&fx.pids);
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
        // a frozen package starts nothing (ComputerEngine.getPackageStartability :3643; the text is PackageManagerService :4642); the refusal is counted
        if st.frozen.contains_key(p.name()) {
            atl_diag::refused(AM, "start refused: package frozen");
            return Err(std::io::Error::other(format!(
                "Package {} is currently frozen!",
                p.name()
            )));
        }
        // the record is read under the state lock: a caller's snapshot may predate an update published while it waited for the lock
        let packages = self.packages.snapshot();
        let Some(p) = packages.by_name(p.name()) else {
            return Err(std::io::Error::other(format!(
                "Package {} is not installed",
                p.name()
            )));
        };
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
        let task = packages.launcher(p).filter(|_| launch_activity).map(|a| {
            let size = self.tasks.opening_size(&crate::packages::bounds_input(a));
            self.tasks.new_task(p.uid, p.name(), size)
        });
        let now = self.now_ms();
        if let Some(task) = task {
            st.open_tasks.insert(task, p.uid);
            st.uids.on_task_added(p.uid, now);
        }
        let settle = st.uids.on_process_started(p.uid, now);
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
            attach_finished: false,
            touched: now,
            kill_reason: None,
        });
        self.arm_settle(p.uid, settle);
        self.touch_all_locked(st);
        self.arm(
            Timer::ProcessStart(start_seq),
            now + self.hw.scale(PROC_START_TIMEOUT_BASE_MS),
        );
        Ok(())
    }

    /// ContentProviderHelper.getContentProviderImpl for a caller with an app thread, user 0: the provider of `authority` (PMS resolveContentProvider over the installed packages), its
    /// record found or made (mProviderMap); published -> the full holder; else its process starts through start_headless_locked unless its package runs one (:540-593), and the
    /// caller is kept as a waiter: told at publication (:620-648, ActivityThread.acquireProvider waits for it), or at the end of its process without attach (unattached_ended_locked).
    /// None: no such provider, its process failed to start (:570-575), or its process is held by another package (not modelled: AOSP installs the provider there, :540-546).
    /// Not modelled: connections and their ref counts (holder.connection null), a provider able to run in the caller's process (multiprocess / same uid: ActivityThread finds its own first).
    fn content_provider_locked(
        &self,
        st: &mut State,
        caller: &Strong<dyn IApplicationThread>,
        caller_uid: u32,
        authority: &str,
    ) -> binder::Result<Option<ContentProviderHolder>> {
        let packages = self.packages.snapshot();
        let Some((p, info)) = resolve_provider(&packages, authority) else {
            return Ok(None);
        };
        // checkContentProviderPermission's first rule: an unexported provider serves its own uid only (ContentProviderHelper :1186-1192)
        if !info.component.exported
            && permission::app_id(p.uid) != permission::app_id(caller_uid)
            && !permission::is_system_or_root(caller_uid)
        {
            return Err(security(&format!(
                "Permission Denial: opening provider {} from uid {caller_uid} that is not exported from uid {}",
                info.component.item.name.as_deref().unwrap_or(""),
                p.uid
            )));
        }
        let class = info.component.item.name.clone().unwrap_or_default();
        let i = match st
            .providers
            .iter()
            .position(|r| r.package == p.name() && r.class == class)
        {
            Some(i) => i,
            None => {
                st.providers.push(ProviderRecord {
                    package: p.name().to_owned(),
                    class,
                    process: info
                        .component
                        .process_name
                        .clone()
                        .unwrap_or_else(|| p.process_name().to_owned()),
                    info,
                    provider: None,
                    pid: None,
                    waiters: Vec::new(),
                });
                st.providers.len() - 1
            }
        };
        let process = st.providers[i].process.clone();
        // ContentProviderRecord.canRunHere (:122-125): the caller's own process may host it -> newHolder(null, true), the caller instantiates its own (ContentProviderHelper :278-286 running,
        // :481-487 not running). A provider reading its own authority from onCreate (SettingsProvider does) would otherwise wait for its own publication.
        let here = Self::record_for_thread(st, caller)
            .and_then(|pid| st.processes.iter().find(|r| r.pid == pid))
            .is_some_and(|r| {
                (st.providers[i].info.multiprocess || r.process_name == process) && r.uid == p.uid
            });
        if here {
            let mut h = st.providers[i].holder();
            h.provider = None;
            h.local = true;
            return Ok(Some(h));
        }
        if st.providers[i].provider.is_some() {
            return Ok(Some(st.providers[i].holder()));
        }
        // A provider is published only by its own package's process (r#publishContentProviders matches the publisher's package), so a wait needs a record of
        // (package, process). Without one, start_headless_locked starts it; a record of (process, uid) owned by another package answers Ok(false) and leaves no way to publish here: refused.
        let own = st
            .processes
            .iter()
            .any(|r| r.package == p.name() && r.process_name == process);
        if own {
            atl_diag::answered(AM, "getContentProvider: waits for its running process");
        } else {
            match self.start_headless_locked(st, p.name(), &process, p.uid) {
                Ok(true) => {
                    atl_diag::answered(AM, "provider cold start (getContentProviderImpl)");
                    log(format_args!(
                        "provider {authority}: started process {process} of {}",
                        p.name()
                    ));
                }
                Ok(false) => {
                    atl_diag::answered(
                        AM,
                        "getContentProvider: process held by another package (not modelled)",
                    );
                    st.providers.remove(i);
                    return Ok(None);
                }
                Err(e) => {
                    atl_diag::answered(AM, "getContentProvider: process start failed");
                    log(format_args!(
                        "Unable to launch app {}/{} for provider {authority}: process is bad ({e})",
                        p.name(),
                        p.uid
                    ));
                    st.providers.remove(i);
                    return Ok(None);
                }
            }
        }
        st.providers[i].waiters.push(caller.clone());
        Ok(Some(st.providers[i].holder()))
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

    /// forceStopPackageLocked (as finishInstrumentationLocked and startInstrumentation call it): the package's processes leave the table; the returned Effects (their pids, the waiters of their providers) are delivered once the lock is
    /// released. A removed process's instrumentation, if any, ends as a crash would end it (handleAppDiedLocked).
    fn force_stop_package_locked(&self, st: &mut State, package: &str) -> Effects {
        let (stopped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut st.processes)
            .into_iter()
            .partition(|r| r.package == package);
        st.processes = kept;
        // cleanupDisabledPackageReceiversLocked: the package's waiting manifest deliveries are over. It runs before the kills below (AOSP runs it after them) because the drives here
        // start waiting receivers synchronously: a force-stopped package must not start a process while it is being stopped.
        self.drop_package_locked(st, package);
        let mut fx = Effects::default();
        for mut rec in stopped {
            fx.pids.push(rec.pid);
            // forceStopPackageInternalLocked: killPackageProcessesLSP(allowRestart false) (AMS :4360-4361) -> cleanUpApplicationRecordLocked -> killServicesLocked(app, false):
            // every hosted service is brought down, STICKY ones included (ActiveServices :7343-7345); none is restarted. No LastAppEnded: the user did not close the app
            if rec.attached.is_some() {
                let (lost, _) = self.process_out_locked(st, &rec, false);
                fx.lost.extend(lost);
            } else {
                // no binder death comes for an unattached process: its end is the one shared with the start timeout (unattached_ended_locked), whose providers are the unpublished ones only
                fx.lost.extend(self.unattached_ended_locked(st, &rec));
                self.services_died(st, rec.pid, false);
                if let Some(task) = rec.task {
                    self.task_gone_locked(st, task);
                }
                self.uid_gone_locked(st, rec.uid);
                self.touch_all_locked(st);
            }
            if let Some(instr) = rec.instr.take() {
                // the process is being killed with its package: the same CANCELED a crash reports (no further force stop: the package's processes are all gone)
                self.report_finished(instr, RESULT_CANCELED, Some(crash_info()));
            }
        }
        fx
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

    /// finishInstrumentationLocked: reports, then (not for no-restart) force-stops the package; the effects to deliver.
    fn finish_instrumentation_locked(
        &self,
        st: &mut State,
        pid: i32,
        code: i32,
        results: Option<Bundle>,
    ) -> Effects {
        let Some(rec) = st.processes.iter_mut().find(|r| r.pid == pid) else {
            return Effects::default();
        };
        let Some(instr) = rec.instr.take() else {
            log(format_args!(
                "finishInstrumentation called on non-instrumented: pid {pid}"
            ));
            return Effects::default();
        };
        let (no_restart, package) = (instr.no_restart, rec.package.clone());
        // the instrumentation no longer holds its process: its grace runs from now
        self.touch_all_locked(st);
        self.report_finished(instr, code, results);
        if no_restart {
            return Effects::default();
        }
        self.force_stop_package_locked(st, &package)
    }

    /// The death of an attached process, from its record's removal to the end of its providers, in app_died's order: its launch Task, its activities, its broadcasts, its services (`allow_restart`: STICKY restarts, false for a force stop), the grace of the processes left, and the providers it published (their waiters returned, told after the lock). A force stop runs it too (forceStopPackageInternalLocked = a death with no restart). Not for an unattached process: unattached_ended_locked.
    fn process_out_locked(
        &self,
        st: &mut State,
        rec: &ProcessRecord,
        allow_restart: bool,
    ) -> (Vec<LostWaiter>, Option<bool>) {
        // WindowProcessController.destroy: its configuration listener goes
        self.tasks.set_process_task(rec.pid, None);
        // Some(finished): this death left no activity anywhere and the process had a Task (a launched app; SettingsProvider, WebView and other no-Task processes never count)
        let last_app = rec
            .attached
            .as_ref()
            .and_then(|a| self.atm.process_died(&a.thread.as_binder()))
            .filter(|_| rec.task.is_some());
        if let Some(task) = rec.task {
            self.task_gone_locked(st, task);
        }
        self.broadcasts_died_locked(st, rec.pid);
        self.services_died(st, rec.pid, allow_restart);
        self.uid_gone_locked(st, rec.uid);
        self.touch_all_locked(st);
        let lost = Self::remove_providers_of_process(st, &rec.package, &rec.process_name, |_| true);
        (lost, last_app)
    }

    /// AppDeathRecipient.binderDied -> appDiedLocked -> handleAppDiedLocked (:3347-3367): the record leaves the table, and an instrumentation it ran ends CANCELED with "Process crashed.".
    fn app_died(&self, pid: i32) {
        let (fx, last_app) = {
            let mut st = self.state();
            let Some(i) = st
                .processes
                .iter()
                .position(|r| r.pid == pid && r.attached.is_some())
            else {
                return;
            };
            let mut rec = st.processes.remove(i);
            let allow_restart = !(rec.kill_reason == Some(KillReason::WindowClosed)
                && self.window_close_stops_services);
            let (lost, finished) = self.process_out_locked(&mut st, &rec, allow_restart);
            log(format_args!(
                "process {} (pid {pid}) died",
                rec.process_name
            ));
            let mut fx = match rec.instr.take() {
                Some(instr) => {
                    log(format_args!(
                        "Crash of app {} running instrumentation {}/{}",
                        rec.process_name, instr.class.package, instr.class.class
                    ));
                    let no_restart = instr.no_restart;
                    self.report_finished(instr, RESULT_CANCELED, Some(crash_info()));
                    if no_restart {
                        Effects::default()
                    } else {
                        self.force_stop_package_locked(&mut st, &rec.package)
                    }
                }
                None => Effects::default(),
            };
            fx.lost.extend(lost);
            (fx, finished)
        };
        self.deliver(fx);
        if let Some(finished) = last_app {
            // read by the harness's run-apk verdict (`finished` / `live`); the host decides whether the run ends (atld --exit-with-last-app)
            log(format_args!(
                "last app process ended: pid {pid} {} at={}",
                if finished { "finished" } else { "live" },
                atl_diag::wall_stamp()
            ));
            if let Err(e) = self.control.last_app_ended(pid, finished) {
                log(format_args!("last app ended: reporting pid {pid}: {e}"));
            }
        }
    }

    /// ContentProviderHelper.removeDyingProviderLocked (:1794-1806) for the providers of (package, process) that `dying` takes: their records go, and the callers still waiting
    /// are returned to be told publish status false after the lock (ContentProviderRecord.onProviderPublishStatusLocked :190-226).
    /// Not modelled: killing the stable clients of a published provider (AOSP kills them, :1832-1871).
    fn remove_providers_of_process(
        st: &mut State,
        package: &str,
        process: &str,
        dying: impl Fn(&ProviderRecord) -> bool,
    ) -> Vec<LostWaiter> {
        let mut lost = Vec::new();
        st.providers.retain_mut(|r| {
            if r.package != package || r.process != process || !dying(r) {
                return true;
            }
            let holder = r.holder();
            let authority = r.info.authority.clone().unwrap_or_default();
            for thread in std::mem::take(&mut r.waiters) {
                lost.push(LostWaiter {
                    thread,
                    holder: holder.clone(),
                    authority: authority.clone(),
                });
            }
            false
        });
        lost
    }

    /// checkPermission(permission, pid, uid) against the uid's granted permission states.
    fn holds(&self, uid: u32, perm: &str) -> bool {
        self.permissions.check(uid, Some(perm))
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

    /// broadcastIntentWithFeature's callerApp (BroadcastController.java:722, getRecordForAppLOSP) as broadcastIntentLocked reads it: its package (callerPackage, :741) and whether the process is
    /// persistent (ProcessList.java:3447-3454: FLAG_SYSTEM | FLAG_PERSISTENT, user 0, the application's own process). (None, false) = no attached process for `caller`.
    fn caller_app(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
    ) -> (Option<String>, bool) {
        let st = self.state();
        let Some(rec) = caller
            .and_then(|t| Self::record_for_thread(&st, t))
            .and_then(|pid| st.processes.iter().find(|r| r.pid == pid))
        else {
            return (None, false);
        };
        let packages = self.packages.snapshot();
        let persistent = packages.by_name(&rec.package).is_some_and(|p| {
            p.app().flags & PERSISTENT_MASK == PERSISTENT_MASK
                && permission::user_id(rec.uid) == 0
                && p.process_name() == rec.process_name
        });
        (Some(rec.package.clone()), persistent)
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

    /// ActivityManagerService.getMemoryInfo:6833 -> ProcessList.getMemoryInfo: the host's MemTotal / MemAvailable through AOSP's thresholds (memory_levels.rs), the display the OOM levels scale
    /// with being the services' one display. An unreadable /proc/meminfo is refused and counted (AOSP's getFreeMemory throws there too).
    fn r#getMemoryInfo(
        &self,
        out_info: &mut Option<ActivityManager_MemoryInfo>,
    ) -> binder::Result<()> {
        use atl_platform_host::MemorySource;
        let host = atl_platform_host::PlatformMemory.memory().map_err(|e| {
            atl_diag::refused(AM, &format!("getMemoryInfo: {e}"));
            Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(e.to_string()))
        })?;
        let display = self.inner.tasks.display();
        let m =
            memory_levels::memory_info(host.total, host.available, display.width, display.height);
        atl_diag::answered(AM, "getMemoryInfo");
        *out_info = Some(ActivityManager_MemoryInfo {
            advertised_mem: m.advertised_mem,
            avail_mem: m.avail_mem,
            total_mem: m.total_mem,
            threshold: m.threshold,
            low_memory: m.low_memory,
            hidden_app_threshold: m.hidden_app_threshold,
            secondary_server_threshold: m.secondary_server_threshold,
            visible_app_threshold: m.visible_app_threshold,
            foreground_app_threshold: m.foreground_app_threshold,
        });
        Ok(())
    }

    /// ActivityManagerService.getProcessesInErrorState:10004: the processes that are crashing or not responding, null ("lazy create the list") when none is. ATL keeps no crash / ANR state:
    /// a crashing app process exits, and no ANR is detected, so the list is always null.
    fn r#getProcessesInErrorState(
        &self,
    ) -> binder::Result<Option<Vec<Option<ActivityManager_ProcessErrorStateInfo>>>> {
        Ok(None)
    }

    /// ActivityManagerService.frozenBinderTransactionDetected:19262: an app's report that a binder call failed (ActivityThread's Binder.transactionCallback, for every non-OK transact).
    /// AOSP logs it and acts only with the freezer on (CachedAppOptimizer.binderError); there is no freezer here, so it is only counted (power-of-two log points, not AOSP's line per call:
    /// apps repeat it for every unanswered call).
    fn r#frozenBinderTransactionDetected(
        &self,
        _debug_pid: i32,
        _code: i32,
        _flags: i32,
        _err: i32,
    ) -> binder::Result<()> {
        atl_diag::answered("IActivityManager", "frozenBinderTransactionDetected");
        Ok(())
    }

    /// ActivityManagerService.getRunningAppProcesses: the attached processes the caller may see (running_processes), each with its state's importance and processState (ProcessList.fillInProcMemInfoLOSP :4183-4205). Not enforced: enforceNotIsolatedCaller (no isolated uids).
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
                state: uid_state::process_state(&self.inner.holdings_locked(&st, r)),
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
                        importance: running_processes::importance(p.state),
                        process_state: p.state as i32,
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
        let packages = inner.packages.snapshot();
        let Some(p) = packages.by_name(&st.processes[i].package) else {
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
            Some(instr) => packages
                .by_name(&instr.target_package)
                .map(|t| package::application_info(t, &packages, package::STOCK_PM_FLAGS))
                .ok_or_else(|| illegal_state("instrumentation target vanished"))?,
            None => package::application_info(p, &packages, package::STOCK_PM_FLAGS),
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
        // preBindApplication:6792-6809: a process started for its launch binds with its Task's configuration, any other with the global one
        let launch_task = st.processes[i]
            .launch_activity
            .then_some(st.processes[i].task)
            .flatten();
        let bind_config = inner.atm.bind_configuration(pid, launch_task);
        // what attachApplicationLocked passes for a plain app start (no profiler, backup or sdk sandbox): nulls, the process configuration (above), the default compat info,
        // the process's providers (generateApplicationProvidersLocked; ActivityThread installs them before Application.onCreate), no disabled compat changes; coreSettings never null (empty: ATL writes no keys, gl-honesty D5) (attachApplicationLocked passes
        // mCoreSettingsObserver.getCoreSettingsLocked(), and ActivityThread.updateDebugViewAttributeState reads it unconditionally); its keys are AOSP's own; ATL writes none (gl-honesty D5).
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
            list: package::process_providers(p, &packages, &process_name, package::STOCK_PM_FLAGS)
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
            Some(&bind_config),
            Some(&CompatibilityInfo::default()),
            // :4773 getCommonServicesLocked
            Some(inner.common_services.get_or_init(common_services)),
            Some(&Bundle::default()),
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
        let (package, launch, seq) = (rec.package.clone(), rec.launch_activity, rec.start_seq);
        let mut st = st;
        // D2 clause 2: a package's first process start of the run gets its boot broadcasts ahead of its first activity: the launch waits for BOOT's final result (Timer::HeldLaunch)
        let boot = inner.first_start_boot_locked(&mut st, &package, launch.then_some(seq));
        drop(st);
        if launch && !boot {
            inner.launch_attached(seq);
        }
        let mut st = inner.state();
        if let Some(r) = st.processes.iter_mut().find(|r| r.pid == pid) {
            r.attach_finished = true;
        }
        // :4933 mBroadcastQueue.onApplicationAttachedLocked, after the activity attach: the process's queue resumes (broadcasts_am.rs)
        inner.process_attached_locked(&mut st, pid);
        // the deliveries that were waiting for it left the pending hold: the headless grace restarts from here
        inner.touch_all_locked(&mut st);
        // :4921 mServices.attachApplicationLocked: the services that waited for this process start (their order against the broadcasts is no app's concern: separate senders, as in AOSP)
        inner.services_attached(&mut st, pid);
        drop(st);
        Ok(())
    }

    /// bindService (the greylisted form) -> bindServiceInstance without an instance name.
    fn r#bindService(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        token: Option<&SpIBinder>,
        service: Option<&Intent>,
        resolved_type: Option<&str>,
        connection: Option<&Strong<dyn IServiceConnection>>,
        flags: i64,
        calling_package: Option<&str>,
        user_id: i32,
    ) -> binder::Result<i32> {
        self.r#bindServiceInstance(
            caller,
            token,
            service,
            resolved_type,
            connection,
            flags,
            None,
            calling_package,
            user_id,
        )
    }

    /// ActivityManagerService.bindServiceInstance -> ActiveServices.bindServiceLocked: the caller must be an attached app (SecurityException "Unable to find app for caller"), then the lookup,
    /// checkComponentPermission and the binding rules (active_services.rs, service_records.rs). ContextImpl turns -1 into SecurityException and 0 into a false bindService.
    fn r#bindServiceInstance(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        _token: Option<&SpIBinder>,
        service: Option<&Intent>,
        _resolved_type: Option<&str>,
        connection: Option<&Strong<dyn IServiceConnection>>,
        flags: i64,
        instance_name: Option<&str>,
        _calling_package: Option<&str>,
        _user_id: i32,
    ) -> binder::Result<i32> {
        let inner = &self.inner;
        let uid = ThreadState::get_calling_uid();
        let mut st = inner.state();
        let client = caller.and_then(|c| Inner::record_for_thread(&st, c));
        let Some(client_pid) = client else {
            return Err(security(&format!(
                "Unable to find app for caller (pid={}) when binding service",
                ThreadState::get_calling_pid()
            )));
        };
        let (Some(intent), Some(conn)) = (service, connection) else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("bindService: null service or connection"),
            ));
        };
        if instance_name.is_some() {
            atl_diag::answered(AM, "bindServiceInstance: instanceName ignored");
        }
        Ok(inner
            .bind_service_locked(&mut st, client_pid, uid, intent, conn.clone(), flags)
            .code())
    }

    /// ActiveServices.unbindServiceLocked: false when the connection has no bindings.
    fn r#unbindService(
        &self,
        connection: Option<&Strong<dyn IServiceConnection>>,
    ) -> binder::Result<bool> {
        let Some(conn) = connection else {
            return Ok(false);
        };
        let mut st = self.inner.state();
        Ok(self.inner.unbind_service_locked(&mut st, conn.clone()))
    }

    /// ActiveServices.publishServiceLocked: the service's onBind result reaches the binding's connections. A null token is IllegalArgumentException ("Invalid service token").
    /// A token of no current record is ignored: AOSP's token is the ServiceRecord itself, so a publish that arrives after the service was brought down (its last client unbound while onBind ran)
    /// finds no binding and does nothing. A forged token, which AOSP refuses, is ignored too: the services keep no record of tokens they minted once the record is gone.
    fn r#publishService(
        &self,
        token: Option<&SpIBinder>,
        intent: Option<&Intent>,
        service: Option<&SpIBinder>,
    ) -> binder::Result<()> {
        let (Some(token), Some(intent)) = (token, intent) else {
            return Err(illegal_argument("Invalid service token"));
        };
        let inner = &self.inner;
        let mut st = inner.state();
        let mut calls = Vec::new();
        if !st.services.publish(
            token,
            &intent_key(intent, None),
            service.cloned(),
            &mut calls,
        ) {
            atl_diag::answered(AM, "publishService: service already brought down (ignored)");
        }
        inner.service_calls.post(calls);
        Ok(())
    }

    /// ActiveServices.unbindFinishedLocked: onUnbind returned true.
    fn r#unbindFinished(
        &self,
        token: Option<&SpIBinder>,
        service: Option<&Intent>,
    ) -> binder::Result<()> {
        let (Some(token), Some(intent)) = (token, service) else {
            return Err(illegal_argument("Invalid service token"));
        };
        let inner = &self.inner;
        let mut st = inner.state();
        let mut calls = Vec::new();
        st.services
            .unbind_finished(token, &intent_key(intent, None), &mut calls);
        inner.service_calls.post(calls);
        Ok(())
    }

    /// ActivityManagerService.startService -> ActiveServices.startServiceLocked (active_services.rs). A caller that names an app must be an attached one
    /// (SecurityException). The answer is the ComponentName ContextImpl reads: the service's own, the `!` / `?` / `!!` sentinels, or null for no such service.
    /// `require_foreground` (startForegroundService): the wait for startForeground is the service's (service_records.rs, armed at the delivery).
    fn r#startService(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        service: Option<&Intent>,
        _resolved_type: Option<&str>,
        require_foreground: bool,
        _calling_package: Option<&str>,
        _calling_feature_id: Option<&str>,
        _user_id: i32,
    ) -> binder::Result<Option<ComponentName>> {
        let uid = ThreadState::get_calling_uid();
        let Some(service) = service else {
            return Ok(None);
        };
        let inner = &self.inner;
        let mut st = inner.state();
        let caller_fg = match caller {
            None => true,
            Some(c) => {
                if Inner::record_for_thread(&st, c).is_none() {
                    return Err(security(&format!(
                        "Unable to find app for caller (pid={}) when starting service",
                        ThreadState::get_calling_pid()
                    )));
                }
                inner.atm.hosts_activity(&c.as_binder())
            }
        };
        Ok(inner.start_service_locked(&mut st, uid, caller_fg, service, require_foreground))
    }

    /// ActivityManagerService.stopService -> ActiveServices.stopServiceLocked: 1 the service's start was dropped, 0 no such service, -1 a refused permission
    /// (ContextImpl throws SecurityException on -1 and returns `res != 0`).
    fn r#stopService(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        service: Option<&Intent>,
        _resolved_type: Option<&str>,
        _user_id: i32,
    ) -> binder::Result<i32> {
        let uid = ThreadState::get_calling_uid();
        let inner = &self.inner;
        let mut st = inner.state();
        if let Some(c) = caller
            && Inner::record_for_thread(&st, c).is_none()
        {
            return Err(security(&format!(
                "Unable to find app for caller (pid={}) when stopping service",
                ThreadState::get_calling_pid()
            )));
        }
        let Some(service) = service else {
            return Ok(0);
        };
        Ok(inner.stop_service_locked(&mut st, uid, service))
    }

    /// ActivityManagerService.stopServiceToken -> ActiveServices.stopServiceTokenLocked (stopSelf): true when the service stopped.
    fn r#stopServiceToken(
        &self,
        class_name: Option<&ComponentName>,
        token: Option<&SpIBinder>,
        start_id: i32,
    ) -> binder::Result<bool> {
        let Some(token) = token else {
            return Ok(false);
        };
        let mut st = self.inner.state();
        Ok(self
            .inner
            .stop_self_locked(&mut st, class_name, token, start_id))
    }

    /// ActivityManagerService.setServiceForeground -> ActiveServices.setServiceForegroundLocked (active_services.rs): the checks and the state under the lock; the notification
    /// is posted (or an old one cancelled) after the lock is released. An unknown service is a silent no-op, as AOSP's findServiceLocked.
    fn r#setServiceForeground(
        &self,
        class_name: Option<&ComponentName>,
        token: Option<&SpIBinder>,
        id: i32,
        notification: Option<&Notification>,
        flags: i32,
        foreground_service_type: i32,
    ) -> binder::Result<()> {
        let inner = &self.inner;
        let posting = {
            let mut st = inner.state();
            inner.set_service_foreground_locked(
                &mut st,
                class_name,
                token,
                id,
                notification,
                flags,
                foreground_service_type,
            )?
        };
        if let Some(p) = posting {
            inner.notices.post(p.uid, &p.package, p.id, &p.notification);
        }
        Ok(())
    }

    /// ActivityManagerService.getForegroundServiceType -> ActiveServices.getForegroundServiceTypeLocked: the type of the named service while it is foreground, else NONE.
    fn r#getForegroundServiceType(
        &self,
        class_name: Option<&ComponentName>,
        token: Option<&SpIBinder>,
    ) -> binder::Result<i32> {
        let st = self.inner.state();
        Ok(self
            .inner
            .foreground_service_type_locked(&st, class_name, token))
    }

    /// serviceDoneExecuting (oneway): the app finished a scheduled service call (ActiveServices.serviceDoneExecutingLocked, active_services.rs).
    fn r#serviceDoneExecuting(
        &self,
        token: Option<&SpIBinder>,
        kind: i32,
        start_id: i32,
        res: i32,
        _intent: Option<&Intent>,
    ) -> binder::Result<()> {
        let mut st = self.inner.state();
        self.inner
            .service_done_executing_locked(&mut st, token, kind, start_id, res)
    }

    /// ActivityManagerService.getContentProvider:7074 -> ContentProviderHelper.getContentProvider:134: a null caller is a SecurityException (:137-141), as is a callingPackage not
    /// of the caller's uid (:145-149); then the provider of `name`, or null when none (getContentProviderImpl, [`Inner::content_provider_locked`]).
    fn r#getContentProvider(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        calling_package: Option<&str>,
        name: Option<&str>,
        _user: i32,
        _stable: bool,
    ) -> binder::Result<Option<ContentProviderHolder>> {
        let Some(caller) = caller else {
            return Err(security(&format!(
                "null IApplicationThread when getting content provider {name:?}"
            )));
        };
        let uid = ThreadState::get_calling_uid();
        if let Some(pkg) = calling_package {
            let packages = self.inner.packages.snapshot();
            let owns = packages
                .by_name(pkg)
                .is_some_and(|p| permission::app_id(p.uid) == permission::app_id(uid));
            if !owns && !permission::is_system_or_root(uid) {
                return Err(security(&format!(
                    "Given calling package {pkg} does not match caller's uid {uid}"
                )));
            }
        }
        let Some(name) = name else {
            return Ok(None);
        };
        let mut st = self.inner.state();
        self.inner
            .content_provider_locked(&mut st, caller, uid, name)
    }

    /// ContentProviderHelper.publishContentProviders:731: an unknown caller is a SecurityException (:742); each holder whose provider a caller asked for (mProviderMap's record of the
    /// caller's package and class, :750-760) gets its binder and the publishing pid, and its waiters notifyContentProviderPublishStatus (:780-790, sent after the lock). A holder nobody asked for
    /// yet is recorded too (the process publishes all its providers at bind; a later getContentProvider must find it published, not wait).
    fn r#publishContentProviders(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
        providers: Option<&[Option<ContentProviderHolder>]>,
    ) -> binder::Result<()> {
        let mut st = self.inner.state();
        let Some(pid) = caller.and_then(|t| Inner::record_for_thread(&st, t)) else {
            return Err(security(
                "Unable to find app for caller when publishing content providers",
            ));
        };
        let Some(proc) = st.processes.iter().find(|r| r.pid == pid) else {
            return Err(security(
                "Unable to find app for caller when publishing content providers",
            ));
        };
        let (package, process) = (proc.package.clone(), proc.process_name.clone());
        let mut sends = Vec::new();
        for h in providers.into_iter().flatten().flatten() {
            let Some(binder) = h.provider.clone() else {
                continue;
            };
            let class = h.info.component.item.name.clone().unwrap_or_default();
            let i = match st
                .providers
                .iter()
                .position(|r| r.package == package && r.class == class)
            {
                Some(i) => i,
                None => {
                    st.providers.push(ProviderRecord {
                        package: package.clone(),
                        class,
                        process: process.clone(),
                        info: h.info.clone(),
                        provider: None,
                        pid: None,
                        waiters: Vec::new(),
                    });
                    st.providers.len() - 1
                }
            };
            let r = &mut st.providers[i];
            r.provider = Some(binder);
            r.pid = Some(pid);
            let holder = r.holder();
            let auth = r.info.authority.clone().unwrap_or_default();
            for w in std::mem::take(&mut r.waiters) {
                sends.push((w, holder.clone(), auth.clone()));
            }
        }
        drop(st);
        atl_diag::answered(AM, "publishContentProviders");
        for (w, holder, auth) in sends {
            if let Err(e) =
                w.r#notifyContentProviderPublishStatus(Some(&holder), Some(&auth), 0, true)
            {
                log(format_args!(
                    "notifyContentProviderPublishStatus {auth}: {e}"
                ));
            }
        }
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
        let packages = inner.packages.snapshot();
        let Some((_, decl)) = packages.instrumentation(&class_name.package, &class_name.class)
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
        let Some(target) = packages.by_name(target_package) else {
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
                packages
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
        let fx = inner.force_stop_package_locked(&mut st, target.name());
        let started =
            inner.start_process_locked(&mut st, target, &default_process, false, Some(instr));
        drop(st);
        inner.deliver(fx);
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
        let fx = {
            let mut st = inner.state();
            let Some(pid) = target.and_then(|t| Inner::record_for_thread(&st, t)) else {
                log(format_args!("finishInstrumentation: no app for the thread"));
                return Ok(());
            };
            inner.finish_instrumentation_locked(&mut st, pid, result_code, results.cloned())
        };
        inner.deliver(fx);
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
        let packages = self.inner.packages.snapshot();
        let owned = permission::is_system_or_root(uid) || packages.is_same_app(package, uid);
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
        let request = intents
            .and_then(|list| list.last())
            .and_then(|last| last.clone());
        let sender = self.inner.pending.get_or_create(key, uid, flags, request)?;
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

    /// ActivityManagerService.broadcastIntentWithFeature:14402 -> BroadcastController.broadcastIntentWithFeature:710-754: the Intent is verified as verifyBroadcastLocked does (:1878-1917: a null
    /// Intent is a NullPointerException (`intent.getFlags()`), a file descriptor in the extras and FLAG_RECEIVER_BOOT_UPGRADE are IllegalArgumentExceptions, FLAG_RECEIVER_FROM_SHELL is dropped
    /// unless the caller is root or shell), then broadcastIntentLocked: the sender check (:1031-1093, broadcast::sender_check: a protected action from a non-system caller is a SecurityException),
    /// a sticky is checked and stored (:1415-1495), an explicit component reaches its manifest receiver (:1516-1518, unless FLAG_RECEIVER_REGISTERED_ONLY), an intent without one the matching registered
    /// receivers (:1520-1545); each passes BroadcastSkipPolicy (broadcast.rs) and is queued for its process (BroadcastQueueImpl, broadcast_queue.rs). Result BROADCAST_SUCCESS.
    /// Not modelled, each counted when used: ordered delivery and `resultTo` (P2: sent unordered, no result), implicit manifest receivers (P2). Not modelled: the isolated-caller check (:715), the options
    /// Bundle, excluded permissions / packages, app ops.
    fn r#broadcastIntentWithFeature(
        &self,
        caller: Option<&Strong<dyn IApplicationThread>>,
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
        // null stays None: a null requiredPermissions is not an empty list (isSignaturePerm, BroadcastSkipPolicy :680-684)
        let required: Option<Vec<String>> =
            required_permissions.map(|list| list.iter().flatten().cloned().collect());
        self.broadcast_from_caller(
            (
                ThreadState::get_calling_pid(),
                ThreadState::get_calling_uid(),
            ),
            self.inner.caller_app(caller),
            intent,
            resolved_type,
            serialized || result_to.is_some(),
            required.as_deref(),
            (
                result_code,
                result_data.map(str::to_owned),
                result_extras.cloned(),
            ),
            sticky,
            user_id,
            result_to.map(|handle| ReceiverHandle::Binder(handle.clone())),
        )
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
                .unregister(&ReceiverHandle::Binder(r.clone()));
            if !removed {
                atl_diag::answered(AM, "unregisterReceiver: not registered");
            }
        }
        Ok(())
    }

    /// ActivityManagerService.finishReceiver: a receiver reports done with its result (an ordered broadcast passes it on, an abort skips the rest unless NO_ABORT); the
    /// delivery ends and so does the process's hold on it ([`Inner::finish_receiver`]).
    fn r#finishReceiver(
        &self,
        who: Option<&SpIBinder>,
        result_code: i32,
        result_data: Option<&str>,
        map: Option<&Bundle>,
        abort_broadcast: bool,
        _flags: i32,
    ) -> binder::Result<()> {
        let result = Outcome {
            code: result_code,
            data: result_data.map(str::to_owned),
            extras: map.cloned(),
        };
        self.inner.finish_receiver(who, result, abort_broadcast);
        Ok(())
    }

    /// ActivityManagerService.registerReceiverWithFeature -> BroadcastController.registerReceiverWithFeature:249 -> registerReceiverWithFeatureTraced:296: the caller's process is looked up by its thread
    /// (unknown: logged, null, :312-315); a caller package the process does not run is a SecurityException (:316-320; core uids, below 10000, are exempt: UserHandle.isCore). Then the export flags
    /// (:428-472, broadcast::register_export), the matching stickies (:378-395, :474-509), the first of which is returned; a null receiver stops there (the sticky read). Else the registration is kept
    /// (:530-579) and every matching sticky is queued to it as an initial sticky (:582-604); the first one the receiver may see (exported, or the sticky's sender is its uid / system) is returned.
    /// The filter was decoded, so a filter with parts the codec does not model was refused before it got here. Not modelled: instant apps, MAX_RECEIVERS_ALLOWED_PER_APP, priorities.
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
        let packages = inner.packages.snapshot();
        let target_sdk = packages
            .by_name(&package)
            .map_or(0, |p| p.app().target_sdk_version);
        let priority = filter.priority;
        let filter = filter_of(filter);
        let only_protected = !filter.actions.is_empty()
            && filter
                .actions
                .iter()
                .all(|a| packages.is_protected_broadcast(Some(a)));
        let exported = broadcast::register_export(
            flags,
            receiver.is_some(),
            target_sdk,
            only_protected,
            &package,
        )
        .map_err(|e| match e {
            ExportRefusal::IllegalArgument(m) => illegal_argument(&m),
            ExportRefusal::Security(m) => {
                log(format_args!("{m}"));
                security(&m)
            }
        })?;
        let user = incoming_user(user_id);
        let stickies = st.broadcasts.stickies(&filter, user);
        let Some(receiver) = receiver else {
            return Ok(stickies.into_iter().next().map(|s| s.intent));
        };
        let bf = BroadcastFilter {
            filter,
            priority,
            package,
            required_permission: required_permission.map(str::to_owned),
            exported,
        };
        let handle = ReceiverHandle::Binder(receiver.clone());
        if !st
            .broadcasts
            .register(handle.clone(), pid, uid, user, bf)
            .map_err(|m| illegal_argument(&m))?
        {
            atl_diag::answered(AM, "registerReceiver: filter already registered");
        }
        let mut first = None;
        for s in stickies {
            if first.is_none()
                && (exported
                    || s.calling_uid == uid
                    || permission::is_system_or_root(s.calling_uid))
            {
                first = Some(s.intent.clone());
            }
            inner.sticky_replay_locked(&mut st, pid, handle.clone(), priority, s.intent);
        }
        drop(st);
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
    /// the app processes' UI agents: a move to the back minimizes, and a TaskDescription titles, the Task's window through the agent of the process the Task was launched in
    agents: Arc<UiAgents>,
    /// false: onBackPressed sends no requestFinish (`--no-back-finish`, the dialog gate's back sub-gate's deliberate break); a Back then never finishes an activity.
    back_finish: bool,
    /// what an unhandled Back on a launcher root does
    root_back: RootBack,
    /// which Tasks an orientation request reshapes
    orientation_reshape: OrientationReshape,
}

impl ActivityClientController {
    /// The UI agent of the process `task`'s window is in (Inner::process_of_task). Not the binder caller's: onBackPressed is oneway, and a oneway call carries no calling pid.
    fn task_agent(&self, task: TaskId) -> Option<Strong<dyn IUiAgent>> {
        let process = self.inner.upgrade()?.process_of_task(task)?;
        self.agents
            .agent(process)
            .and_then(|b| b.into_interface::<dyn IUiAgent>().ok())
    }

    /// The reshape check (desktop_bounds::reshaped_size) of `task` for `input`, an orientation request (`why`: what made it): a due reshape goes to the Task's window as RESIZE. Logged as
    /// `orientation reshape task=<id> <w>x<h> -> <w>x<h> (<why>)`. No Task size or no agent (headless): counted, nothing else.
    fn reshape(&self, inner: &Inner, task: TaskId, input: &ActivityInput, why: &str) {
        let Some(size) = inner.tasks.size(task) else {
            atl_diag::answered(ACC, "orientation reshape: no Task size");
            return;
        };
        let display = inner.tasks.display();
        let area = desktop_bounds::Rect {
            left: 0,
            top: 0,
            right: display.width,
            bottom: display.height,
        };
        let current = (size.width, size.height);
        let Some((width, height)) = reshaped_size(area, current, input, self.orientation_reshape)
        else {
            return;
        };
        // a declared display: the Task is the display, whatever the request (tasks.rs full_display)
        let (width, height) = inner
            .tasks
            .full_display()
            .map_or((width, height), |s| (s.width, s.height));
        if (width, height) == current {
            return;
        }
        log(format_args!(
            "orientation reshape task={} {}x{} -> {width}x{height} ({why})",
            task.raw(),
            size.width,
            size.height
        ));
        let Some(agent) = self.task_agent(task) else {
            atl_diag::answered(ACC, "orientation reshape: no UI agent");
            return;
        };
        atl_diag::answered(ACC, "orientation reshape (window resize asked)");
        if let Err(e) = agent.resize(width, height) {
            log(format_args!("orientation reshape: resize: {e:?}"));
        }
    }

    /// moveActivityTaskToBack of `task`, of the app of `uid`: its window's UI agent minimizes the window (the desktop's "back": the Task stays, behind the others). Logged as `onBackPressed: moveTaskToBack uid=<uid>`.
    /// No agent (headless, or not registered yet): counted, nothing else; the activity stays.
    fn task_to_back(&self, uid: u32, task: TaskId) {
        log(format_args!("onBackPressed: moveTaskToBack uid={uid}"));
        let Some(agent) = self.task_agent(task) else {
            atl_diag::answered(ACC, "onBackPressed: moveTaskToBack, no UI agent");
            return;
        };
        atl_diag::answered(ACC, "onBackPressed: moveTaskToBack");
        if let Err(e) = agent.minimize() {
            log(format_args!("onBackPressed: minimize: {e:?}"));
        }
    }
}

impl ActivityClientController {
    /// getCallingRecord's (package, class) for `token`; None for no token, no services, or no owed result.
    fn calling(&self, token: Option<&SpIBinder>) -> Option<(String, String)> {
        self.inner.upgrade()?.atm.calling(token?)
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
        if let (Some(token), Some(inner)) = (token, self.inner.upgrade()) {
            if !inner.atm.resumed(token) {
                atl_diag::answered(ACC, "activityResumed: no such activity");
                return Ok(());
            }
            if let Some((task, input)) = inner.atm.top_orientation_changed(token) {
                self.reshape(&inner, task, &input, "top activity");
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

    /// ActivityClientController.activityDestroyed (oneway): the destroying record goes; the last one of a Task ends the Task's processes (ActivityRecord.destroyed ->
    /// killTaskProcessesOnDestroyedIfNeeded). Counted when no destroying record has the token.
    fn r#activityDestroyed(&self, token: Option<&SpIBinder>) -> binder::Result<()> {
        let known = match (token, self.inner.upgrade()) {
            (Some(token), Some(inner)) => inner.activity_destroyed(token),
            _ => false,
        };
        if !known {
            atl_diag::answered(ACC, "activityDestroyed: no such destroying activity");
        }
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
            Some(Back::TaskToBack { uid, task }) => self.task_to_back(uid, task),
            Some(Back::Finish) if !self.back_finish => {
                atl_diag::refused(ACC, "onBackPressed (--no-back-finish)");
            }
            known => {
                back_pressed(known.is_some(), callback.map(|c| &**c));
            }
        }
        Ok(())
    }

    /// ActivityClientController.setRequestedOrientation:826-840 -> ActivityRecord.setRequestedOrientation: the record keeps the request, then the Task gets AOSP's reshape check
    /// (notifyActivityRequestedOrientationChanged, every call). Not modelled: the API 36 large-screen "ignore" rule (isRestrictedFixedOrientation), which the initial bounds do not apply either.
    /// A Task whose reshape is due asks its window for the new size; the size the compositor grants comes back as SET_BOUNDS, and the configuration change follows from there (task_resized).
    fn r#setRequestedOrientation(
        &self,
        token: Option<&SpIBinder>,
        orientation: i32,
    ) -> binder::Result<()> {
        let (Some(token), Some(inner)) = (token, self.inner.upgrade()) else {
            atl_diag::answered(ACC, "setRequestedOrientation: no token");
            return Ok(());
        };
        let Some((task, input)) = inner.atm.set_requested_orientation(token, orientation) else {
            atl_diag::answered(ACC, "setRequestedOrientation: no such activity");
            return Ok(());
        };
        log(format_args!(
            "setRequestedOrientation task={} orientation={orientation}",
            task.raw()
        ));
        self.reshape(&inner, task, &input, "setRequestedOrientation");
        Ok(())
    }

    /// ActivityClientController.getRequestedOrientation:843-850.
    fn r#getRequestedOrientation(&self, token: Option<&SpIBinder>) -> binder::Result<i32> {
        Ok(match (token, self.inner.upgrade()) {
            (Some(token), Some(inner)) => inner.atm.requested_orientation(token),
            _ => SCREEN_ORIENTATION_UNSPECIFIED,
        })
    }

    /// ActivityClientController.getDisplayId:626-635: the root task's display, DEFAULT_DISPLAY when invalid or when the token has no task. M2 has one display.
    fn r#getDisplayId(&self, _token: Option<&SpIBinder>) -> binder::Result<i32> {
        Ok(DEFAULT_DISPLAY)
    }

    /// ActivityClientController.getCallingActivity:692-697: the component of the record the activity owes its result to (getCallingRecord), null when not started for a result.
    fn r#getCallingActivity(
        &self,
        token: Option<&SpIBinder>,
    ) -> binder::Result<Option<ComponentName>> {
        Ok(self
            .calling(token)
            .map(|(package, class)| ComponentName { package, class }))
    }

    /// ActivityClientController.getCallingPackage:699-704: that record's package, null when not started for a result.
    fn r#getCallingPackage(&self, token: Option<&SpIBinder>) -> binder::Result<Option<String>> {
        Ok(self.calling(token).map(|(package, _)| package))
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

    /// ActivityClientController.setTaskDescription -> ActivityRecord.setTaskDescription + Task.updateTaskDescription. Activity.onApplyThemeResource sends it for every themed activity (label null),
    /// so each activity's create retitles. The label is kept per activity; the Task's merged label (ActivityTasks::set_task_label) and root activity go to the app's UI agent, whose window is
    /// titled as recents labels the task. Only the label is kept: colours and icon have no desktop consumer. Not recomputed when an activity leaves (AOSP also updates on removal).
    fn r#setTaskDescription(
        &self,
        token: Option<&SpIBinder>,
        values: Option<&ActivityManager_TaskDescription>,
    ) -> binder::Result<()> {
        let (Some(token), Some(inner)) = (token, self.inner.upgrade()) else {
            atl_diag::answered(ACC, "setTaskDescription: no token");
            return Ok(());
        };
        let label = values.and_then(|v| v.label.clone());
        let Some(title) = inner.atm.set_task_label(token, label) else {
            atl_diag::answered(ACC, "setTaskDescription: no such activity");
            return Ok(());
        };
        let Some(agent) = self.task_agent(title.task) else {
            atl_diag::answered(ACC, "setTaskDescription (kept, no UI agent)");
            return Ok(());
        };
        atl_diag::answered(ACC, "setTaskDescription (window titled)");
        if let Err(e) = agent.task_title(title.label.as_deref(), &title.package, &title.class) {
            log(format_args!("setTaskDescription: task title: {e:?}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use atl_aidl::aidl::android::app::IApplicationThread::BnApplicationThread;
    use atl_services_types::app_ops;
    use atl_services_types::display_model::Declaration;
    use binder::{BinderFeatures, Interface, StatusCode};

    use super::*;
    use crate::appops;
    use crate::packages::fixture;

    /// A process control that starts nothing: the broadcast tests below send to no receiver.
    struct NoProcesses;
    impl ProcessControl for NoProcesses {
        fn start(&self, _: &StartRequest) -> std::io::Result<i32> {
            Err(std::io::Error::other("no processes in this test"))
        }
        fn kill(&self, _: i32) -> std::io::Result<()> {
            Ok(())
        }
        fn program_pid(&self, pid: i32) -> std::io::Result<i32> {
            Ok(pid)
        }
        fn last_app_ended(&self, _: i32, _: bool) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn no_app_manager() -> ActivityManager {
        manager_with(Arc::new(NoProcesses))
    }

    /// An empty manager whose processes go through `control`.
    fn manager_with(control: Arc<dyn ProcessControl>) -> ActivityManager {
        let packages = Arc::new(PackageTable::new(Packages::new(
            Vec::new(),
            Default::default(),
        )));
        let permissions = Arc::new(Permissions::new(&packages.snapshot(), &[]));
        ActivityManager::new(
            packages,
            permissions,
            control,
            Arc::new(Tasks::new(DisplayModel::default(), None, None).expect("the default display")),
            Arc::new(UiAgents::default()),
            true,
            RootBack::default(),
            OrientationReshape::default(),
            true,
            uid_state::BACKGROUND_SETTLE_TIME_MS,
            true,
            Arc::new(crate::notification::NoForegroundNotices),
            HwTimeouts::default(),
        )
    }

    fn sticky_uids(am: &ActivityManager, action: &str) -> Vec<u32> {
        let filter = broadcast::Filter {
            actions: vec![action.to_owned()],
            ..Default::default()
        };
        let st = am.inner.state();
        st.broadcasts
            .stickies(&filter, USER_SYSTEM)
            .iter()
            .map(|s| s.calling_uid)
            .collect()
    }

    /// R7-3: what broadcastIntentWithFeature does for an app-uid binder caller. The battery's sticky ACTION_BATTERY_CHANGED needs BROADCAST_STICKY, which an app without it does not hold:
    /// SecurityException, nothing stored. Only the services' own entry ([`ActivityManager::broadcast_as_system`], no binder method) stores it, as SYSTEM_UID.
    #[test]
    fn app_caller_cannot_send_the_system_sticky() {
        const ACTION: &str = "android.intent.action.BATTERY_CHANGED";
        const APP_UID: u32 = 10_050;
        let am = no_app_manager();
        let intent = Intent {
            action: Some(ACTION.to_owned()),
            ..Intent::default()
        };
        let sent = am.broadcast_from_caller(
            (4242, APP_UID),
            (None, false),
            Some(&intent),
            None,
            false,
            Some(&[]),
            (0, None, None),
            true,
            USER_CURRENT,
            None,
        );
        let e = sent.expect_err("an app without BROADCAST_STICKY sends no sticky");
        assert_eq!(e.exception_code(), ExceptionCode::SECURITY);
        assert!(sticky_uids(&am, ACTION).is_empty());
        am.broadcast_as_system(&intent, true);
        assert_eq!(sticky_uids(&am, ACTION), vec![SYSTEM_UID]);
    }

    /// A process that never attaches (it died before attach, so no binder death comes) leaves at its start timeout; another process of the table stays. Its
    /// services are brought down by the same call (service_records.rs start_failed).
    #[test]
    fn unattached_process_is_removed_at_its_start_timeout() {
        let am = no_app_manager();
        let record = |start_seq: i64, pid: i32| ProcessRecord {
            package: "p".into(),
            process_name: "p:x".into(),
            uid: 10_001,
            start_seq,
            pid,
            attached: None,
            launch_activity: false,
            task: None,
            instr: None,
            attach_finished: false,
            touched: 0,
            kill_reason: None,
        };
        am.inner
            .state()
            .processes
            .extend([record(1, 41), record(2, 42)]);
        am.inner.process_start_timed_out(1);
        let seqs: Vec<i64> = am
            .inner
            .state()
            .processes
            .iter()
            .map(|r| r.start_seq)
            .collect();
        assert_eq!(seqs, [2]);
    }

    /// The startForeground timeout's ANR by the process's state (`foreground_anr`, AS :7640): a windowed process is only counted (no ANR dialog), a process that ended
    /// first is counted, a headless one is killed. The row fgstimeout proves the headless kill end to end; this pins the other two counters.
    #[test]
    fn fgs_timeout_anr_by_process_state() {
        let control = Arc::new(RecordingProcesses::default());
        let am = manager_with(control.clone());
        let record = |pid: i32, task: Option<TaskId>| ProcessRecord {
            package: "p".into(),
            process_name: "p:fgs".into(),
            uid: 10_001,
            start_seq: 1,
            pid,
            attached: None,
            launch_activity: false,
            task,
            instr: None,
            attach_finished: false,
            touched: 0,
            kill_reason: None,
        };
        am.inner
            .state()
            .processes
            .push(record(41, TaskId::from_raw(7)));
        am.inner.state().processes.push(record(43, None));
        let counted = |api: &str| {
            atl_diag::counts()
                .into_iter()
                .find(|(o, a, _)| o == "answered" && a == api)
                .map_or(0, |(_, _, n)| n)
        };
        let windowed = "IActivityManager.fgs start timeout: ANR not modelled";
        let ended = "IActivityManager.fgs start timeout: process ended before the ANR";
        let headless = "IActivityManager.fgs start timeout: headless process ended (silent bg ANR)";
        let before = (counted(windowed), counted(ended), counted(headless));
        am.inner.foreground_anr(41);
        am.inner.foreground_anr(42);
        am.inner.foreground_anr(43);
        assert_eq!(counted(windowed) - before.0, 1, "windowed: counted");
        assert_eq!(counted(ended) - before.1, 1, "no such process: counted");
        assert_eq!(counted(headless) - before.2, 1, "headless: counted");
        assert_eq!(
            *control.kills.lock().expect("kills"),
            [43],
            "only the headless process is killed"
        );
    }

    /// Records the process starts, in order, and the kills a test makes.
    #[derive(Default)]
    struct RecordingProcesses {
        starts: Mutex<Vec<String>>,
        target_sdks: Mutex<Vec<i32>>,
        kills: Mutex<Vec<i32>>,
    }

    impl ProcessControl for RecordingProcesses {
        fn start(&self, request: &StartRequest) -> std::io::Result<i32> {
            self.target_sdks
                .lock()
                .expect("target_sdks")
                .push(request.target_sdk);
            let mut starts = self.starts.lock().expect("starts");
            starts.push(request.process_name.clone());
            // any pid: the n-th start gets n
            Ok(starts.len() as i32)
        }
        fn kill(&self, pid: i32) -> std::io::Result<()> {
            self.kills.lock().expect("kills").push(pid);
            Ok(())
        }
        fn program_pid(&self, pid: i32) -> std::io::Result<i32> {
            Ok(pid)
        }
        fn last_app_ended(&self, _: i32, _: bool) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An app thread waiting for a provider: each publish status it is told, in order.
    struct PublishStatus {
        told: Arc<Mutex<Vec<bool>>>,
    }

    impl Interface for PublishStatus {}

    impl IApplicationThread for PublishStatus {
        fn r#notifyContentProviderPublishStatus(
            &self,
            _holder: Option<&ContentProviderHolder>,
            _authority: Option<&str>,
            _user_id: i32,
            published: bool,
        ) -> binder::Result<()> {
            self.told.lock().expect("told").push(published);
            Ok(())
        }
    }

    /// A caller's app thread binder, and the list of the publish statuses it is told.
    fn waiting_caller() -> (Strong<dyn IApplicationThread>, Arc<Mutex<Vec<bool>>>) {
        let told = Arc::new(Mutex::new(Vec::new()));
        let thread = BnApplicationThread::new_binder(
            PublishStatus {
                told: Arc::clone(&told),
            },
            BinderFeatures::default(),
        );
        (thread, told)
    }

    const PROVIDER_PACKAGE: &str = "com.example.provider";
    const PROVIDER_CLASS: &str = "com.example.provider.Data";
    const PROVIDER_AUTHORITY: &str = "com.example.provider.data";
    /// the uid of the provider's package and of every caller here (one app)
    const PROVIDER_UID: u32 = 10_321;

    /// One installed package with one provider, hosted by its default process.
    fn provider_manager(control: Arc<RecordingProcesses>) -> ActivityManager {
        let mut package = fixture::record(PROVIDER_PACKAGE);
        package.uid = PROVIDER_UID;
        package.info.providers = Some(vec![Some(ProviderInfo {
            component: fixture::component(PROVIDER_PACKAGE, PROVIDER_CLASS),
            authority: Some(PROVIDER_AUTHORITY.to_owned()),
            ..Default::default()
        })]);
        let packages = Arc::new(PackageTable::new(Packages::new(
            vec![package],
            Default::default(),
        )));
        let permissions = Arc::new(Permissions::new(&packages.snapshot(), &[]));
        ActivityManager::new(
            packages,
            permissions,
            control,
            Arc::new(Tasks::new(DisplayModel::default(), None, None).expect("the default display")),
            Arc::new(UiAgents::default()),
            true,
            RootBack::default(),
            OrientationReshape::default(),
            true,
            uid_state::BACKGROUND_SETTLE_TIME_MS,
            true,
            Arc::new(crate::notification::NoForegroundNotices),
            HwTimeouts::default(),
        )
    }

    /// getContentProviderImpl for `caller` (an app thread of PROVIDER_UID): the holder, without a provider while its process has not published it.
    fn ask_for_provider(
        am: &ActivityManager,
        caller: &Strong<dyn IApplicationThread>,
    ) -> Option<ContentProviderHolder> {
        let mut st = am.inner.state();
        am.inner
            .content_provider_locked(&mut st, caller, PROVIDER_UID, PROVIDER_AUTHORITY)
            .expect("no binder error")
    }

    /// The cold start of a provider, then its process never attaching (AMS handleProcessStartOrKillTimeoutLocked :4444-4491): every waiter is told false, the record goes,
    /// and the next request starts the process again.
    #[test]
    fn provider_waiters_are_released_when_their_process_never_attaches() {
        let control = Arc::new(RecordingProcesses::default());
        let am = provider_manager(Arc::clone(&control));
        let (first, first_told) = waiting_caller();
        let (second, second_told) = waiting_caller();
        let holder = ask_for_provider(&am, &first).expect("a holder for the first caller");
        assert!(holder.provider.is_none(), "not published yet");
        ask_for_provider(&am, &second).expect("a holder for the second caller");
        assert_eq!(
            control.starts.lock().expect("starts").len(),
            1,
            "the second request waits on the starting process: one start"
        );
        let (start_seq, pid) = {
            let st = am.inner.state();
            (st.processes[0].start_seq, st.processes[0].pid)
        };
        am.inner.process_start_timed_out(start_seq);
        assert_eq!(*first_told.lock().expect("told"), [false]);
        assert_eq!(*second_told.lock().expect("told"), [false]);
        assert!(am.inner.state().providers.is_empty());
        assert_eq!(*control.kills.lock().expect("kills"), [pid]);
        ask_for_provider(&am, &first).expect("a holder after the release");
        assert_eq!(
            control.starts.lock().expect("starts").len(),
            2,
            "no record is left: the next request starts the process again"
        );
    }

    /// forceStopPackageLocked of the package whose provider process never attached: the same end releases its waiters (AMS :4385-4393 for the providers, :4360 for the kill).
    #[test]
    fn force_stop_releases_the_waiters_of_an_unattached_provider_process() {
        let control = Arc::new(RecordingProcesses::default());
        let am = provider_manager(Arc::clone(&control));
        let (caller, told) = waiting_caller();
        ask_for_provider(&am, &caller).expect("a holder for the caller");
        am.force_stop_package(PROVIDER_PACKAGE);
        assert_eq!(*told.lock().expect("told"), [false]);
        assert!(am.inner.state().providers.is_empty());
        assert!(am.inner.state().processes.is_empty());
    }

    /// A force stop is a process death (forceStopPackageInternalLocked, AMS :4309): the attached process's launch Task, its activity and the providers it published
    /// go with it, as they do when it dies (app_died). Red before process_out_locked: all three stayed in the table.
    #[test]
    fn force_stop_drops_the_providers_tasks_and_activities_of_an_attached_process() {
        let control = Arc::new(RecordingProcesses::default());
        let am = provider_manager(Arc::clone(&control));
        let (caller, _) = waiting_caller();
        ask_for_provider(&am, &caller).expect("a holder for the caller");
        let (app, _) = waiting_caller();
        let task = am.inner.tasks.new_task(
            PROVIDER_UID,
            PROVIDER_PACKAGE,
            TaskSize {
                width: 1080,
                height: 1920,
            },
        );
        let now = am.inner.now_ms();
        let pid = {
            let mut st = am.inner.state();
            st.uids.on_task_added(PROVIDER_UID, now);
            st.open_tasks.insert(task, PROVIDER_UID);
            st.processes[0].attached = Some(Attached {
                thread: app.clone(),
                program_pid: None,
                _death: DeathRecipient::new(|| {}),
            });
            st.processes[0].task = Some(task);
            let pid = st.processes[0].pid;
            // published: the provider's binder stands in for the app's IContentProvider; its waiters were told at publication
            st.providers[0].provider = Some(app.as_binder());
            st.providers[0].pid = Some(pid);
            st.providers[0].waiters.clear();
            pid
        };
        let decl = fixture::activity(PROVIDER_PACKAGE, PROVIDER_CLASS);
        am.activity_tasks().launch_launcher(
            app.clone(),
            pid,
            PROVIDER_UID,
            PROVIDER_PACKAGE,
            &decl,
            task,
        );
        assert!(
            am.activity_tasks().hosts_activity(&app.as_binder()),
            "the activity is in the stack before the stop"
        );
        am.force_stop_package(PROVIDER_PACKAGE);
        let activity = am.activity_tasks().hosts_activity(&app.as_binder());
        let left = {
            let st = am.inner.state();
            (st.processes.len(), st.providers.len(), st.open_tasks.len())
        };
        assert_eq!(
            (left, activity),
            ((0, 0, 0), false),
            "after the force stop: (processes, providers, launch Tasks) left, and the activity's presence"
        );
        assert_eq!(*control.kills.lock().expect("kills"), [pid]);
    }

    /// The start every caller reaches: start_process_locked for the provider package's default process, under the state lock.
    fn start_provider_process(am: &ActivityManager) -> std::io::Result<()> {
        let mut st = am.inner.state();
        let packages = am.inner.packages.snapshot();
        let p = packages
            .by_name(PROVIDER_PACKAGE)
            .expect("the provider package is installed");
        am.inner
            .start_process_locked(&mut st, p, PROVIDER_PACKAGE, false, None)
    }

    /// Starts process `process_name` of the provider package; its host pid when accepted (the record the start leaves is found by its name).
    fn start_provider_process_as(am: &ActivityManager, process_name: &str) -> std::io::Result<i32> {
        let mut st = am.inner.state();
        let packages = am.inner.packages.snapshot();
        let p = packages
            .by_name(PROVIDER_PACKAGE)
            .expect("the provider package is installed");
        am.inner
            .start_process_locked(&mut st, p, process_name, false, None)?;
        Ok(st
            .processes
            .iter()
            .find(|r| r.process_name == process_name)
            .expect("the start recorded its process")
            .pid)
    }

    /// PackageFreezer.freezePackage counts: two freezes need two drops. A frozen package's start is refused (ComputerEngine.getPackageStartability :3643, the text
    /// PackageManagerService :4642); the first start after the last drop runs. Red before the gate: the start goes through.
    #[test]
    fn a_frozen_package_does_not_start() {
        let control = Arc::new(RecordingProcesses::default());
        let am = provider_manager(Arc::clone(&control));
        let first = am.freeze_package(PROVIDER_PACKAGE);
        let second = am.freeze_package(PROVIDER_PACKAGE);
        let refused = start_provider_process(&am).expect_err("frozen: the start is refused");
        assert_eq!(
            refused.to_string(),
            format!("Package {PROVIDER_PACKAGE} is currently frozen!")
        );
        drop(first);
        start_provider_process(&am).expect_err("one freeze left: still refused");
        drop(second);
        start_provider_process(&am).expect("unfrozen: the start runs");
        assert_eq!(
            control.starts.lock().expect("starts").len(),
            1,
            "only the unfrozen start reached the control"
        );
    }

    /// A start takes its record from a snapshot taken before the state lock; a publish while it waits must not start the old record: start_process_locked re-reads
    /// the record under the lock. Red before the re-read: the process starts with the stale target SDK.
    #[test]
    fn a_start_waiting_for_the_lock_takes_the_published_record() {
        const UPDATED_TARGET_SDK: i32 = 30;
        let control = Arc::new(RecordingProcesses::default());
        let am = provider_manager(Arc::clone(&control));
        let update = am.inner.state();
        let (taken_tx, taken_rx) = std::sync::mpsc::channel();
        let inner = Arc::clone(&am.inner);
        let racer = std::thread::spawn(move || {
            let snapshot = inner.packages.snapshot();
            taken_tx.send(()).expect("the test waits for the snapshot");
            let p = snapshot
                .by_name(PROVIDER_PACKAGE)
                .expect("the provider package is installed");
            let mut st = inner.state();
            inner.start_process_locked(&mut st, p, PROVIDER_PACKAGE, false, None)
        });
        taken_rx.recv().expect("the start took its snapshot");
        // the update publishes the package with another target SDK while the start waits for the lock
        let mut updated = fixture::record(PROVIDER_PACKAGE);
        updated.uid = PROVIDER_UID;
        updated
            .info
            .application_info
            .as_mut()
            .expect("the fixture's application info")
            .target_sdk_version = UPDATED_TARGET_SDK;
        let next = am.inner.packages.snapshot().with_records(vec![updated]);
        am.inner.packages.publish(next);
        drop(update);
        racer
            .join()
            .expect("the racing start")
            .expect("the start is not refused");
        assert_eq!(
            *control.target_sdks.lock().expect("target_sdks"),
            [UPDATED_TARGET_SDK]
        );
    }

    /// freezePackage stops and counts in one hold (PackageFreezer.java :65-97). Four racers start processes of the package in a tight loop, each under its own process name
    /// (each start is real); the freeze lands among them. Once it returns, the kills are exactly the starts accepted before it: a refused start kills nothing.
    /// Red with the stop and the count in two holds: a start accepted between them is neither killed nor refused.
    #[test]
    fn freeze_kills_every_accepted_start() {
        const RACERS: usize = 4;
        const STARTS: usize = 64;
        for round in 0..200 {
            let control = Arc::new(RecordingProcesses::default());
            let am = provider_manager(Arc::clone(&control));
            let gate = Arc::new(std::sync::Barrier::new(RACERS + 1));
            let racers: Vec<_> = (0..RACERS)
                .map(|r| {
                    let (am, gate) = (am.clone(), Arc::clone(&gate));
                    std::thread::spawn(move || {
                        gate.wait();
                        (0..STARTS)
                            .filter_map(|s| {
                                let name = format!("{PROVIDER_PACKAGE}:r{r}s{s}");
                                start_provider_process_as(&am, &name).ok()
                            })
                            .collect::<Vec<i32>>()
                    })
                })
                .collect();
            gate.wait();
            let frozen = am.freeze_package(PROVIDER_PACKAGE);
            let mut accepted: Vec<i32> = racers
                .into_iter()
                .flat_map(|t| t.join().expect("a racing start"))
                .collect();
            let mut killed = control.kills.lock().expect("kills").clone();
            drop(frozen);
            accepted.sort_unstable();
            killed.sort_unstable();
            assert_eq!(
                killed, accepted,
                "round {round}: the freeze kills the starts accepted before it, and a refused start kills nothing"
            );
        }
    }

    /// A process of (process, uid) that another package owns cannot publish this provider (publishContentProviders matches the publisher's package): the request gets
    /// no provider, starts nothing, and waits for nothing.
    #[test]
    fn provider_is_refused_when_another_package_owns_its_process() {
        let control = Arc::new(RecordingProcesses::default());
        let am = provider_manager(Arc::clone(&control));
        am.inner.state().processes.push(ProcessRecord {
            package: "com.example.other".to_owned(),
            process_name: PROVIDER_PACKAGE.to_owned(),
            uid: PROVIDER_UID,
            start_seq: 1,
            pid: 77,
            attached: None,
            launch_activity: false,
            task: None,
            instr: None,
            attach_finished: false,
            touched: 0,
            kill_reason: None,
        });
        let (caller, told) = waiting_caller();
        assert!(ask_for_provider(&am, &caller).is_none());
        assert!(am.inner.state().providers.is_empty());
        assert!(control.starts.lock().expect("starts").is_empty());
        assert!(told.lock().expect("told").is_empty());
    }

    /// The global configuration's locale: the host's UI language tag as given, else AOSP's en-US (LocaleList needs one; ActivityThread.handleBindApplication).
    #[test]
    fn global_configuration_locale_is_the_host_tag_or_the_default() {
        let display = DisplayModel::default();
        let config = global_configuration(&display, Some("zh-TW"));
        assert_eq!(config.locale_list.as_deref(), Some("zh-TW"));
        let config = global_configuration(&display, None);
        assert_eq!(config.locale_list.as_deref(), Some("en-US"));
    }

    /// The declared 411x914 dp at 160 dpi (px = dp) and 256 dpi (px = dp x 1.6): 411 x 1.6 = 657.6 -> 658 px, 914 x 1.6 = 1462.4 -> 1462 px.
    /// Back to dp as global_configuration does (px / 1.6, rounded): 658 / 1.6 = 411.25 -> 411, 1462 / 1.6 = 913.75 -> 914. The smallest width is 411 at both densities.
    #[test]
    fn global_configuration_of_a_declared_display_is_411_dp_at_every_density() {
        let declared = Declaration {
            width_dp: 411,
            height_dp: 914,
        };
        for (dpi, px) in [(160, (411, 914)), (256, (658, 1462))] {
            let monitor = DisplayModel {
                dpi,
                ..DisplayModel::default()
            };
            let display =
                DisplayModel::of(monitor, Some(declared)).expect("411x914 dp is in bounds");
            assert_eq!((display.width, display.height), px);
            let config = global_configuration(&display, None);
            assert_eq!(config.density_dpi, dpi);
            assert_eq!(
                (config.screen_width_dp, config.screen_height_dp),
                (411, 914)
            );
            assert_eq!(config.smallest_screen_width_dp, 411);
        }
    }

    /// DisplayContent.computeScreenConfiguration:2486-2488 + computeScreenAppConfiguration:2458: bounds, appBounds and maxBounds of the global configuration are the display from the origin.
    #[test]
    fn global_configuration_bounds_are_the_display() {
        let display = DisplayModel {
            width: 1440,
            height: 900,
            ..DisplayModel::default()
        };
        let w = global_configuration(&display, None).window_configuration;
        let want = Rect {
            left: 0,
            top: 0,
            right: 1440,
            bottom: 900,
        };
        assert_eq!(
            (w.bounds, w.app_bounds, w.max_bounds),
            (want.clone(), Some(want.clone()), want)
        );
    }

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
        let back = Back::TaskToBack {
            uid: 10_001,
            task: TaskId::from_raw(1).expect("task number"),
        };
        assert_eq!(under_policy(back, RootBack::Minimize), back);
        assert_eq!(under_policy(back, RootBack::Close), Back::Finish);
        assert_eq!(under_policy(Back::Finish, RootBack::Minimize), Back::Finish);
        assert_eq!(under_policy(Back::Finish, RootBack::Close), Back::Finish);
    }

    /// AppOpsManager.OP_TAKE_AUDIO_FOCUS: MODE_FOREGROUND by default, no capability.
    const OP_TAKE_AUDIO_FOCUS: i32 = 32;

    /// An attached ProcessRecord for foreground-settle tests; tests call `app_died` / `force_stop_package` directly so no binder death link is registered.
    fn attached_process(uid: u32, pid: i32, task: Option<TaskId>) -> ProcessRecord {
        let (thread, _) = waiting_caller();
        ProcessRecord {
            package: "com.example.app".into(),
            process_name: "com.example.app".into(),
            uid,
            start_seq: i64::from(pid),
            pid,
            attached: Some(Attached {
                thread,
                program_pid: Some(pid),
                _death: DeathRecipient::new(|| {}),
            }),
            launch_activity: task.is_some(),
            task,
            instr: None,
            attach_finished: true,
            touched: 0,
            kill_reason: None,
        }
    }

    /// Pins that once a uid's Task ends and its last process dies (or is force-stopped), a new process restarting 1 s later gets no 5 s foreground settle (AppOpsUidStateTrackerImpl.updateUidProcState :242-248, commitUidPendingState :402-405); break: omitting `process_out_locked` in `app_died` or `force_stop_package_locked`.
    #[test]
    fn last_process_death_ends_foreground_settle() {
        const UID: u32 = 10_401;
        let am = no_app_manager();
        let task = TaskId::from_raw(1).expect("task id");
        let t = am.inner.now_ms();
        {
            let mut st = am.inner.state();
            st.open_tasks.insert(task, UID);
            st.uids.on_task_added(UID, t);
            st.processes.push(attached_process(UID, 41, Some(task)));
        }
        am.inner.task_gone(task);
        am.inner.app_died(41);
        am.inner
            .state()
            .processes
            .push(attached_process(UID, 42, None));
        let mut view = am.uid_view(UID);
        view.now = view.restricted_since.unwrap_or(t) + 1_000;
        assert_eq!(view.state, Some(ProcessState::CachedEmpty));
        assert_eq!(view.restricted_since, None);
        assert_eq!(
            appops::eval_mode(OP_TAKE_AUDIO_FOCUS, app_ops::FOREGROUND, &view),
            app_ops::IGNORED
        );
        let task2 = TaskId::from_raw(2).expect("task id");
        let t2 = am.inner.now_ms();
        {
            let mut st = am.inner.state();
            st.open_tasks.insert(task2, UID);
            st.uids.on_task_added(UID, t2);
        }
        am.inner.task_gone(task2);
        am.force_stop_package("com.example.app");
        am.inner
            .state()
            .processes
            .push(attached_process(UID, 43, None));
        let mut view = am.uid_view(UID);
        view.now = view.restricted_since.unwrap_or(t2) + 1_000;
        assert_eq!(view.restricted_since, None);
        assert_eq!(
            appops::eval_mode(OP_TAKE_AUDIO_FOCUS, app_ops::FOREGROUND, &view),
            app_ops::IGNORED
        );
    }

    /// Pins that a uid whose Task ends while its process stays alive keeps the 5 s foreground settle (AppOpsUidStateTrackerImpl.updateUidProcState :262-277); break: clearing `restricted_since` in `task_gone_locked` while a live process remains.
    #[test]
    fn surviving_process_keeps_foreground_settle() {
        const UID: u32 = 10_402;
        let am = no_app_manager();
        let task = TaskId::from_raw(1).expect("task id");
        let t = am.inner.now_ms();
        {
            let mut st = am.inner.state();
            st.open_tasks.insert(task, UID);
            st.uids.on_task_added(UID, t);
            st.processes.push(attached_process(UID, 51, Some(task)));
        }
        am.inner.task_gone(task);
        let view = am.uid_view(UID);
        assert_eq!(view.state, Some(ProcessState::CachedEmpty));
        let since = view.restricted_since.expect("live process keeps settle");
        let at = |now| UidView { now, ..view };
        assert_eq!(
            appops::eval_mode(OP_TAKE_AUDIO_FOCUS, app_ops::FOREGROUND, &at(since + 4_999)),
            app_ops::ALLOWED
        );
        assert_eq!(
            appops::eval_mode(OP_TAKE_AUDIO_FOCUS, app_ops::FOREGROUND, &at(since + 5_000)),
            app_ops::IGNORED
        );
    }

    /// Pins that when one of a uid's two attached processes dies with its Task, the surviving process keeps the 5 s foreground settle until it also dies (AppOpsUidStateTrackerImpl.updateUidProcState :242-243, :262-277); break: calling `on_uid_gone` on any process death without checking for another attached process of the uid.
    #[test]
    fn one_of_two_processes_dying_keeps_foreground_settle() {
        const UID: u32 = 10_403;
        let am = no_app_manager();
        let task = TaskId::from_raw(1).expect("task id");
        let t = am.inner.now_ms();
        {
            let mut st = am.inner.state();
            st.open_tasks.insert(task, UID);
            st.uids.on_task_added(UID, t);
            st.processes.push(attached_process(UID, 61, Some(task)));
            st.processes.push(attached_process(UID, 62, None));
        }
        am.inner.app_died(61);
        let view = am.uid_view(UID);
        assert_eq!(view.state, Some(ProcessState::CachedEmpty));
        let since = view.restricted_since.expect("second process keeps settle");
        let at = |now| UidView { now, ..view };
        assert_eq!(
            appops::eval_mode(OP_TAKE_AUDIO_FOCUS, app_ops::FOREGROUND, &at(since + 1_000)),
            app_ops::ALLOWED
        );
        assert_eq!(
            appops::eval_mode(OP_TAKE_AUDIO_FOCUS, app_ops::FOREGROUND, &at(since + 5_000)),
            app_ops::IGNORED
        );
        am.inner.app_died(62);
        am.inner
            .state()
            .processes
            .push(attached_process(UID, 63, None));
        let mut view = am.uid_view(UID);
        assert_eq!(view.restricted_since, None);
        view.now = since + 1_000;
        assert_eq!(
            appops::eval_mode(OP_TAKE_AUDIO_FOCUS, app_ops::FOREGROUND, &view),
            app_ops::IGNORED
        );
    }
}
