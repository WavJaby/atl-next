// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/pm/ComputerEngine.java, frameworks/base/services/core/java/com/android/server/pm/PackageManagerService.java, frameworks/base/services/core/java/com/android/server/pm/IPackageManagerBase.java
//! IPackageManager, M2 subset: getApplicationInfo, getPackageInfo, hasSystemFeature, getPackagesForUid, notifyDexLoad. Every other method is counted unimplemented (counters).
//! One user (0): a userId other than 0 is a user that does not exist. Flags (MATCH_*, GET_*) are not interpreted: the records carry no components, metadata or signatures yet.

use std::io::Write;
use std::sync::Arc;

use atl_aidl::aidl::android::content::pm::IPackageManager::{EnforcePermission, IPackageManager};
use atl_parcelables::android::content::pm::{ApplicationInfo, PackageInfo};
use atl_parcelables::java::util::Map;
use atl_parcelables::parts::PackageItemInfo;
use binder::{ExceptionCode, Interface, Status, ThreadState};

use crate::packages::{PackageRecord, Packages};
use crate::{counters, permission};

/// The interface name counters use.
const INTERFACE: &str = "IPackageManager";
/// The only user (UserHandle.USER_SYSTEM).
const USER_SYSTEM: i32 = 0;
/// ApplicationInfo.FLAG_HAS_CODE (content/pm/ApplicationInfo.java): every APK ATL runs has code.
const FLAG_HAS_CODE: i32 = 1 << 2;

/// The ApplicationInfo of `p` (PackageInfoUtils.generateApplicationInfo, reduced to what the record holds): names, uid, paths, SDK levels, enabled.
pub fn application_info(p: &PackageRecord) -> ApplicationInfo {
    ApplicationInfo {
        item: PackageItemInfo {
            name: None,
            package_name: Some(p.name.clone()),
            ..Default::default()
        },
        process_name: Some(p.process_name().to_owned()),
        flags: FLAG_HAS_CODE,
        source_dir: Some(p.source_dir.clone()),
        public_source_dir: Some(p.source_dir.clone()),
        data_dir: Some(p.data_dir.clone()),
        credential_protected_data_dir: Some(p.data_dir.clone()),
        uid: i32::try_from(p.uid).unwrap_or(i32::MAX),
        min_sdk_version: p.min_sdk,
        target_sdk_version: p.target_sdk,
        long_version_code: p.version_code,
        enabled: true,
        ..Default::default()
    }
}

pub struct PackageManager {
    packages: Arc<Packages>,
}

impl PackageManager {
    pub fn new(packages: Arc<Packages>) -> Self {
        PackageManager { packages }
    }

    /// The record `caller` may see under `name` in `user` (ComputerEngine.getApplicationInfoInternal:997 `!mUserManager.exists(userId) return null`; visibility per Packages::visible).
    fn visible(&self, name: Option<&str>, user: i32) -> Option<&PackageRecord> {
        if user != USER_SYSTEM {
            return None;
        }
        self.packages.visible(ThreadState::get_calling_uid(), name?)
    }
}

impl Interface for PackageManager {}

impl IPackageManager for PackageManager {
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
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// ComputerEngine.getApplicationInfo:986: null for a package the caller may not see or that does not exist.
    fn r#getApplicationInfo(
        &self,
        name: Option<&str>,
        _flags: i64,
        user: i32,
    ) -> binder::Result<Option<ApplicationInfo>> {
        Ok(self.visible(name, user).map(application_info))
    }

    /// ComputerEngine.getPackageInfo:1581: null as getApplicationInfo; the PackageInfo carries the ApplicationInfo and version.
    fn r#getPackageInfo(
        &self,
        name: Option<&str>,
        _flags: i64,
        user: i32,
    ) -> binder::Result<Option<PackageInfo>> {
        Ok(self.visible(name, user).map(|p| PackageInfo {
            package_name: Some(p.name.clone()),
            // PackageInfo.versionCode: the low 32 bits of the long version code, versionCodeMajor the high (PackageInfo.setLongVersionCode)
            version_code: p.version_code as i32,
            version_code_major: (p.version_code >> 32) as i32,
            version_name: p.version_name.clone(),
            application_info: Some(application_info(p)),
            ..Default::default()
        }))
    }

    /// PackageManagerService.hasSystemFeature:2954: true when mAvailableFeatures holds `name` at >= `version`. M2: the set is empty, so false for every feature;
    /// features.toml (plan: feature groups by table, each row's hasSystemFeature flags) becomes the source when the first feature group is needed.
    fn r#hasSystemFeature(&self, _name: Option<&str>, _version: i32) -> binder::Result<bool> {
        Ok(false)
    }

    /// ComputerEngine.getPackagesForUid:1987: the packages of the uid's app id; null when none, or when the caller may not see them (an app sees its own uid only).
    fn r#getPackagesForUid(&self, uid: i32) -> binder::Result<Option<Vec<Option<String>>>> {
        let Ok(uid) = u32::try_from(uid) else {
            return Ok(None);
        };
        let caller = ThreadState::get_calling_uid();
        if !permission::is_system_or_root(caller)
            && permission::app_id(caller) != permission::app_id(uid)
        {
            return Ok(None);
        }
        let names: Vec<Option<String>> = self
            .packages
            .by_app_id(permission::app_id(uid))
            .map(|p| Some(p.name.clone()))
            .collect();
        Ok((!names.is_empty()).then_some(names))
    }

    /// PackageManagerService.notifyDexLoad:5611-5628: the system or the loading package's own uid may report; others are ignored (logged in AOSP). The report then goes to ART Service's
    /// dexopt bookkeeping, which ATL does not have: an accepted report is dropped.
    fn r#notifyDexLoad(
        &self,
        package: Option<&str>,
        _contexts: Option<&Map>,
        _isa: Option<&str>,
    ) -> binder::Result<()> {
        let caller = ThreadState::get_calling_uid();
        let own = package
            .and_then(|n| self.packages.by_name(n))
            .is_some_and(|p| permission::app_id(p.uid) == permission::app_id(caller));
        if !(permission::is_system_or_root(caller) || own) {
            let _ = writeln!(
                std::io::stderr(),
                "services: invalid dex load report: package {package:?}, uid {caller}"
            );
        }
        Ok(())
    }
}
