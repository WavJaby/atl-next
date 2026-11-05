// SPDX-License-Identifier: MIT
//! atl-system-services: registers the M2 services and the window service with servicemanager (`activity`, `package`, `display`, `window`, as Context.ACTIVITY_SERVICE / PACKAGE_SERVICE /
//! DISPLAY_SERVICE / WINDOW_SERVICE / INPUT_METHOD_SERVICE name them) and the compositor registry (compositor_registry.rs: how an app's in-process SurfaceFlinger gets registered) and serves them on libbinder's thread pool, until killed. Runs as the system uid: servicemanager accepts addService from it. The packages come
//! from a record file (atld's install step writes it), the shared libraries and the display from arguments (`--display`: display_model.rs; omitted, the model's default). Processes are started
//! and killed through the control connection atld passes as stdin ([`atl_services::process_control`]).
//! Prints `services: ready` once all are registered; unimplemented calls are counted on stderr (atl_services::counters).

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
extern crate binder;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use atl_aidl::aidl::android::app::IActivityManager::BnActivityManager;
use atl_aidl::aidl::android::content::pm::IPackageManager::BnPackageManager;
use atl_services::activity::ActivityManager;
use atl_services::compositor_registry;
use atl_services::display_model::DisplayModel;
use atl_services::package::PackageManager;
use atl_services::packages::{Library, PackageRecord, Packages};
use atl_services::process_control::ControlSocket;
use atl_services::ui_agent::UiAgents;
use binder::{BinderFeatures, ProcessState};
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// The installed packages: a JSON array of `PackageRecord`s (atl_services::packages), paths as the app processes see them.
    #[arg(long)]
    packages: PathBuf,
    /// A shared library an app may `<uses-library>`: `<name>=<jar path as the app processes see it>[,<dependency name>...]` (repeat). SystemConfig's `<library name file dependency>`.
    #[arg(long = "shared-library", value_parser = Library::parse_arg)]
    shared_libraries: Vec<(String, Library)>,
    /// A package whose launcher activity starts once the services are up (the stand-in for a home screen tap; startActivity is not implemented).
    #[arg(long)]
    launch: Option<String>,
    /// The display the services answer with (`WxH@hz,dpi`, display_model.rs). Omitted: the model's default.
    #[arg(long, value_parser = DisplayModel::parse_arg)]
    display: Option<DisplayModel>,
    /// AOSP's keychars/Virtual.kcm: loaded through libinput at start; its Parcel form goes with every InputDevice.
    #[arg(long)]
    keychars: PathBuf,
}

/// The names the framework looks the services up by (Context.ACTIVITY_SERVICE, Context.PACKAGE_SERVICE / ActivityThread.getPackageManager).
const ACTIVITY: &str = "activity";
const PACKAGE: &str = "package";
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

fn main() -> Result<()> {
    let a = Args::parse();
    let display = a.display.unwrap_or_default();
    let text = std::fs::read_to_string(&a.packages)
        .with_context(|| format!("reading {}", a.packages.display()))?;
    let list: Vec<PackageRecord> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", a.packages.display()))?;
    let libraries: BTreeMap<String, Library> = a.shared_libraries.into_iter().collect();
    let packages = Arc::new(Packages::new(list, libraries));
    let control = Arc::new(ControlSocket::from_stdin().context("the process control on stdin")?);
    ProcessState::start_thread_pool();
    // the UI agents the registry takes in REGISTER, read by the window service at each window's first relayout
    let agents = Arc::new(UiAgents::default());
    // the Tasks of the display: each app's window size (tasks.rs), read by the window service and the registry; the activity service creates each launch's Task here
    let tasks = Arc::new(atl_services::tasks::Tasks::new(display));
    let activity = ActivityManager::new(packages.clone(), control, display, Arc::clone(&tasks));
    if let Some(package) = &a.launch
        && packages.by_name(package).is_none()
    {
        bail!("--launch {package}: not an installed package");
    }
    let am = BnActivityManager::new_binder(activity.clone(), BinderFeatures::default());
    binder::add_service(ACTIVITY, am.as_binder()).context("addService activity")?;
    let pm = BnPackageManager::new_binder(PackageManager::new(packages), BinderFeatures::default());
    binder::add_service(PACKAGE, pm.as_binder()).context("addService package")?;
    binder::add_service(DISPLAY, atl_services::display::new_binder(display))
        .context("addService display")?;
    binder::add_service(
        WINDOW,
        atl_services::window::new_binder(display, Arc::clone(&agents), Arc::clone(&tasks)),
    )
    .context("addService window")?;
    binder::add_service(INPUT_METHOD, atl_services::input_method::new_binder())
        .context("addService input_method")?;
    binder::add_service(CONTENT, atl_services::content::new_binder())
        .context("addService content")?;
    binder::add_service(AUDIO, atl_services::audio::new_binder()).context("addService audio")?;
    binder::add_service(USER, atl_services::user::new_binder()).context("addService user")?;
    let contents =
        std::fs::read(&a.keychars).with_context(|| format!("reading {}", a.keychars.display()))?;
    let name = a
        .keychars
        .file_name()
        .and_then(|n| n.to_str())
        .context("the key map's file name")?;
    let map = atl_libinput::KeyCharacterMap::load(name, &contents)
        .with_context(|| format!("loading {}", a.keychars.display()))?;
    let input = atl_services::input::new_binder(&map.parcel_bytes()).map_err(anyhow::Error::msg)?;
    binder::add_service(INPUT, input).context("addService input")?;
    binder::add_service(
        compositor_registry::SERVICE,
        atl_services::registry_service::new_binder(
            Arc::clone(&agents),
            Arc::clone(&tasks),
            activity.clone(),
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
