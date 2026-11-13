// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/pm/ComputerEngine.java, frameworks/base/services/core/java/com/android/server/pm/PackageManagerService.java, frameworks/base/services/core/java/com/android/server/pm/IPackageManagerBase.java, frameworks/base/services/core/java/com/android/server/pm/parsing/PackageInfoUtils.java
//! IPackageManager, subset: getApplicationInfo, getPackageInfo, getActivityInfo, getServiceInfo, getProviderInfo, resolveContentProvider, hasSystemFeature, getPackagesForUid, notifyDexLoad,
//! getInstrumentationInfoAsUser, queryInstrumentationAsUser, checkSignatures. Every other method is counted unimplemented (counters).
//! Every answer is a clone of the installer's PackageInfo (crate::packages) cut to the caller's flags: GET_META_DATA, MATCH_DISABLED_COMPONENTS, and getPackageInfo's GET_<component> / GET_PERMISSIONS.
//! One user (0): a userId other than 0 is a user that does not exist. `sharedLibraryFiles` is always filled (ActivityManagerService's STOCK_PM_FLAGS asks for it on every app path).

use std::io::Write;
use std::sync::Arc;

use atl_aidl::aidl::android::content::pm::IPackageManager::{EnforcePermission, IPackageManager};
use atl_parcelables::android::content::ComponentName;
use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, InstrumentationInfo, PackageInfo, ParceledListSlice,
    ProviderInfo, ServiceInfo,
};
use atl_parcelables::codecs::android::content::pm::PackageInfo::{Signature, SigningDetails};
use atl_parcelables::parts::{ComponentInfo, SharedLibraryInfo};
use binder::{ExceptionCode, Interface, Status, ThreadState};

use crate::packages::{GET_META_DATA, PackageRecord, Packages, Permissions, answer};
use atl_services_types::packages::LibraryNode;
use atl_services_types::permission;

/// The interface name counters use.
const INTERFACE: &str = "IPackageManager";
/// The only user (UserHandle.USER_SYSTEM).
const USER_SYSTEM: i32 = 0;
/// SharedLibraryInfo.TYPE_BUILTIN and VERSION_UNDEFINED (SharedLibraryInfo.java:60, :92).
const SHARED_LIBRARY_TYPE_BUILTIN: i32 = 0;
const SHARED_LIBRARY_VERSION_UNDEFINED: i64 = -1;
/// PackageManager.SIGNATURE_MATCH, NEITHER_SIGNED, FIRST_NOT_SIGNED, SECOND_NOT_SIGNED, NO_MATCH, UNKNOWN_PACKAGE (PackageManager.java:1480-1510).
const SIGNATURE_MATCH: i32 = 0;
const SIGNATURE_NEITHER_SIGNED: i32 = 1;
const SIGNATURE_FIRST_NOT_SIGNED: i32 = -1;
const SIGNATURE_SECOND_NOT_SIGNED: i32 = -2;
const SIGNATURE_NO_MATCH: i32 = -3;
const SIGNATURE_UNKNOWN_PACKAGE: i32 = -4;
/// PackageManager.GET_SIGNATURES, GET_SIGNING_CERTIFICATES (PackageManager.java:1084, :1309).
const GET_SIGNATURES: i64 = 0x40;
const GET_SIGNING_CERTIFICATES: i64 = 0x0800_0000;
/// PackageManager.GET_ACTIVITIES, GET_RECEIVERS, GET_SERVICES, GET_PROVIDERS, GET_INSTRUMENTATION, GET_PERMISSIONS (PackageManager.java): what PackageInfoCommonUtils.generate copies.
const GET_ACTIVITIES: i64 = 0x1;
const GET_RECEIVERS: i64 = 0x2;
const GET_SERVICES: i64 = 0x4;
const GET_PROVIDERS: i64 = 0x8;
const GET_INSTRUMENTATION: i64 = 0x10;
const GET_PERMISSIONS: i64 = 0x1000;
/// PackageInfo.REQUESTED_PERMISSION_GRANTED (1 << 1).
const REQUESTED_PERMISSION_GRANTED: i32 = 1 << 1;
/// ActivityManagerService.STOCK_PM_FLAGS: GET_SHARED_LIBRARY_FILES, the flags AMS reads an app's ApplicationInfo with (bindApplication's, the launch's).
pub const STOCK_PM_FLAGS: i64 = 0x400;

/// The ApplicationInfo of `p` for `flags` (PackageInfoUtils.generateApplicationInfo over the stored one): meta-data only with GET_META_DATA, and the shared libraries its `<uses-library>`s name:
/// `sharedLibraryFiles` (their jars; what ComputerEngine fills for GET_SHARED_LIBRARY_FILES, which STOCK_PM_FLAGS always sets) and `sharedLibraryInfos`, from which LoadedApk builds the libraries' class
/// loaders. A `<uses-library>` that names no installed library adds nothing. Not applied: the scan-time PackageBackwardCompatibility.modifySharedLibraries (implicit org.apache.http.legacy /
/// android.test.base for old targetSdk levels).
pub fn application_info(p: &PackageRecord, packages: &Packages, flags: i64) -> ApplicationInfo {
    let mut a = p.app().clone();
    if flags & GET_META_DATA == 0 {
        a.item.meta_data = None;
    }
    let libraries = packages.used_libraries(p);
    a.shared_library_files =
        (!libraries.is_empty()).then(|| libraries.iter().map(|l| Some(l.file.clone())).collect());
    a.shared_library_infos =
        (!libraries.is_empty()).then(|| libraries.iter().map(|l| Some(library_info(l))).collect());
    a
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

/// A stored component answered for `flags`: [`answer`]'s filter, then its ApplicationInfo as getApplicationInfo builds it for the same flags (generate*Info takes the one generateApplicationInfo
/// built); None when filtered out.
fn cut(p: &PackageRecord, packages: &Packages, c: &mut ComponentInfo, flags: i64) -> Option<()> {
    answer(c, flags).then_some(())?;
    c.application_info = application_info(p, packages, flags);
    Some(())
}

/// PackageInfoUtils.generateActivityInfo of `activity` for `flags`. The launch (LaunchActivityItem.info, STOCK_PM_FLAGS) answers with the component whether or not it is enabled: the start
/// already resolved it.
pub fn activity_info(
    p: &PackageRecord,
    packages: &Packages,
    activity: &ActivityInfo,
    flags: i64,
) -> ActivityInfo {
    let mut a = activity.clone();
    let matched = cut(
        p,
        packages,
        &mut a.component,
        flags | crate::packages::MATCH_DISABLED_COMPONENTS,
    );
    debug_assert!(matched.is_some());
    a
}

/// ComputerEngine.queryContentProviders for one package (ContentProviderHelper.generateApplicationProvidersLocked's call): its providers that match `flags` and have an authority in `process`,
/// higher initOrder first (sProviderInitOrderSorter). Equal orders stay in manifest order; AOSP leaves them in its map's order, which no app can rely on.
pub fn process_providers(
    p: &PackageRecord,
    packages: &Packages,
    process: &str,
    flags: i64,
) -> Vec<ProviderInfo> {
    let mut list: Vec<ProviderInfo> = p
        .providers()
        .filter(|d| d.authority.is_some() && d.component.process_name.as_deref() == Some(process))
        .filter_map(|d| {
            let mut d = d.clone();
            cut(p, packages, &mut d.component, flags).map(|()| d)
        })
        .collect();
    list.sort_by_key(|d| std::cmp::Reverse(d.init_order));
    list
}

/// getActivityInfo after the visibility check: `class` of `p` for `flags`; None for a class `p` does not declare or a disabled one without MATCH_DISABLED_COMPONENTS.
fn find_activity_info(
    p: &PackageRecord,
    packages: &Packages,
    class: &str,
    flags: i64,
) -> Option<ActivityInfo> {
    let mut a = p.activity(class)?.clone();
    cut(p, packages, &mut a.component, flags)?;
    Some(a)
}

/// The stored list of a PackageInfo field, each entry answered for `flags`, when `flags` asks for it (PackageInfoCommonUtils.generate: a list only under its GET_* flag, null when empty).
fn listed<T: Clone>(
    asked: bool,
    list: &Option<Vec<Option<T>>>,
    mut keep: impl FnMut(&mut T) -> bool,
) -> Option<Vec<Option<T>>> {
    if !asked {
        return None;
    }
    let out: Vec<Option<T>> = list
        .iter()
        .flatten()
        .flatten()
        .filter_map(|c| {
            let mut c = c.clone();
            keep(&mut c).then_some(Some(c))
        })
        .collect();
    (!out.is_empty()).then_some(out)
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

    /// PackageInfoUtils.generate (services, :203-207): the parse's requestedPermissionsFlags (REQUIRED, NEVER_FOR_LOCATION, IMPLICIT) plus REQUESTED_PERMISSION_GRANTED for each one the uid
    /// holds now; the installer's PackageInfoCommonUtils.generate has no permission state to set it from.
    fn requested_flags(&self, p: &PackageRecord) -> Option<Vec<i32>> {
        let names = p.info.requested_permissions.as_deref()?;
        let parsed = p
            .info
            .requested_permissions_flags
            .as_deref()
            .unwrap_or_default();
        Some(
            names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let held = name
                        .as_deref()
                        .is_some_and(|n| self.permissions.granted(p.uid, n));
                    parsed.get(i).copied().unwrap_or(0)
                        | if held {
                            REQUESTED_PERMISSION_GRANTED
                        } else {
                            0
                        }
                })
                .collect(),
        )
    }

    /// getPackageInfo after the visibility check (PackageInfoCommonUtils.generate over the stored info, cut to `flags`).
    fn package_info(&self, p: &PackageRecord, flags: i64) -> PackageInfo {
        let packages = &self.packages;
        let c = |c: &mut ComponentInfo| cut(p, packages, c, flags).is_some();
        let info = &p.info;
        PackageInfo {
            application_info: Some(application_info(p, packages, flags)),
            activities: listed(flags & GET_ACTIVITIES != 0, &info.activities, |a| {
                c(&mut a.component)
            }),
            receivers: listed(flags & GET_RECEIVERS != 0, &info.receivers, |a| {
                c(&mut a.component)
            }),
            services: listed(flags & GET_SERVICES != 0, &info.services, |s| {
                c(&mut s.component)
            }),
            providers: listed(flags & GET_PROVIDERS != 0, &info.providers, |d| {
                c(&mut d.component)
            }),
            instrumentation: listed(
                flags & GET_INSTRUMENTATION != 0,
                &info.instrumentation,
                |_| true,
            ),
            permissions: listed(flags & GET_PERMISSIONS != 0, &info.permissions, |_| true),
            requested_permissions: if flags & GET_PERMISSIONS != 0 {
                info.requested_permissions.clone()
            } else {
                None
            },
            requested_permissions_flags: if flags & GET_PERMISSIONS != 0 {
                self.requested_flags(p)
            } else {
                None
            },
            // the installer's generate asked for both signer forms (PackageInfoCommonUtils.generate:196-219 decides their content); each is kept only under its own flag
            signatures: if flags & GET_SIGNATURES != 0 {
                info.signatures.clone()
            } else {
                None
            },
            signing_info: if flags & GET_SIGNING_CERTIFICATES != 0 {
                info.signing_info.clone()
            } else {
                None
            },
            ..info.clone()
        }
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
        atl_services_wire::unimplemented(INTERFACE, method)
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

    /// ComputerEngine.getActivityInfoInternal: null for an unknown user, package or class, or a disabled activity without MATCH_DISABLED_COMPONENTS (ApplicationPackageManager then throws
    /// NameNotFoundException).
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

    /// ComputerEngine.getServiceInfo: null for an unknown user, package or class, or a disabled service without MATCH_DISABLED_COMPONENTS; GET_META_DATA keeps the `<service>`'s meta-data
    /// (Firebase's ComponentDiscoveryService lists its registrars there).
    fn r#getServiceInfo(
        &self,
        component: Option<&ComponentName>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ServiceInfo>> {
        let Some(c) = component else {
            return Ok(None);
        };
        Ok(self.visible(Some(&c.package), user).and_then(|p| {
            let mut s = p.service_info(&c.class, flags)?;
            s.component.application_info = application_info(p, &self.packages, flags);
            Some(s)
        }))
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
            let mut d = p.provider(&c.class)?.clone();
            cut(p, &self.packages, &mut d.component, flags)?;
            Some(d)
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
            let p = self.visible(Some(record.name()), user)?;
            p.providers()
                .filter(|d| {
                    d.authority
                        .as_deref()
                        .is_some_and(|a| a.split(';').any(|one| one == name))
                })
                .find_map(|d| {
                    let mut d = d.clone();
                    cut(p, &self.packages, &mut d.component, flags).map(|()| d)
                })
        }))
    }

    /// ComputerEngine.getPackageInfo:1581: null as getApplicationInfo; the stored PackageInfo cut to the flags.
    fn r#getPackageInfo(
        &self,
        name: Option<&str>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<PackageInfo>> {
        Ok(self
            .visible(name, user)
            .map(|p| self.package_info(p, flags)))
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
            .map(|p| Some(p.name().to_owned()))
            .collect();
        Ok((!names.is_empty()).then_some(names))
    }

    /// PackageManagerService.notifyDexLoad:5611-5628: the system or the loading package's own uid may report; others are ignored (logged in AOSP). The report then goes to ART Service's
    /// dexopt bookkeeping, which ATL does not have: an accepted report is dropped.
    fn r#notifyDexLoad(
        &self,
        package: Option<&str>,
        _contexts: Option<&atl_parcelables::java::util::Map>,
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
        Ok(p.instrumentation()
            .find(|i| i.item.name.as_deref() == Some(c.class.as_str()))
            .cloned())
    }

    /// ComputerEngine.queryInstrumentationAsUser:4965-4999: the instrumentations targeting `targetPackage` (all when null) of the packages the caller may see; an empty list for a user that does not exist.
    fn r#queryInstrumentationAsUser(
        &self,
        target_package: Option<&str>,
        _flags: i32,
        user: i32,
    ) -> binder::Result<Option<ParceledListSlice>> {
        let caller = ThreadState::get_calling_uid();
        let instrumentation: Vec<InstrumentationInfo> = if user != USER_SYSTEM {
            Vec::new()
        } else {
            self.packages
                .all_instrumentation()
                .filter(|(_, i)| {
                    target_package.is_none_or(|t| Some(t) == i.target_package.as_deref())
                })
                .filter(|(p, _)| self.packages.visible(caller, p.name()).is_some())
                .map(|(_, i)| i.clone())
                .collect()
        };
        Ok(Some(ParceledListSlice::new(instrumentation)))
    }

    /// ComputerEngine.checkSignatures:4199-4217: SIGNATURE_UNKNOWN_PACKAGE when either package is not installed or the caller may not see it; otherwise checkSignaturesInternal compares the
    /// signing details the installer verified.
    fn r#checkSignatures(
        &self,
        pkg1: Option<&str>,
        pkg2: Option<&str>,
        user: i32,
    ) -> binder::Result<i32> {
        let (Some(p1), Some(p2)) = (self.visible(pkg1, user), self.visible(pkg2, user)) else {
            return Ok(SIGNATURE_UNKNOWN_PACKAGE);
        };
        Ok(check_signatures(
            p1.info.signing_info.as_ref(),
            p2.info.signing_info.as_ref(),
        ))
    }
}

/// The signers of `d`: none for UNKNOWN (SigningDetails.UNKNOWN has null signatures).
fn signers(d: &SigningDetails) -> Option<Vec<&Signature>> {
    match d {
        SigningDetails::Unknown => None,
        SigningDetails::Known { signatures, .. } => {
            signatures.as_ref().map(|s| s.iter().flatten().collect())
        }
    }
}

/// The oldest signer of a lineage, if `d` has one (SigningDetails.hasPastSigningCertificates: non-null and non-empty).
fn oldest_signer(d: &SigningDetails) -> Option<&Signature> {
    match d {
        SigningDetails::Known {
            past_signing_certificates: Some(past),
            ..
        } => past.first()?.as_ref(),
        _ => None,
    }
}

/// ComputerEngine.checkSignaturesInternal:4276-4303: a null details is not signed; compareSignatures on the current signers, and when that does not match and either has a lineage, the oldest
/// signer of each lineage instead (pre-rotation clients).
fn check_signatures(d1: Option<&SigningDetails>, d2: Option<&SigningDetails>) -> i32 {
    let (Some(d1), Some(d2)) = (d1, d2) else {
        return match (d1, d2) {
            (None, None) => SIGNATURE_NEITHER_SIGNED,
            (None, Some(_)) => SIGNATURE_FIRST_NOT_SIGNED,
            _ => SIGNATURE_SECOND_NOT_SIGNED,
        };
    };
    let result = compare_signature_arrays(signers(d1), signers(d2));
    if result == SIGNATURE_MATCH {
        return result;
    }
    let (o1, o2) = (oldest_signer(d1), oldest_signer(d2));
    if o1.is_none() && o2.is_none() {
        return result;
    }
    fn pick<'a>(o: Option<&'a Signature>, d: &'a SigningDetails) -> Option<Vec<&'a Signature>> {
        o.map(|s| vec![s]).or_else(|| signers(d))
    }
    compare_signature_arrays(pick(o1, d1), pick(o2, d2))
}

/// PackageManagerServiceUtils.compareSignatureArrays:394-430: null sides are unsigned; otherwise equal as sets (Signature.equals compares the bytes).
fn compare_signature_arrays(s1: Option<Vec<&Signature>>, s2: Option<Vec<&Signature>>) -> i32 {
    match (s1, s2) {
        (None, None) => SIGNATURE_NEITHER_SIGNED,
        (None, Some(_)) => SIGNATURE_FIRST_NOT_SIGNED,
        (Some(_), None) => SIGNATURE_SECOND_NOT_SIGNED,
        (Some(a), Some(b)) => {
            if a.len() != b.len() {
                return SIGNATURE_NO_MATCH;
            }
            let set = |v: Vec<&Signature>| -> std::collections::BTreeSet<Option<Vec<u8>>> {
                v.into_iter().map(|s| s.bytes.clone()).collect()
            };
            if set(a) == set(b) {
                SIGNATURE_MATCH
            } else {
                SIGNATURE_NO_MATCH
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::packages::MATCH_DISABLED_COMPONENTS;
    use crate::packages::fixture::*;
    use atl_parcelables::android::os::Bundle;
    use atl_services_types::packages::PermissionDecl;

    /// One package: `<application>` meta-data `app.key`, activity `a.Native` with NativeActivity's `android.app.lib_name`, activity `a.Bare` with none, disabled activity `a.Off`.
    fn packages() -> Packages {
        let mut r = record("a");
        r.info
            .application_info
            .as_mut()
            .expect("app")
            .item
            .meta_data = Some(Bundle::from_strings(&[("app.key", "v")]));
        let mut native = activity("a", "a.Native");
        native.component.item.meta_data = Some(Bundle::from_strings(&[(
            "android.app.lib_name",
            "native-lib",
        )]));
        let mut off = activity("a", "a.Off");
        off.component.enabled = false;
        r.info.activities = Some(vec![Some(native), Some(activity("a", "a.Bare")), Some(off)]);
        Packages::new(vec![r], BTreeMap::new())
    }

    /// PackageInfoUtils.generateActivityInfo:590-595: GET_META_DATA keeps the `<activity>`'s meta-data, without it null; the ApplicationInfo inside follows the same flags (:381). A disabled activity
    /// needs MATCH_DISABLED_COMPONENTS. The visibility / user check before it needs a binder calling uid (`/dev/binder`), so the test starts after it.
    #[test]
    fn get_activity_info_follows_the_flags() {
        let packages = packages();
        let p = packages.by_name("a").expect("a");
        let get = |class: &str, flags| find_activity_info(p, &packages, class, flags);
        let with = get("a.Native", GET_META_DATA).expect("a.Native exists");
        let meta = with.component.item.meta_data.expect("meta-data");
        assert_eq!(
            meta.entries().expect("entries")[0].0,
            "android.app.lib_name"
        );
        assert!(with.component.application_info.item.meta_data.is_some());
        let without = get("a.Native", 0).expect("a.Native exists");
        assert!(without.component.item.meta_data.is_none());
        assert!(without.component.application_info.item.meta_data.is_none());
        assert!(get("a.Off", 0).is_none());
        assert!(get("a.Off", MATCH_DISABLED_COMPONENTS).is_some());
        // break: a component removed from the stored PackageInfo is not found by name
        let mut removed = p.clone();
        removed.info.activities = Some(vec![]);
        assert!(find_activity_info(&removed, &packages, "a.Native", GET_META_DATA).is_none());
    }

    /// PackageInfoCommonUtils.generate: activities only with GET_ACTIVITIES, disabled ones dropped without MATCH_DISABLED_COMPONENTS.
    #[test]
    fn package_info_lists_follow_the_flags() {
        let packages = Arc::new(packages());
        let pm = PackageManager::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages, &[])),
        );
        let p = packages.by_name("a").expect("a");
        assert!(pm.package_info(p, 0).activities.is_none());
        let names = |flags| -> Vec<String> {
            pm.package_info(p, flags)
                .activities
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|a| a.component.item.name)
                .collect()
        };
        assert_eq!(names(GET_ACTIVITIES), ["a.Native", "a.Bare"]);
        assert_eq!(
            names(GET_ACTIVITIES | MATCH_DISABLED_COMPONENTS),
            ["a.Native", "a.Bare", "a.Off"]
        );
    }

    /// PackageInfoUtils.generate: the parse's REQUIRED (1, every uses-permission) plus GRANTED (2) for what the uid holds: INTERNET (normal 0x0) from install, POST_NOTIFICATIONS (dangerous 0x1)
    /// only after grantRuntimePermission. Without GET_PERMISSIONS no flags at all.
    #[test]
    fn requested_flags_carry_the_live_grants() {
        const INTERNET: &str = "android.permission.INTERNET";
        const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";
        let mut r = record("g");
        r.info.requested_permissions =
            Some(vec![Some(INTERNET.into()), Some(POST_NOTIFICATIONS.into())]);
        r.info.requested_permissions_flags = Some(vec![1, 1]);
        let packages = Arc::new(Packages::new(vec![r], BTreeMap::new()));
        let platform =
            [(INTERNET, 0x0), (POST_NOTIFICATIONS, 0x1)].map(|(name, level)| PermissionDecl {
                name: name.into(),
                protection_level: level,
            });
        let permissions = Arc::new(Permissions::new(&packages, &platform));
        let pm = PackageManager::new(Arc::clone(&packages), Arc::clone(&permissions));
        let p = packages.by_name("g").expect("g");
        assert_eq!(pm.package_info(p, 0).requested_permissions_flags, None);
        assert_eq!(
            pm.package_info(p, GET_PERMISSIONS)
                .requested_permissions_flags,
            Some(vec![3, 1])
        );
        permissions
            .grant_runtime(p, POST_NOTIFICATIONS)
            .expect("grant");
        assert_eq!(
            pm.package_info(p, GET_PERMISSIONS)
                .requested_permissions_flags,
            Some(vec![3, 3])
        );
    }

    fn sig(b: u8) -> Option<Signature> {
        Some(Signature {
            bytes: Some(vec![b]),
        })
    }

    fn signed(current: &[u8], past: &[u8]) -> SigningDetails {
        SigningDetails::Known {
            signatures: Some(current.iter().map(|b| sig(*b)).collect()),
            scheme_version: 3,
            public_keys: None,
            past_signing_certificates: (!past.is_empty())
                .then(|| past.iter().map(|b| sig(*b)).collect()),
        }
    }

    /// Expected values: PackageManagerServiceUtils.compareSignatureArrays (set equality) and ComputerEngine.checkSignaturesInternal (the oldest lineage signer on a mismatch).
    #[test]
    fn check_signatures_follows_aosp() {
        let a = signed(&[1], &[]);
        assert_eq!(check_signatures(Some(&a), Some(&a)), SIGNATURE_MATCH);
        assert_eq!(
            check_signatures(Some(&a), Some(&signed(&[2], &[]))),
            SIGNATURE_NO_MATCH
        );
        // order does not matter; size does
        assert_eq!(
            check_signatures(Some(&signed(&[1, 2], &[])), Some(&signed(&[2, 1], &[]))),
            SIGNATURE_MATCH
        );
        assert_eq!(
            check_signatures(Some(&signed(&[1, 2], &[])), Some(&a)),
            SIGNATURE_NO_MATCH
        );
        // rotated 1 -> 2: the oldest signer (1) matches a package still signed by 1
        assert_eq!(
            check_signatures(Some(&signed(&[2], &[1, 2])), Some(&a)),
            SIGNATURE_MATCH
        );
        let unknown = SigningDetails::Unknown;
        assert_eq!(
            check_signatures(Some(&unknown), Some(&a)),
            SIGNATURE_FIRST_NOT_SIGNED
        );
        assert_eq!(
            check_signatures(Some(&a), Some(&unknown)),
            SIGNATURE_SECOND_NOT_SIGNED
        );
        assert_eq!(check_signatures(None, None), SIGNATURE_NEITHER_SIGNED);
    }
}
