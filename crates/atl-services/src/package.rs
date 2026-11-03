// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/pm/ComputerEngine.java, frameworks/base/services/core/java/com/android/server/pm/PackageManagerService.java, frameworks/base/services/core/java/com/android/server/pm/IPackageManagerBase.java, frameworks/base/services/core/java/com/android/server/pm/parsing/PackageInfoUtils.java
//! IPackageManager, subset: getApplicationInfo, getPackageInfo, hasSystemFeature, getPackagesForUid, notifyDexLoad, getInstrumentationInfoAsUser, queryInstrumentationAsUser, checkSignatures.
//! Every other method is counted unimplemented (counters).
//! One user (0): a userId other than 0 is a user that does not exist. Flags (MATCH_*, GET_*) are not interpreted: the records carry no components or signatures, and `sharedLibraryFiles` is always filled.

use std::io::Write;
use std::sync::Arc;

use atl_aidl::aidl::android::content::pm::IPackageManager::{EnforcePermission, IPackageManager};
use atl_parcelables::android::content::ComponentName;
use atl_parcelables::android::content::pm::{
    ApplicationInfo, InstrumentationInfo, PackageInfo, ParceledListSlice,
};
use atl_parcelables::java::util::Map;
use atl_parcelables::parts::{PackageItemInfo, SharedLibraryInfo};
use binder::{ExceptionCode, Interface, Status, ThreadState};

use crate::packages::{InstrumentationDecl, LibraryNode, PackageRecord, Packages};
use crate::{counters, manifest_flags, permission};

/// The interface name counters use.
const INTERFACE: &str = "IPackageManager";
/// The only user (UserHandle.USER_SYSTEM).
const USER_SYSTEM: i32 = 0;
/// SharedLibraryInfo.TYPE_BUILTIN and VERSION_UNDEFINED (SharedLibraryInfo.java:60, :92).
const SHARED_LIBRARY_TYPE_BUILTIN: i32 = 0;
const SHARED_LIBRARY_VERSION_UNDEFINED: i64 = -1;
/// PackageManager.SIGNATURE_MATCH / SIGNATURE_UNKNOWN_PACKAGE (PackageManager.java:1480, :1510).
const SIGNATURE_MATCH: i32 = 0;
const SIGNATURE_UNKNOWN_PACKAGE: i32 = -4;

/// The directory of an installed package's native libraries below its install directory (PackageManagerService: `lib/<isa>` under /data/app/<package>); the ISA level is left out until the installer extracts libraries from the APK's lib/<abi>/.
const NATIVE_LIBRARY_SUBDIR: &str = "lib";

/// The ApplicationInfo of `p` (PackageInfoUtils.generateApplicationInfo, reduced to what the record holds): names, uid, paths, SDK levels, enabled, flags (manifest_flags), and the shared libraries its manifest
/// uses: `sharedLibraryFiles` (their jars, in manifest order; what ComputerEngine fills for GET_SHARED_LIBRARY_FILES, which ActivityManagerService's STOCK_PM_FLAGS always sets) and `sharedLibraryInfos`,
/// from which LoadedApk builds the libraries' class loaders (it takes only `.apk` entries of `sharedLibraryFiles`, ApplicationInfo's legacy field). A `<uses-library>` that names no installed
/// library adds nothing (a required one would stop the install in AOSP; the services do not install).
/// `nativeLibraryDir` is never null for an installed app: LoadedApk.makePaths adds it to the class loader's library path, and a null breaks `new File(null)` there.
pub fn application_info(p: &PackageRecord, packages: &Packages) -> ApplicationInfo {
    let native_library_dir = std::path::Path::new(&p.source_dir)
        .parent()
        .map(|dir| dir.join(NATIVE_LIBRARY_SUBDIR).display().to_string());
    let libraries = packages.used_libraries(p);
    let library_files: Vec<Option<String>> =
        libraries.iter().map(|l| Some(l.file.clone())).collect();
    ApplicationInfo {
        shared_library_infos: (!libraries.is_empty())
            .then(|| libraries.iter().map(|l| Some(library_info(l))).collect()),
        native_library_dir,
        item: PackageItemInfo {
            name: None,
            package_name: Some(p.name.clone()),
            ..Default::default()
        },
        process_name: Some(p.process_name().to_owned()),
        flags: manifest_flags::application_flags(p),
        source_dir: Some(p.source_dir.clone()),
        public_source_dir: Some(p.source_dir.clone()),
        shared_library_files: (!library_files.is_empty()).then_some(library_files),
        data_dir: Some(p.data_dir.clone()),
        credential_protected_data_dir: Some(p.data_dir.clone()),
        device_protected_data_dir: Some(p.device_protected_data_dir.clone()),
        uid: i32::try_from(p.uid).unwrap_or(i32::MAX),
        min_sdk_version: p.min_sdk,
        target_sdk_version: p.target_sdk,
        long_version_code: p.version_code,
        enabled: true,
        ..Default::default()
    }
}

/// The SharedLibraryInfo of a builtin library (SystemConfig `<library>`, as PackageManagerService registers it: a path, no package, VERSION_UNDEFINED, TYPE_BUILTIN), its dependencies below it.
/// The declaring package (the platform) is not modelled.
fn library_info(l: &LibraryNode) -> SharedLibraryInfo {
    SharedLibraryInfo {
        path: Some(l.file.clone()),
        name: Some(l.name.clone()),
        version: SHARED_LIBRARY_VERSION_UNDEFINED,
        kind: SHARED_LIBRARY_TYPE_BUILTIN,
        dependencies: (!l.dependencies.is_empty()).then(|| {
            l.dependencies
                .iter()
                .map(|d| Some(library_info(d)))
                .collect()
        }),
        ..Default::default()
    }
}

/// PackageInfoUtils.generateInstrumentationInfo (PackageInfoUtils.java), reduced to what the record holds: the component's names, its target, the package's paths, the two flags.
fn instrumentation_info(p: &PackageRecord, i: &InstrumentationDecl) -> InstrumentationInfo {
    let native_library_dir = std::path::Path::new(&p.source_dir)
        .parent()
        .map(|dir| dir.join(NATIVE_LIBRARY_SUBDIR).display().to_string());
    InstrumentationInfo {
        item: PackageItemInfo {
            name: Some(i.name.clone()),
            package_name: Some(p.name.clone()),
            ..Default::default()
        },
        target_package: Some(i.target_package.clone()),
        target_processes: i.target_processes.clone(),
        source_dir: Some(p.source_dir.clone()),
        public_source_dir: Some(p.source_dir.clone()),
        data_dir: Some(p.data_dir.clone()),
        device_protected_data_dir: Some(p.device_protected_data_dir.clone()),
        credential_protected_data_dir: Some(p.data_dir.clone()),
        native_library_dir,
        handle_profiling: i.handle_profiling,
        functional_test: i.functional_test,
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
        Ok(self
            .visible(name, user)
            .map(|p| application_info(p, &self.packages)))
    }

    /// ComputerEngine.getPackageInfo:1581: null as getApplicationInfo; the PackageInfo carries the ApplicationInfo, the version and the requested permissions.
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
            application_info: Some(application_info(p, &self.packages)),
            requested_permissions: (!p.requested_permissions.is_empty())
                .then(|| p.requested_permissions.iter().cloned().map(Some).collect()),
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

    /// ComputerEngine.getInstrumentationInfoAsUser:4944-4961: null for a user that does not exist, a package that does not exist or the caller may not see, or a class the package does not
    /// declare as an instrumentation.
    fn r#getInstrumentationInfoAsUser(
        &self,
        class_name: Option<&ComponentName>,
        _flags: i32,
        user: i32,
    ) -> binder::Result<Option<InstrumentationInfo>> {
        let Some(c) = class_name else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("getInstrumentationInfoAsUser: null component"),
            ));
        };
        let Some(p) = self.visible(Some(&c.package), user) else {
            return Ok(None);
        };
        Ok(p.instrumentation
            .iter()
            .find(|i| i.name == c.class)
            .map(|i| instrumentation_info(p, i)))
    }

    /// ComputerEngine.queryInstrumentationAsUser:4965-4999: the instrumentations targeting `targetPackage` (all when null) of the packages the caller may see; an empty list for a user that does not exist.
    fn r#queryInstrumentationAsUser(
        &self,
        target_package: Option<&str>,
        _flags: i32,
        user: i32,
    ) -> binder::Result<Option<ParceledListSlice>> {
        let caller = ThreadState::get_calling_uid();
        let instrumentation = if user != USER_SYSTEM {
            Vec::new()
        } else {
            self.packages
                .all_instrumentation()
                .filter(|(_, i)| target_package.is_none_or(|t| t == i.target_package))
                .filter(|(p, _)| self.packages.visible(caller, &p.name).is_some())
                .map(|(p, i)| instrumentation_info(p, i))
                .collect()
        };
        Ok(Some(ParceledListSlice::new(instrumentation)))
    }

    /// ComputerEngine.checkSignatures:4199-4217: SIGNATURE_UNKNOWN_PACKAGE when either package is not installed or the caller may not see it; otherwise checkSignaturesInternal compares the signing
    /// details. Nothing here reads or verifies APK signatures (no package has any), so two known packages always compare as SIGNATURE_MATCH.
    fn r#checkSignatures(
        &self,
        pkg1: Option<&str>,
        pkg2: Option<&str>,
        user: i32,
    ) -> binder::Result<i32> {
        let known = |n| self.visible(n, user).is_some();
        Ok(if known(pkg1) && known(pkg2) {
            SIGNATURE_MATCH
        } else {
            SIGNATURE_UNKNOWN_PACKAGE
        })
    }
}
