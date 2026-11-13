// SPDX-License-Identifier: MIT
//! atl-system-services: registers the M2 services and the window service with servicemanager (`activity`, `package`, `display`, `window`, as Context.ACTIVITY_SERVICE / PACKAGE_SERVICE /
//! DISPLAY_SERVICE / WINDOW_SERVICE / INPUT_METHOD_SERVICE name them) and the compositor registry (compositor_registry.rs: how an app's in-process SurfaceFlinger gets registered) and serves them on libbinder's thread pool, until killed. Runs as the system uid: servicemanager accepts addService from it. The packages are
//! installed at start from the sessions atld holds files for (`--session`), the shared libraries and the display come from arguments (`--display`: display_model.rs; omitted, the model's default). Processes are started
//! and killed through the control connection atld passes as stdin ([`atl_services::process_control`]).
//! Prints `services: ready` once all are registered; unimplemented calls are counted on stderr (atl_services_types::counters).

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
extern crate binder;
// librustutils: the process's system properties (ro.opengles.version, which atld writes into the services' property area)
extern crate rustutils;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use atl_aidl::aidl::android::app::IActivityManager::BnActivityManager;
use atl_aidl::aidl::android::content::pm::IPackageManager::BnPackageManager;
use atl_services::activity::{ActivityManager, RootBack};
use atl_services::install;
use atl_services::package::PackageManager;
use atl_services::packages::{PackageRecord, Packages, Permissions};
use atl_services::process_control::ControlSocket;
use atl_services::ui_agents::UiAgents;
use atl_services_types::compositor_registry;
use atl_services_types::display_model::DisplayModel;
use atl_services_types::native_libs::SupportedAbi;
use atl_services_types::packages::{Library, PermissionDecl};
use binder::{BinderFeatures, ProcessState};
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// An install session the host holds files for (repeat, in uid order), by its id. Installed at start through the control socket (atl_services::install): the installer's parse is the package.
    #[arg(long = "session")]
    sessions: Vec<u32>,
    /// The first session's uid; the i-th gets uid + i. Default: the first uid above the installer's (atl_broker::INSTALLER_UID).
    #[arg(long, default_value_t = atl_broker::INSTALLER_UID + 1)]
    first_uid: u32,
    /// A device ABI with its instruction set (repeat, preferred first): `<abi>=<isa>` (ro.product.cpu.abilist with arches.toml row keys).
    #[arg(long = "abi", value_parser = parse_abi)]
    abis: Vec<SupportedAbi>,
    /// framework-res's permission definitions: a JSON array of `PermissionDecl` (the grant rule's platform half). Omitted: no platform permission is granted.
    #[arg(long)]
    platform_permissions: Option<PathBuf>,
    /// Test only: every package gets the native GL driver (the GL selection gate's break).
    #[arg(long)]
    no_gl_selection: bool,
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
    /// Test only: this service (`connectivity`, `permissionmgr`, `power`) is not registered (repeat), the deliberate break of the app gate's line for it (its manager is then null).
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
/// Context.POWER_SERVICE and Context.THERMAL_SERVICE: SystemServiceRegistry builds PowerManager from both (getServiceOrThrow each).
const POWER: &str = "power";
const THERMAL: &str = "thermalservice";

fn parse_abi(s: &str) -> Result<SupportedAbi, String> {
    let (abi, isa) = s.split_once('=').ok_or("expected <abi>=<isa>")?;
    Ok(SupportedAbi {
        abi: abi.into(),
        isa: isa.into(),
    })
}

fn read_json<T: serde::de::DeserializeOwned>(p: &std::path::Path) -> Result<T> {
    let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", p.display()))
}

/// `--root-back`'s values.
fn parse_root_back(s: &str) -> Result<RootBack, String> {
    match s {
        "minimize" => Ok(RootBack::Minimize),
        "close" => Ok(RootBack::Close),
        _ => Err(format!("{s}: expected minimize or close")),
    }
}

/// ActivityTaskManagerService's GL_ES_VERSION: `SystemProperties.getInt("ro.opengles.version", GL_ES_VERSION_UNDEFINED)` (:867); unset -> 0. A value that is not an int is atld's bug: fail the start.
fn gl_es_version() -> Result<i32> {
    let name = atl_property_service::GLES_VERSION;
    match rustutils::system_properties::read(name).with_context(|| format!("reading {name}"))? {
        Some(v) if !v.is_empty() => v
            .parse()
            .with_context(|| format!("{name}={v:?}: not an int")),
        _ => Ok(0),
    }
}

fn main() -> Result<()> {
    let a = Args::parse();
    let display = a.display.unwrap_or_default();
    let control = Arc::new(ControlSocket::from_stdin().context("the process control on stdin")?);
    // the package list: installed now through the host's file work (atl_services::install), before any service answers
    let config = install::Config {
        first_uid: a.first_uid,
        abis: a.abis.clone(),
        no_gl_selection: a.no_gl_selection,
    };
    let list: Vec<PackageRecord> = install::install(&control, &a.sessions, &config)?;
    let libraries: BTreeMap<String, Library> = a.shared_libraries.into_iter().collect();
    let packages = Arc::new(Packages::new(list, libraries));
    // the permission states every service checks callers against: framework-res's definitions first, then the packages' (permission::initial_states, then runtime grants)
    let platform_permissions: Vec<PermissionDecl> = match &a.platform_permissions {
        Some(p) => read_json(p)?,
        None => Vec::new(),
    };
    let permissions = Arc::new(Permissions::new(&packages, &platform_permissions));
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
            gl_es_version()?,
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
        // the host's default network through the platform layer, read once at start; unreadable = none, said so
        use atl_platform_host::NetworkSource;
        let network = match atl_platform_host::PlatformNetwork.default_network() {
            Ok(n) => n,
            Err(e) => {
                writeln!(std::io::stderr(), "connectivity: {e}; no default network")?;
                None
            }
        };
        match &network {
            Some(n) => writeln!(
                std::io::stderr(),
                "connectivity: default network {} {:?} {:?} metered={} dns={}",
                n.interface,
                n.kind,
                n.connectivity,
                n.metered,
                n.dns.len()
            )?,
            None => writeln!(std::io::stderr(), "connectivity: no default network")?,
        }
        binder::add_service(
            CONNECTIVITY,
            atl_services::connectivity::new_binder(Arc::clone(&permissions), network.as_ref()),
        )
        .context("addService connectivity")?;
    }
    if !a.omit_service.iter().any(|s| s == POWER) {
        binder::add_service(
            POWER,
            atl_services::power::new_binder(Arc::clone(&permissions)),
        )
        .context("addService power")?;
    }
    binder::add_service(THERMAL, atl_services::power::new_thermal_binder())
        .context("addService thermalservice")?;
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
