// SPDX-License-Identifier: MIT
//! atl-system-services: registers the M2 services with servicemanager (`activity`, `package`, `display`, as Context.ACTIVITY_SERVICE / PACKAGE_SERVICE / DISPLAY_SERVICE name them) and serves them on
//! libbinder's thread pool, until killed. Runs as the system uid: servicemanager accepts addService from it. One package and one expected start, from the arguments (the broker gives them later).
//! Prints `services: ready` once all are registered; unimplemented calls are counted on stderr (atl_services::counters).

// the converter's rlib, on the crate search path build.rs sets (no cargo dependency names it)
extern crate binder;

use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use atl_aidl::aidl::android::app::IActivityManager::BnActivityManager;
use atl_aidl::aidl::android::content::pm::IPackageManager::BnPackageManager;
use atl_services::activity::ActivityManager;
use atl_services::package::PackageManager;
use atl_services::packages::{PackageRecord, Packages, PendingStart};
use binder::{BinderFeatures, ProcessState};
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// The installed package's name.
    #[arg(long)]
    package: String,
    /// Its uid.
    #[arg(long)]
    uid: u32,
    /// Its base.apk path.
    #[arg(long)]
    apk: String,
    /// Its data directory (credential protected storage).
    #[arg(long)]
    data: String,
    /// Its device protected data directory.
    #[arg(long)]
    device_data: String,
    /// Its launcher activity's class name.
    #[arg(long)]
    activity: String,
    /// The startSeq of the expected start of this package as its uid.
    #[arg(long)]
    start_seq: i64,
    /// Its manifest's versionCode, minSdkVersion and targetSdkVersion (the caller reads them from the APK).
    #[arg(long)]
    version_code: i64,
    #[arg(long)]
    min_sdk: i32,
    #[arg(long)]
    target_sdk: i32,
    /// For a client that is a Rust test double, not a Java app: bindApplication sends no shared memory descriptor (ActivityManager::without_app_shared_memory).
    #[arg(long)]
    no_app_shared_memory: bool,
}

/// The names the framework looks the services up by (Context.ACTIVITY_SERVICE, Context.PACKAGE_SERVICE / ActivityThread.getPackageManager).
const ACTIVITY: &str = "activity";
const PACKAGE: &str = "package";
/// Context.DISPLAY_SERVICE.
const DISPLAY: &str = "display";

fn main() -> Result<()> {
    let a = Args::parse();
    let packages = Arc::new(Packages::new(vec![PackageRecord {
        name: a.package.clone(),
        uid: a.uid,
        source_dir: a.apk,
        data_dir: a.data,
        device_protected_data_dir: a.device_data,
        version_code: a.version_code,
        version_name: None,
        min_sdk: a.min_sdk,
        target_sdk: a.target_sdk,
        launch_activity: a.activity,
    }]));
    let pending = vec![PendingStart {
        start_seq: a.start_seq,
        package: a.package,
        uid: a.uid,
    }];
    ProcessState::start_thread_pool();
    let mut activity_manager = ActivityManager::new(packages.clone(), pending);
    if a.no_app_shared_memory {
        activity_manager = activity_manager.without_app_shared_memory();
    }
    let am = BnActivityManager::new_binder(activity_manager, BinderFeatures::default());
    binder::add_service(ACTIVITY, am.as_binder()).context("addService activity")?;
    let pm = BnPackageManager::new_binder(PackageManager::new(packages), BinderFeatures::default());
    binder::add_service(PACKAGE, pm.as_binder()).context("addService package")?;
    binder::add_service(DISPLAY, atl_services::display::new_binder())
        .context("addService display")?;
    writeln!(std::io::stderr(), "services: ready")?;
    ProcessState::join_thread_pool();
    Ok(())
}
