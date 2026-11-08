// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/pm/ComputerEngine.java, frameworks/base/services/core/java/com/android/server/pm/PackageManagerService.java, frameworks/base/services/core/java/com/android/server/pm/IPackageManagerBase.java, frameworks/base/services/core/java/com/android/server/pm/parsing/PackageInfoUtils.java
//! IPackageManager, subset: getApplicationInfo, getPackageInfo, getActivityInfo, getProviderInfo, resolveContentProvider, hasSystemFeature, getPackagesForUid, notifyDexLoad, getInstrumentationInfoAsUser, queryInstrumentationAsUser, checkSignatures.
//! Every other method is counted unimplemented (counters).
//! One user (0): a userId other than 0 is a user that does not exist. Flags (MATCH_*, GET_*) are not interpreted except GET_META_DATA (application, activity and provider meta-data) and MATCH_DISABLED_COMPONENTS for providers: the records carry no signatures and of the components only activities and providers, and `sharedLibraryFiles` is always filled.

use std::io::Write;
use std::sync::Arc;

use atl_aidl::aidl::android::content::pm::IPackageManager::{EnforcePermission, IPackageManager};
use atl_parcelables::android::content::ComponentName;
use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, InstrumentationInfo, PackageInfo, ParceledListSlice,
    ProviderInfo,
};
use atl_parcelables::android::os::Bundle;
use atl_parcelables::codecs::android::os::Bundle::Value;
use atl_parcelables::java::util::Map;
use atl_parcelables::parts::{ComponentInfo, PackageItemInfo, SharedLibraryInfo};
use binder::{ExceptionCode, Interface, Status, ThreadState};

use crate::packages::{
    ActivityDecl, InstrumentationDecl, LibraryNode, MetaValue, PackageRecord, Packages,
    ProviderDecl,
};
use crate::permission::{self, Permissions};
use crate::{counters, manifest_flags};

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
/// PackageManager.GET_META_DATA (PackageManager.java).
const GET_META_DATA: i64 = 0x80;
/// PackageManager.MATCH_DISABLED_COMPONENTS (PackageManager.java).
const MATCH_DISABLED_COMPONENTS: i64 = 0x200;
/// ActivityManagerService.STOCK_PM_FLAGS: GET_SHARED_LIBRARY_FILES, the flags AMS reads an app's ApplicationInfo with (bindApplication's, the launch's).
pub const STOCK_PM_FLAGS: i64 = 0x400;

/// Property.toBundle per entry, in manifest order (a later put of a name replaces the earlier).
fn meta_bundle(entries: &[(String, MetaValue)]) -> Bundle {
    Bundle::from_entries(entries.iter().map(|(k, v)| {
        let v = match v {
            MetaValue::String(s) => Value::String(Some(s.clone())),
            MetaValue::Int(i) => Value::Int(*i),
            MetaValue::Bool(b) => Value::Bool(*b),
            MetaValue::Float(f) => Value::Float(*f),
        };
        (k.clone(), v)
    }))
}

/// The ApplicationInfo of `p` (PackageInfoUtils.generateApplicationInfo, reduced to what the record holds): names, uid, paths, SDK levels, enabled, flags (manifest_flags), and the shared libraries its manifest
/// uses: `sharedLibraryFiles` (their jars, in manifest order; what ComputerEngine fills for GET_SHARED_LIBRARY_FILES, which ActivityManagerService's STOCK_PM_FLAGS always sets) and `sharedLibraryInfos`,
/// from which LoadedApk builds the libraries' class loaders (it takes only `.apk` entries of `sharedLibraryFiles`, ApplicationInfo's legacy field). A `<uses-library>` that names no installed
/// library adds nothing (a required one would stop the install in AOSP; the services do not install).
/// `nativeLibraryDir` is never null for an installed app: LoadedApk.makePaths adds it to the class loader's library path, and a null breaks `new File(null)` there.
/// Native-library fields as PackageManagerService sets them for a non-system app (PackageAbiHelperImpl.deriveNativeLibraryPaths): root `<code path>/lib`, dir `<root>/<isa>`, root requires the ISA.
/// `flags` (PackageManager GET_*): GET_META_DATA adds `<application>`'s meta-data, null when it has none (PackageInfoUtils:659, "coerce to null if empty"); the other flags are not interpreted.
pub fn application_info(p: &PackageRecord, packages: &Packages, flags: i64) -> ApplicationInfo {
    let native_library_root_dir = std::path::Path::new(&p.native_library_dir)
        .parent()
        .map(|dir| dir.display().to_string());
    let libraries = packages.used_libraries(p);
    let library_files: Vec<Option<String>> =
        libraries.iter().map(|l| Some(l.file.clone())).collect();
    ApplicationInfo {
        shared_library_infos: (!libraries.is_empty())
            .then(|| libraries.iter().map(|l| Some(library_info(l))).collect()),
        native_library_dir: Some(p.native_library_dir.clone()),
        native_library_root_dir,
        native_library_root_requires_isa: true,
        primary_cpu_abi: p.primary_cpu_abi.clone(),
        // PackageImpl.toAppInfoWithoutState: name = className = <application android:name>; appComponentFactory as written
        class_name: p.manifest.application_class.clone(),
        app_component_factory: p.manifest.app_component_factory.clone(),
        item: PackageItemInfo {
            name: p.manifest.application_class.clone(),
            package_name: Some(p.name.clone()),
            meta_data: (flags & GET_META_DATA != 0 && !p.manifest.application_meta_data.is_empty())
                .then(|| meta_bundle(&p.manifest.application_meta_data)),
            ..Default::default()
        },
        process_name: Some(p.process_name().to_owned()),
        theme: p.application_theme,
        flags: manifest_flags::application_flags(p),
        private_flags_ext: manifest_flags::application_private_flags_ext(
            p,
            atl_parcelables::aconfig::PREDICTIVE_BACK_DEFAULT_ENABLE_SDK_36,
        ),
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
    let native_library_dir = Some(p.native_library_dir.clone());
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

/// PackageInfoUtils.generateActivityInfo, reduced to what the record holds: the component's names, its application (`ai`, already built for the caller's flags), process, theme, flags, configChanges;
/// GET_META_DATA adds the `<activity>`'s own meta-data, null when it has none (as the application's). Both the launch (LaunchActivityItem.info) and getActivityInfo answer with it.
pub fn activity_info(
    p: &PackageRecord,
    activity: &ActivityDecl,
    ai: ApplicationInfo,
    flags: i64,
) -> ActivityInfo {
    ActivityInfo {
        component: ComponentInfo {
            item: PackageItemInfo {
                name: Some(activity.name.clone()),
                package_name: Some(p.name.clone()),
                meta_data: (flags & GET_META_DATA != 0 && !activity.meta_data.is_empty())
                    .then(|| meta_bundle(&activity.meta_data)),
                ..Default::default()
            },
            application_info: ai,
            process_name: Some(p.process_name().to_owned()),
            enabled: true,
            exported: true,
            ..Default::default()
        },
        theme: activity.theme,
        flags: manifest_flags::activity_flags(p, activity),
        config_changes: activity.config_changes,
        ..Default::default()
    }
}

/// PackageInfoUtils.generateProviderInfo, reduced to what the declaration holds; GET_META_DATA adds the `<provider>`'s own meta-data, null when it has none.
fn provider_info(
    p: &PackageRecord,
    d: &ProviderDecl,
    packages: &Packages,
    flags: i64,
) -> ProviderInfo {
    ProviderInfo {
        component: ComponentInfo {
            item: PackageItemInfo {
                name: Some(d.name.clone()),
                package_name: Some(p.name.clone()),
                meta_data: (flags & GET_META_DATA != 0 && !d.meta_data.is_empty())
                    .then(|| meta_bundle(&d.meta_data)),
                ..Default::default()
            },
            application_info: application_info(p, packages, flags),
            process_name: Some(provider_process(p, d).to_owned()),
            enabled: d.enabled,
            exported: d.exported,
            direct_boot_aware: d.direct_boot_aware,
            ..Default::default()
        },
        authority: d.authority.clone(),
        read_permission: d.read_permission.clone(),
        write_permission: d.write_permission.clone(),
        grant_uri_permissions: d.grant_uri_permissions,
        force_uri_permissions: d.force_uri_permissions,
        multiprocess: d.multiprocess,
        init_order: d.init_order,
        is_syncable: d.syncable,
        ..Default::default()
    }
}

/// ParsedMainComponent.getProcessName: the component's process, else the package's.
fn provider_process<'a>(p: &'a PackageRecord, d: &'a ProviderDecl) -> &'a str {
    d.process.as_deref().unwrap_or_else(|| p.process_name())
}

/// PackageStateUtils.isEnabledAndMatches, reduced to the component's own state.
fn provider_matches(d: &ProviderDecl, flags: i64) -> bool {
    d.enabled || flags & MATCH_DISABLED_COMPONENTS != 0
}

/// ComputerEngine.queryContentProviders for one package (ContentProviderHelper.generateApplicationProvidersLocked's call): its enabled providers with an authority in `process`, higher initOrder first
/// (sProviderInitOrderSorter). Equal orders stay in manifest order; AOSP leaves them in its map's order, which no app can rely on.
pub fn process_providers(
    p: &PackageRecord,
    packages: &Packages,
    process: &str,
    flags: i64,
) -> Vec<ProviderInfo> {
    let mut list: Vec<&ProviderDecl> = p
        .manifest
        .providers
        .iter()
        .filter(|d| {
            d.authority.is_some() && provider_process(p, d) == process && provider_matches(d, flags)
        })
        .collect();
    list.sort_by_key(|d| std::cmp::Reverse(d.init_order));
    list.into_iter()
        .map(|d| provider_info(p, d, packages, flags))
        .collect()
}

/// getActivityInfo after the visibility check:`class` of `p` with its ApplicationInfo built for the same `flags`; None for a class `p` does not declare.
fn find_activity_info(
    p: &PackageRecord,
    packages: &Packages,
    class: &str,
    flags: i64,
) -> Option<ActivityInfo> {
    p.manifest
        .activities
        .iter()
        .find(|a| a.name == class)
        .map(|a| activity_info(p, a, application_info(p, packages, flags), flags))
}

pub struct PackageManager {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
}

impl PackageManager {
    pub fn new(packages: Arc<Packages>, permissions: Arc<Permissions>) -> Self {
        PackageManager {
            packages,
            permissions,
        }
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
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
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
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ApplicationInfo>> {
        Ok(self
            .visible(name, user)
            .map(|p| application_info(p, &self.packages, flags)))
    }

    /// ComputerEngine.getActivityInfoInternal: null for an unknown user, package or class (ApplicationPackageManager then throws NameNotFoundException); the ApplicationInfo inside is built with the same flags.
    fn r#getActivityInfo(
        &self,
        component: Option<&ComponentName>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ActivityInfo>> {
        let Some(c) = component else {
            return Ok(None);
        };
        Ok(self
            .visible(Some(&c.package), user)
            .and_then(|p| find_activity_info(p, &self.packages, &c.class, flags)))
    }

    /// ComputerEngine.getProviderInfo:4119: null for an unknown user, package or class, and for a disabled provider without MATCH_DISABLED_COMPONENTS.
    fn r#getProviderInfo(
        &self,
        component: Option<&ComponentName>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ProviderInfo>> {
        let Some(c) = component else {
            return Ok(None);
        };
        Ok(self.visible(Some(&c.package), user).and_then(|p| {
            p.manifest
                .providers
                .iter()
                .find(|d| d.name == c.class && provider_matches(d, flags))
                .map(|d| provider_info(p, d, &self.packages, flags))
        }))
    }

    /// ComputerEngine.resolveContentProvider:4787: the visible provider one of whose `;`-separated authorities is `name`. Not modelled: the `user@authority` form (one user).
    fn r#resolveContentProvider(
        &self,
        name: Option<&str>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ProviderInfo>> {
        let Some(name) = name else {
            return Ok(None);
        };
        Ok(self.packages.all().iter().find_map(|record| {
            let p = self.visible(Some(&record.name), user)?;
            let d = p.manifest.providers.iter().find(|d| {
                provider_matches(d, flags)
                    && d.authority
                        .as_deref()
                        .is_some_and(|a| a.split(';').any(|one| one == name))
            })?;
            Some(provider_info(p, d, &self.packages, flags))
        }))
    }

    /// ComputerEngine.getPackageInfo:1581: null as getApplicationInfo; the PackageInfo carries the ApplicationInfo, the version and the requested permissions.
    fn r#getPackageInfo(
        &self,
        name: Option<&str>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<PackageInfo>> {
        Ok(self.visible(name, user).map(|p| PackageInfo {
            package_name: Some(p.name.clone()),
            // PackageInfo.versionCode: the low 32 bits of the long version code, versionCodeMajor the high (PackageInfo.setLongVersionCode)
            version_code: p.version_code as i32,
            version_code_major: (p.version_code >> 32) as i32,
            version_name: p.version_name.clone(),
            application_info: Some(application_info(p, &self.packages, flags)),
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::packages::Manifest;

    fn activity(name: &str, meta_data: Vec<(String, MetaValue)>) -> ActivityDecl {
        ActivityDecl {
            name: name.into(),
            launcher: false,
            theme: 0,
            config_changes: 0,
            booleans: BTreeMap::new(),
            launch_mode: 0,
            task_affinity: None,
            process: None,
            meta_data,
            screen_orientation: crate::packages::SCREEN_ORIENTATION_UNSPECIFIED,
            resizeable: true,
            min_aspect_ratio: None,
            max_aspect_ratio: None,
        }
    }

    /// One package: `<application>` meta-data `app.key`, activity `a.Native` with NativeActivity-style meta-data of every MetaValue type, activity `a.Bare` with none.
    fn record() -> PackageRecord {
        let native_meta = vec![
            (
                "android.app.lib_name".into(),
                MetaValue::String("native-lib".into()),
            ),
            ("flag".into(), MetaValue::Bool(true)),
            ("res".into(), MetaValue::Int(0x7f01_0001)),
            ("scale".into(), MetaValue::Float(1.5)),
        ];
        PackageRecord {
            manifest: Manifest {
                name: "a".into(),
                version_code: 1,
                version_name: None,
                min_sdk: 21,
                target_sdk: 34,
                process_name: None,
                application_theme: 0,
                application_class: None,
                app_component_factory: None,
                application_meta_data: vec![("app.key".into(), MetaValue::String("v".into()))],
                application_booleans: BTreeMap::new(),
                activities: vec![
                    activity("a.Native", native_meta),
                    activity("a.Bare", vec![]),
                ],
                requested_permissions: vec![],
                permissions: vec![],
                libraries: vec![],
                instrumentation: vec![],
                receivers: vec![],
                providers: vec![],
            },
            uid: 10_001,
            source_dir: String::new(),
            primary_cpu_abi: None,
            native_library_dir: String::new(),
            data_dir: String::new(),
            device_protected_data_dir: String::new(),
            granted_permissions: vec![],
        }
    }

    /// The bundle's entries by key: a Bundle is an ArrayMap, iterated in key-hash order, not insertion order.
    fn entries(b: &Option<Bundle>) -> Option<Vec<(String, Value)>> {
        b.as_ref().map(|b| {
            let mut e = b.entries().expect("bundle decodes");
            e.sort_by(|x, y| x.0.cmp(&y.0));
            e
        })
    }

    /// PackageInfoUtils.generateActivityInfo:590-595: GET_META_DATA -> the `<activity>`'s meta-data, an empty one coerced to null; without the flag null. The ApplicationInfo inside follows the same
    /// flags (:381 nulls application meta-data without GET_META_DATA). Values typed as ParsingPackageUtils.parseMetaData does (string, boolean, int/resource id, float).
    /// The visibility / user check before it needs a binder calling uid (`/dev/binder`), so the test starts after it.
    #[test]
    fn get_activity_info_meta_data() {
        let p = record();
        let packages = Packages::new(vec![p.clone()], BTreeMap::new());
        let get = |class: &str, flags| find_activity_info(&p, &packages, class, flags);
        let with = get("a.Native", GET_META_DATA).expect("a.Native exists");
        assert_eq!(
            entries(&with.component.item.meta_data),
            Some(vec![
                (
                    "android.app.lib_name".into(),
                    Value::String(Some("native-lib".into()))
                ),
                ("flag".into(), Value::Bool(true)),
                ("res".into(), Value::Int(0x7f01_0001)),
                ("scale".into(), Value::Float(1.5)),
            ])
        );
        assert_eq!(
            entries(&with.component.application_info.item.meta_data),
            Some(vec![("app.key".into(), Value::String(Some("v".into())))])
        );
        let without = get("a.Native", 0).expect("a.Native exists");
        assert!(without.component.item.meta_data.is_none());
        assert!(without.component.application_info.item.meta_data.is_none());
        let bare = get("a.Bare", GET_META_DATA).expect("a.Bare exists");
        assert!(
            bare.component.item.meta_data.is_none(),
            "empty meta-data -> null"
        );
        // ComputerEngine.getActivityInfoInternal: null for a class the package does not declare
        assert!(get("a.Missing", GET_META_DATA).is_none());
    }
}
