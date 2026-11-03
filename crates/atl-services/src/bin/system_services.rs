// SPDX-License-Identifier: MIT
//! atl-system-services: registers the M2 services with servicemanager (`activity`, `package`, `display`, as Context.ACTIVITY_SERVICE / PACKAGE_SERVICE / DISPLAY_SERVICE name them) and serves them on
//! libbinder's thread pool, until killed. Runs as the system uid: servicemanager accepts addService from it. The packages come from a record file (atld's install step writes it), the shared
//! libraries from arguments. Processes are started and killed through the control connection atld passes as stdin ([`atl_services::process_control`]).
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
use atl_services::package::PackageManager;
use atl_services::packages::{Library, PackageRecord, Packages};
use atl_services::process_control::ControlSocket;
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
}

/// The names the framework looks the services up by (Context.ACTIVITY_SERVICE, Context.PACKAGE_SERVICE / ActivityThread.getPackageManager).
const ACTIVITY: &str = "activity";
const PACKAGE: &str = "package";
/// Context.DISPLAY_SERVICE.
const DISPLAY: &str = "display";

fn main() -> Result<()> {
    let a = Args::parse();
    let text = std::fs::read_to_string(&a.packages)
        .with_context(|| format!("reading {}", a.packages.display()))?;
    let list: Vec<PackageRecord> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", a.packages.display()))?;
    let libraries: BTreeMap<String, Library> = a.shared_libraries.into_iter().collect();
    let packages = Arc::new(Packages::new(list, libraries));
    let control = Arc::new(ControlSocket::from_stdin().context("the process control on stdin")?);
    ProcessState::start_thread_pool();
    let activity = ActivityManager::new(packages.clone(), control);
    if let Some(package) = &a.launch
        && packages.by_name(package).is_none()
    {
        bail!("--launch {package}: not an installed package");
    }
    let am = BnActivityManager::new_binder(activity.clone(), BinderFeatures::default());
    binder::add_service(ACTIVITY, am.as_binder()).context("addService activity")?;
    let pm = BnPackageManager::new_binder(PackageManager::new(packages), BinderFeatures::default());
    binder::add_service(PACKAGE, pm.as_binder()).context("addService package")?;
    binder::add_service(DISPLAY, atl_services::display::new_binder())
        .context("addService display")?;
    writeln!(std::io::stderr(), "services: ready")?;
    if let Some(package) = &a.launch {
        activity
            .start_launcher(package)
            .map_err(anyhow::Error::msg)?;
    }
    ProcessState::join_thread_pool();
    Ok(())
}
