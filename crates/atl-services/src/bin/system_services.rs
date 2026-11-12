// SPDX-License-Identifier: MIT
//! atl-system-services: registers the M2 services and the window service with servicemanager (`activity`, `package`, `display`, `window`, as Context.ACTIVITY_SERVICE / PACKAGE_SERVICE /
//! DISPLAY_SERVICE / WINDOW_SERVICE / INPUT_METHOD_SERVICE name them) and the compositor registry (compositor_registry.rs: how an app's in-process SurfaceFlinger gets registered) and serves them on libbinder's thread pool, until killed. Runs as the system uid: servicemanager accepts addService from it. The packages come
//! from a record file (atld's install step writes it), the shared libraries and the display from arguments (`--display`: display_model.rs; omitted, the model's default). Processes are started
//! and killed through the control connection atld passes as stdin ([`atl_services::process_control`]).
//! Prints `services: ready` once all are registered; unimplemented calls are counted on stderr (atl_services_types::counters).

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
extern crate binder;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use atl_aidl::aidl::android::app::IActivityManager::BnActivityManager;
use atl_aidl::aidl::android::content::pm::IPackageManager::BnPackageManager;
use atl_services::activity::{ActivityManager, RootBack};
use atl_services::package::PackageManager;
use atl_services::process_control::ControlSocket;
use atl_services::ui_agents::UiAgents;
use atl_services_types::compositor_registry;
use atl_services_types::display_model::DisplayModel;
use atl_services_types::packages::{Library, PackageRecord, Packages};
use atl_services_types::permission::Permissions;
use binder::{BinderFeatures, ProcessState};
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// The installed packages: a JSON array of `PackageRecord`s (atl_services_types::packages), paths as the app processes see them.
    #[arg(long)]
    packages: PathBuf,
    /// A shared library an app may `<uses-library>`: `<name>=<jar path as the app processes see it>[,<dependency name>...]` (repeat). SystemConfig's `<library name file dependency>`.
    #[arg(long = "shared-library", value_parser = Library::parse_arg)]
    shared_libraries: Vec<(String, Library)>,
    /// A package whose launcher activity starts once the services are up (the stand-in for a home screen tap at boot; later starts come as startActivity with no caller app, `atl-am`).
    #[arg(long)]
    launch: Option<String>,
    /// The display the services answer with (`WxH@hz,dpi`, display_model.rs). Omitted: the model's default.
    #[arg(long, value_parser = DisplayModel::parse_arg)]
    display: Option<DisplayModel>,
    /// AOSP's keychars directory (/system/usr/keychars): Virtual.kcm (the virtual keyboard's map, also carried by the pointer devices) and Generic.kcm (the physical keyboard's), loaded through libinput at start.
    #[arg(long)]
    keychars: PathBuf,
    /// Test only: input method clients get no death link, the deliberate break of the ime gate's death sub-gate (a dead app's client then stays in the table).
    #[arg(long)]
    no_ime_death_link: bool,
    /// Test only: a start that would reuse a singleTop / singleTask instance (onNewIntent) is refused, the deliberate break of the start-finish stage's new-intent sub-gate.
    #[arg(long)]
    no_new_intent: bool,
    /// Test only: a start owes its caller no result, so no onActivityResult arrives (none is delivered or cancelled), the deliberate break of the app gate's result sub-gate.
    #[arg(long)]
    no_activity_result: bool,
    /// Test only: onBackPressed sends no requestFinish, the deliberate break of the dialog gate's back sub-gate (no activity finishes on an unhandled Back).
    #[arg(long)]
    no_back_finish: bool,
    /// What an unhandled Back on a Task's launcher root does: `minimize` (Android 12+ moveTaskToBack; the app keeps running) or `close` (the root finishes; pre-Android-12).
    #[arg(long, value_parser = parse_root_back, default_value = "minimize")]
    root_back: RootBack,
    /// Test only: `sensorservice` is not registered, the deliberate break of the sensors gate's empty stage (SensorManager then waits for it, SensorManager.cpp `waitForSensorService`).
    #[arg(long)]
    no_sensor_service: bool,
    /// Test only: this service (`connectivity`, `permissionmgr`) is not registered (repeat), the deliberate break of the app gate's line for it (its manager is then null).
    #[arg(long)]
    omit_service: Vec<String>,
    /// Start `sensorservice` in HAL bypass data injection mode with this package allow-listed (AOSP `dumpsys sensorservice hal_bypass_replay_data_injection <package>`); refused while the machine has a real accelerometer. atld `--tilt-keys` sets it.
    #[arg(long)]
    sensor_injection: Option<String>,
    /// Test only: injected events are accepted from any process, not only the focused window's, the deliberate break of the sensors gate's tilt `focus` sub-gate.
    #[arg(long)]
    no_sensor_focus_check: bool,
}

/// The names the framework looks the services up by (Context.ACTIVITY_SERVICE, Context.PACKAGE_SERVICE / ActivityThread.getPackageManager).
const ACTIVITY: &str = "activity";
/// Context.ACTIVITY_TASK_SERVICE (Context.java:4730).
const ACTIVITY_TASK: &str = "activity_task";
const PACKAGE: &str = "package";
/// AMS publishes ProcessInfoService as "processinfo" (ActivityManagerService.setSystemProcess).
const PROCESS_INFO: &str = "processinfo";
/// Context.DISPLAY_SERVICE.
const DISPLAY: &str = "display";
/// Context.WINDOW_SERVICE.
const WINDOW: &str = "window";
/// Context.INPUT_SERVICE (InputManagerGlobal.java:172-181 looks it up by this name).
const INPUT: &str = "input";
/// Context.INPUT_METHOD_SERVICE (Context.java:5471).
const INPUT_METHOD: &str = "input_method";
/// ContentResolver.CONTENT_SERVICE_NAME (ContentResolver.java:3925).
const CONTENT: &str = "content";
/// Context.AUDIO_SERVICE.
const AUDIO: &str = "audio";
/// Context.USER_SERVICE.
const USER: &str = "user";
/// The name StorageManager looks IStorageManager up by (StorageManager.java:326).
const MOUNT: &str = "mount";
/// Context.CLIPBOARD_SERVICE.
const CLIPBOARD: &str = "clipboard";
/// Context.NOTIFICATION_SERVICE (Toast.getService looks INotificationManager up by it).
const NOTIFICATION: &str = "notification";
/// Context.ALARM_SERVICE (Context.java:4750).
const ALARM: &str = "alarm";
/// The name libsensor's SensorManager waits for (SensorManager.cpp `waitForSensorService`).
const SENSOR: &str = "sensorservice";
/// Context.CONNECTIVITY_SERVICE (ConnectivityFrameworkInitializer.registerServiceWrappers looks it up by this name).
const CONNECTIVITY: &str = "connectivity";
/// The name PermissionManager's constructor looks IPermissionManager up by (PermissionManager.java getServiceOrThrow("permissionmgr")).
const PERMISSION_MANAGER: &str = "permissionmgr";

/// `--root-back`'s values.
fn parse_root_back(s: &str) -> Result<RootBack, String> {
    match s {
        "minimize" => Ok(RootBack::Minimize),
        "close" => Ok(RootBack::Close),
        _ => Err(format!("{s}: expected minimize or close")),
    }
}

fn main() -> Result<()> {
    let a = Args::parse();
    let display = a.display.unwrap_or_default();
    let text = std::fs::read_to_string(&a.packages)
        .with_context(|| format!("reading {}", a.packages.display()))?;
    let list: Vec<PackageRecord> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", a.packages.display()))?;
    let libraries: BTreeMap<String, Library> = a.shared_libraries.into_iter().collect();
    let packages = Arc::new(Packages::new(list, libraries));
    // the install-time grants every service checks callers against (permission.rs, rule A)
    let permissions = Arc::new(Permissions::new(Arc::clone(&packages)));
    let control = Arc::new(ControlSocket::from_stdin().context("the process control on stdin")?);
    ProcessState::start_thread_pool();
    // the UI agents the registry takes in REGISTER, read by the window service at each window's first relayout
    let agents = Arc::new(UiAgents::default());
    // the Tasks of the display: each app's window size (tasks.rs), read by the window service and the registry; the activity service creates each launch's Task here
    let tasks = Arc::new(atl_services::tasks::Tasks::new(display));
    // the display listeners of the apps (IDisplayManager.registerCallback): the display service takes them, the registry tells them of a change
    let display_callbacks = Arc::new(atl_services::display::DisplayCallbacks::default());
    let activity = ActivityManager::new(
        packages.clone(),
        Arc::clone(&permissions),
        control,
        Arc::clone(&tasks),
        Arc::clone(&agents),
        !a.no_back_finish,
        a.root_back,
    );
    if let Some(package) = &a.launch
        && packages.by_name(package).is_none()
    {
        bail!("--launch {package}: not an installed package");
    }
    let am = BnActivityManager::new_binder(activity.clone(), BinderFeatures::default());
    binder::add_service(ACTIVITY, am.as_binder()).context("addService activity")?;
    binder::add_service(
        ACTIVITY_TASK,
        atl_services::activity_task::ActivityTaskService::new_binder(
            activity.clone(),
            !a.no_new_intent,
            !a.no_activity_result,
        ),
    )
    .context("addService activity_task")?;
    let pm = BnPackageManager::new_binder(
        PackageManager::new(packages.clone(), Arc::clone(&permissions)),
        BinderFeatures::default(),
    );
    binder::add_service(PACKAGE, pm.as_binder()).context("addService package")?;
    let focus = Arc::new(atl_services::focus::FocusedProcess::default());
    binder::add_service(
        DISPLAY,
        atl_services::display::new_binder(
            Arc::clone(&tasks),
            Arc::clone(&display_callbacks),
            Arc::clone(&permissions),
        ),
    )
    .context("addService display")?;
    binder::add_service(
        WINDOW,
        atl_services::window::new_binder(
            Arc::clone(&agents),
            Arc::clone(&tasks),
            Arc::clone(&permissions),
        ),
    )
    .context("addService window")?;
    binder::add_service(
        INPUT_METHOD,
        atl_services::input_method::new_binder(
            Arc::clone(&permissions),
            Arc::clone(&agents),
            !a.no_ime_death_link,
            Arc::clone(&focus),
        ),
    )
    .context("addService input_method")?;
    binder::add_service(
        CONTENT,
        atl_services::content::new_binder(Arc::clone(&permissions)),
    )
    .context("addService content")?;
    binder::add_service(
        AUDIO,
        atl_services::audio::new_binder(Arc::clone(&permissions)),
    )
    .context("addService audio")?;
    binder::add_service(USER, atl_services::user::new_binder()).context("addService user")?;
    binder::add_service(PROCESS_INFO, atl_services::process_info::new_binder())
        .context("addService processinfo")?;
    binder::add_service(
        MOUNT,
        atl_services::storage::new_binder(Arc::clone(&permissions)),
    )
    .context("addService mount")?;
    binder::add_service(
        CLIPBOARD,
        atl_services::clipboard::new_binder(Arc::clone(&permissions)),
    )
    .context("addService clipboard")?;
    if !a.omit_service.iter().any(|s| s == CONNECTIVITY) {
        binder::add_service(
            CONNECTIVITY,
            atl_services::connectivity::new_binder(Arc::clone(&permissions)),
        )
        .context("addService connectivity")?;
    }
    if !a.omit_service.iter().any(|s| s == PERMISSION_MANAGER) {
        binder::add_service(
            PERMISSION_MANAGER,
            atl_services::permission_manager::new_binder(
                Arc::clone(&packages),
                Arc::clone(&permissions),
            ),
        )
        .context("addService permissionmgr")?;
    }
    binder::add_service(
        NOTIFICATION,
        atl_services::notification::new_binder(
            Arc::clone(&tasks),
            Arc::clone(&permissions),
            Arc::clone(&packages),
        ),
    )
    .context("addService notification")?;
    binder::add_service(
        ALARM,
        atl_services::alarm::new_binder(Arc::clone(&packages), Arc::clone(&permissions)),
    )
    .context("addService alarm")?;
    if !a.no_sensor_service {
        // the machine's sensors through the platform layer, listed once at start
        let source: Arc<dyn atl_platform_host::Sensors> =
            Arc::new(atl_platform_host::PlatformSensors::default());
        let hub = atl_services::sensor::SensorHub::new(
            source,
            a.sensor_injection.as_deref(),
            (!a.no_sensor_focus_check).then(|| Arc::clone(&focus)),
        );
        writeln!(
            std::io::stderr(),
            "sensor: {} sensors listed",
            hub.sensor_list().len()
        )?;
        binder::add_service(SENSOR, atl_services::sensor_wire::new_binder(hub))
            .context("addService sensorservice")?;
    }
    // the bare file name is the load name the Parcel form carries (atl-libinput KeyCharacterMap::load)
    let kcm = |name: &str| -> anyhow::Result<Vec<u8>> {
        let path = a.keychars.join(name);
        let contents =
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let map = atl_libinput::KeyCharacterMap::load(name, &contents)
            .with_context(|| format!("loading {}", path.display()))?;
        Ok(map.parcel_bytes())
    };
    let input = atl_services::input::new_binder(
        &kcm("Virtual.kcm")?,
        &kcm("Generic.kcm")?,
        Arc::clone(&agents),
        Arc::clone(&permissions),
    )
    .map_err(anyhow::Error::msg)?;
    binder::add_service(INPUT, input).context("addService input")?;
    binder::add_service(
        compositor_registry::SERVICE,
        atl_services::registry_service::new_binder(
            Arc::clone(&agents),
            Arc::clone(&tasks),
            activity.clone(),
            display_callbacks,
        ),
    )
    .context("addService compositor registry")?;
    writeln!(std::io::stderr(), "services: ready")?;
    if let Some(package) = &a.launch {
        activity
            .start_launcher(package)
            .map_err(anyhow::Error::msg)?;
    }
    ProcessState::join_thread_pool();
    Ok(())
}
