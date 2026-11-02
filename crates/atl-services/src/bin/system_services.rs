// SPDX-License-Identifier: MIT
//! atl-system-services: registers the M2 services with servicemanager (`activity`, `package`, as Context.ACTIVITY_SERVICE / PACKAGE_SERVICE name them) and serves them on
//! libbinder's thread pool, until killed. Runs as the system uid: servicemanager accepts addService from it. One package and one expected start, from the arguments (the broker gives them later).
//! Prints `services: ready` once both are registered; unimplemented calls are counted on stderr (atl_services::counters).

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
    /// Its data directory.
    #[arg(long)]
    data: String,
    /// Its launcher activity's class name.
    #[arg(long)]
    activity: String,
    /// The startSeq of the expected start of this package as its uid.
    #[arg(long)]
    start_seq: i64,
}

/// The names the framework looks the services up by (Context.ACTIVITY_SERVICE, Context.PACKAGE_SERVICE / ActivityThread.getPackageManager).
const ACTIVITY: &str = "activity";
const PACKAGE: &str = "package";
/// The package's SDK levels and version until the APK parser gives them: the API level M2 targets (34) for both, version 1.
const SDK: i32 = 34;
const VERSION: i64 = 1;

fn main() -> Result<()> {
    let a = Args::parse();
    let packages = Arc::new(Packages::new(vec![PackageRecord {
        name: a.package.clone(),
        uid: a.uid,
        source_dir: a.apk,
        data_dir: a.data,
        version_code: VERSION,
        version_name: None,
        min_sdk: SDK,
        target_sdk: SDK,
        launch_activity: a.activity,
    }]));
    let pending = vec![PendingStart {
        start_seq: a.start_seq,
        package: a.package,
        uid: a.uid,
    }];
    ProcessState::start_thread_pool();
    let am = BnActivityManager::new_binder(
        ActivityManager::new(packages.clone(), pending),
        BinderFeatures::default(),
    );
    binder::add_service(ACTIVITY, am.as_binder()).context("addService activity")?;
    let pm = BnPackageManager::new_binder(PackageManager::new(packages), BinderFeatures::default());
    binder::add_service(PACKAGE, pm.as_binder()).context("addService package")?;
    writeln!(std::io::stderr(), "services: ready")?;
    ProcessState::join_thread_pool();
    Ok(())
}
