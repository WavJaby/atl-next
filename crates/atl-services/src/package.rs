// SPDX-License-Identifier: Apache-2.0
//! Derived from the Android Open Source Project: frameworks/base/services/core/java/com/android/server/pm/ComputerEngine.java, frameworks/base/services/core/java/com/android/server/pm/PackageManagerService.java, frameworks/base/services/core/java/com/android/server/pm/IPackageManagerBase.java, frameworks/base/services/core/java/com/android/server/pm/parsing/PackageInfoUtils.java
//! IPackageManager, subset: getApplicationInfo, getPackageInfo, getActivityInfo, getServiceInfo, getProviderInfo, resolveContentProvider, hasSystemFeature, getSystemAvailableFeatures, getPackagesForUid, notifyDexLoad,
//! getInstrumentationInfoAsUser, queryInstrumentationAsUser, checkSignatures, queryIntentServices, queryIntentActivities, resolveIntent, requestPackageChecksums. Every other method is counted unimplemented (counters).
//! Every answer is a clone of the installer's PackageInfo (crate::packages) cut to the caller's flags: GET_META_DATA, MATCH_DISABLED_COMPONENTS, and getPackageInfo's GET_<component> / GET_PERMISSIONS.
//! One user (0): a userId other than 0 is a user that does not exist. `sharedLibraryFiles` is always filled (ActivityManagerService's STOCK_PM_FLAGS asks for it on every app path).

use std::io::Write;
use std::sync::Arc;

use atl_aidl::aidl::android::content::pm::IOnChecksumsReadyListener::IOnChecksumsReadyListener;
use atl_aidl::aidl::android::content::pm::IPackageManager::{EnforcePermission, IPackageManager};
use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, FeatureInfo, InstrumentationInfo, PackageInfo,
    ParceledListSlice, ProviderInfo, ServiceInfo,
};
use atl_parcelables::android::content::{ComponentName, Intent};
use atl_parcelables::codecs::android::content::pm::PackageInfo::{Signature, SigningDetails};
use atl_parcelables::codecs::android::content::pm::ResolveInfo::{ResolveInfo, Resolved};
use atl_services_types::installer_output::Kind;

use crate::activity::{filter_of, intent_key};
use crate::broadcast;
use crate::packages::FLAG_SYSTEM;
use atl_parcelables::parts::{ComponentInfo, SharedLibraryInfo};
use atl_services_types::system_config::Features;
use binder::{ExceptionCode, Interface, Status, ThreadState};

use crate::packages::{GET_META_DATA, PackageRecord, Packages, Permissions, answer};
use atl_services_types::packages::LibraryNode;
use atl_services_types::permission;

/// The only user (UserHandle.USER_SYSTEM).
pub(crate) const USER_SYSTEM: i32 = 0;
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
pub const GET_SIGNATURES: i64 = 0x40;
/// ApplicationInfo.FLAG_INSTALLED (1 << 23).
pub const FLAG_INSTALLED: i32 = 1 << 23;
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
/// PackageManager.GET_RESOLVED_FILTER (:1091; the same bit as GET_SIGNATURES) and MATCH_DEFAULT_ONLY (:1194).
const GET_RESOLVED_FILTER: i64 = 0x40;
pub(crate) const MATCH_DEFAULT_ONLY: i64 = 0x0001_0000;
/// Intent.CATEGORY_DEFAULT.
const CATEGORY_DEFAULT: &str = "android.intent.category.DEFAULT";

/// The ApplicationInfo of `p` for `flags` (PackageInfoUtils.generateApplicationInfo over the stored one): meta-data only with GET_META_DATA, and the shared libraries its `<uses-library>`s name:
/// `sharedLibraryFiles` (their jars; what ComputerEngine fills for GET_SHARED_LIBRARY_FILES, which STOCK_PM_FLAGS always sets) and `sharedLibraryInfos`, from which LoadedApk builds the libraries' class
/// loaders. A `<uses-library>` that names no installed library adds nothing. Not applied: the scan-time PackageBackwardCompatibility.modifySharedLibraries (implicit org.apache.http.legacy /
/// android.test.base for old targetSdk levels).
pub fn application_info(p: &PackageRecord, packages: &Packages, flags: i64) -> ApplicationInfo {
    let mut a = p.app().clone();
    // PackageInfoUtils.updateApplicationInfo: FLAG_INSTALLED from the user state; every record is installed for the one user
    a.flags |= FLAG_INSTALLED;
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

/// PackageInfoUtils.generateServiceInfo of `service` for `flags`: what scheduleCreateService carries (ServiceRecord.serviceInfo, resolved by the bind, enabled or not).
pub fn service_info(
    p: &PackageRecord,
    packages: &Packages,
    service: &ServiceInfo,
    flags: i64,
) -> ServiceInfo {
    let mut s = service.clone();
    let matched = cut(
        p,
        packages,
        &mut s.component,
        flags | crate::packages::MATCH_DISABLED_COMPONENTS,
    );
    debug_assert!(matched.is_some());
    s
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

/// The package name of a result (RESOLVE_PRIORITY_SORTER's last key).
fn result_package(r: &ResolveInfo) -> &str {
    match &r.info {
        Resolved::Activity(a) => a.component.item.package_name.as_deref().unwrap_or_default(),
        Resolved::Service(s) => s.component.item.package_name.as_deref().unwrap_or_default(),
        _ => "",
    }
}

/// ComputerEngine.queryIntentServicesInternal:649 for `caller`: an Intent naming no component but a selector resolves the selector. An explicit component -> its getServiceInfo (visible package,
/// declared, enabled or MATCH_DISABLED_COMPONENTS). Otherwise queryIntentServicesInternalBody:731 over every package `caller` may see, or only `intent.package` (AppsFilter as
/// Packages::visible); each `<service>` `<intent-filter>` that IntentFilter.match accepts (MATCH_DEFAULT_ONLY: only filters with CATEGORY_DEFAULT, IntentResolver.buildResolveList) gives one result
/// per component (ServiceIntentResolver.allowFilterResult), built as newResult:1661 and sorted by ComponentResolver.RESOLVE_PRIORITY_SORTER. Empty, never null, when nothing matches or the user
/// does not exist. Not modelled: instant apps (none are installed), SaferIntentUtils enforcement (aconfig-flag / compat-change gated; null actions already match nothing here).
/// `resolved_type`: the caller's resolvedType; None falls back to the Intent's own type (intent_key).
pub(crate) fn query_intent_services(
    packages: &Packages,
    caller: u32,
    intent: &Intent,
    resolved_type: Option<&str>,
    flags: i64,
    user: i32,
) -> Vec<ResolveInfo> {
    query_intent(
        packages,
        caller,
        intent,
        resolved_type,
        flags,
        user,
        Component::Service,
    )
    .into_iter()
    .map(|(_, r)| r)
    .collect()
}

/// Which component table a query reads (ComputerEngine's per-kind IntentResolver).
#[derive(Clone, Copy)]
enum Component {
    Activity,
    Service,
}

impl Component {
    fn kind(self) -> Kind {
        match self {
            Component::Activity => Kind::ActivityIntents,
            Component::Service => Kind::ServiceIntents,
        }
    }

    fn classes(self, p: &PackageRecord) -> Vec<&str> {
        fn name(c: &ComponentInfo) -> &str {
            c.item.name.as_deref().unwrap_or_default()
        }
        match self {
            Component::Activity => p.activities().map(|a| name(&a.component)).collect(),
            Component::Service => p.services().map(|s| name(&s.component)).collect(),
        }
    }

    /// get{Activity,Service}Info of `class` in `p` for `flags`, ApplicationInfo filled; None when undeclared or filtered out.
    fn info(
        self,
        p: &PackageRecord,
        packages: &Packages,
        class: &str,
        flags: i64,
    ) -> Option<Resolved> {
        match self {
            Component::Activity => {
                find_activity_info(p, packages, class, flags).map(Resolved::Activity)
            }
            Component::Service => {
                let mut s = p.service_info(class, flags)?;
                s.component.application_info = application_info(p, packages, flags);
                Some(Resolved::Service(s))
            }
        }
    }
}

/// queryIntent{Activities,Services}InternalBody for one component kind, sorted by RESOLVE_PRIORITY_SORTER; each result paired with ResolveInfo.isDefault (not parcelled; the sort and
/// chooseBestActivity read it). The rules: [`query_intent_services`].
fn query_intent(
    packages: &Packages,
    caller: u32,
    intent: &Intent,
    resolved_type: Option<&str>,
    flags: i64,
    user: i32,
    kind: Component,
) -> Vec<(bool, ResolveInfo)> {
    if user != USER_SYSTEM {
        return Vec::new();
    }
    let intent = match (&intent.component, &intent.selector) {
        (None, Some(selector)) => selector.as_ref(),
        _ => intent,
    };
    if let Some(c) = &intent.component {
        return packages
            .visible(caller, &c.package)
            .and_then(|p| kind.info(p, packages, &c.class, flags))
            .map(|info| {
                (
                    false,
                    ResolveInfo {
                        info,
                        ..ResolveInfo::default()
                    },
                )
            })
            .into_iter()
            .collect();
    }
    let key = intent_key(intent, resolved_type);
    let default_only = flags & MATCH_DEFAULT_ONLY != 0;
    let mut found: Vec<(bool, ResolveInfo)> = Vec::new();
    for p in packages.all() {
        if intent.package.as_deref().is_some_and(|n| n != p.name())
            || packages.visible(caller, p.name()).is_none()
        {
            continue;
        }
        for class in kind.classes(p) {
            for ii in p.intents(kind.kind(), class) {
                let Some(f) = &ii.intent_filter else { continue };
                if default_only
                    && !f
                        .categories
                        .iter()
                        .flatten()
                        .flatten()
                        .any(|c| c == CATEGORY_DEFAULT)
                {
                    continue;
                }
                let Some(m) = broadcast::match_value(&filter_of(f), &key) else {
                    continue;
                };
                let Some(info) = kind.info(p, packages, class, flags) else {
                    continue;
                };
                let app_flags = match &info {
                    Resolved::Activity(a) => a.component.application_info.flags,
                    Resolved::Service(s) => s.component.application_info.flags,
                    _ => 0,
                };
                let system = app_flags & FLAG_SYSTEM != 0;
                found.push((
                    ii.has_default,
                    ResolveInfo {
                        info,
                        filter: (flags & GET_RESOLVED_FILTER != 0).then(|| f.clone()),
                        priority: f.priority,
                        r#match: m,
                        label_res: ii.label_res,
                        non_localized_label: ii.non_localized_label.clone(),
                        icon: ii.icon,
                        system,
                        ..ResolveInfo::default()
                    },
                ));
                // allowFilterResult: one result per component, its first matching filter
                break;
            }
        }
    }
    // RESOLVE_PRIORITY_SORTER: priority, preferredOrder (never set), isDefault, match, system descending, then package name; a stable sort keeps manifest order on ties
    found.sort_by(|(da, a), (db, b)| {
        (b.priority, b.preferred_order, *db, b.r#match, b.system)
            .cmp(&(a.priority, a.preferred_order, *da, a.r#match, a.system))
            .then_with(|| result_package(a).cmp(result_package(b)))
    });
    found
}

/// ResolveIntentHelper.resolveIntentInternal + chooseBestActivity:153 over queryIntentActivities: none -> null; one -> it; several -> the first when it differs from the second in priority,
/// preferredOrder or isDefault. A tie is AOSP's ResolverActivity (the chooser), with no preferred activity stored: not modelled, answered null (the caller sees "no activity", never a wrong
/// app) and counted. Not modelled: instant apps, RESOLVE_NON_BROWSER_ONLY (private flag, system callers only), the start path's SaferIntentUtils filter.
pub(crate) fn resolve_activity(
    packages: &Packages,
    caller: u32,
    intent: &Intent,
    resolved_type: Option<&str>,
    flags: i64,
    user: i32,
) -> Option<ResolveInfo> {
    let mut query = query_intent(
        packages,
        caller,
        intent,
        resolved_type,
        flags,
        user,
        Component::Activity,
    );
    if query.len() > 1 {
        let ((d0, r0), (d1, r1)) = (&query[0], &query[1]);
        if (r0.priority, r0.preferred_order, d0) == (r1.priority, r1.preferred_order, d1) {
            atl_diag::refused(
                "android.content.pm.IPackageManager",
                "resolveIntent (chooser tie)",
            );
            return None;
        }
    }
    (!query.is_empty()).then(|| query.swap_remove(0).1)
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
    /// PackageManagerService.mAvailableFeatures: SystemConfig's features (atl_services_types::system_config), read once at start
    features: Features,
    /// `ro.opengles.version` at start (major << 16 | minor; 0 unset): getSystemAvailableFeatures' last entry
    gl_es_version: i32,
}

impl PackageManager {
    pub fn new(
        packages: Arc<Packages>,
        permissions: Arc<Permissions>,
        features: Features,
        gl_es_version: i32,
    ) -> Self {
        PackageManager {
            packages,
            permissions,
            features,
            gl_es_version,
        }
    }

    /// PackageManagerService.getSystemAvailableFeatures:5398: every feature, then one entry with only reqGlEsVersion (`ro.opengles.version`, GL_ES_VERSION_UNDEFINED 0 when unset).
    fn system_available_features(&self) -> Vec<FeatureInfo> {
        self.features
            .iter()
            .map(|(name, version)| FeatureInfo {
                name: Some(name.to_owned()),
                version,
                ..FeatureInfo::default()
            })
            .chain(std::iter::once(FeatureInfo {
                req_gl_es_version: self.gl_es_version,
                ..FeatureInfo::default()
            }))
            .collect()
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

    /// IPackageManager.queryIntentServices over the stored packages (query_intent_services).
    fn query_intent_services(
        &self,
        caller: u32,
        intent: &Intent,
        resolved_type: Option<&str>,
        flags: i64,
        user: i32,
    ) -> Vec<ResolveInfo> {
        query_intent_services(&self.packages, caller, intent, resolved_type, flags, user)
    }

    /// getPackageInfo after the visibility check (PackageInfoCommonUtils.generate over the stored info, cut to `flags`).
    pub fn package_info(&self, p: &PackageRecord, flags: i64) -> PackageInfo {
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

    /// PMS.getPermissionControllerPackageName: the package Packages::permission_controller resolves; null while none is installed (the condition of runtime-permissions P1).
    fn r#getPermissionControllerPackageName(&self) -> binder::Result<Option<String>> {
        Ok(self.packages.permission_controller().map(str::to_owned))
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

    /// PackageManagerService.hasSystemFeature:2954: true when mAvailableFeatures holds `name` at >= `version`; a null name is in no map.
    fn r#hasSystemFeature(&self, name: Option<&str>, version: i32) -> binder::Result<bool> {
        Ok(name.is_some_and(|n| self.features.has(n, version)))
    }

    /// PackageManagerService.getSystemAvailableFeatures:5398 (any caller, instant apps included).
    fn r#getSystemAvailableFeatures(&self) -> binder::Result<Option<ParceledListSlice>> {
        Ok(Some(ParceledListSlice::new(
            self.system_available_features(),
        )))
    }

    /// IPackageManagerBase.queryIntentServices:1094: queryIntentServicesInternal for the calling uid, never null. A null Intent is Java's NullPointerException.
    fn r#queryIntentServices(
        &self,
        intent: Option<&Intent>,
        resolved_type: Option<&str>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ParceledListSlice>> {
        let Some(intent) = intent else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("queryIntentServices: null intent"),
            ));
        };
        let caller = ThreadState::get_calling_uid();
        Ok(Some(ParceledListSlice::new(self.query_intent_services(
            caller,
            intent,
            resolved_type,
            flags,
            user,
        ))))
    }

    /// IPackageManagerBase.queryIntentActivities: queryIntentActivitiesInternal for the calling uid (query_intent), never null. A null Intent is Java's NullPointerException.
    fn r#queryIntentActivities(
        &self,
        intent: Option<&Intent>,
        resolved_type: Option<&str>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ParceledListSlice>> {
        let Some(intent) = intent else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("queryIntentActivities: null intent"),
            ));
        };
        let caller = ThreadState::get_calling_uid();
        let found = query_intent(
            &self.packages,
            caller,
            intent,
            resolved_type,
            flags,
            user,
            Component::Activity,
        );
        Ok(Some(ParceledListSlice::new(
            found.into_iter().map(|(_, r)| r).collect::<Vec<_>>(),
        )))
    }

    /// PackageManagerService.resolveIntent -> ResolveIntentHelper.resolveIntentInternal for the calling uid (resolve_activity). A null Intent is Java's NullPointerException.
    fn r#resolveIntent(
        &self,
        intent: Option<&Intent>,
        resolved_type: Option<&str>,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ResolveInfo>> {
        let Some(intent) = intent else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("resolveIntent: null intent"),
            ));
        };
        let caller = ThreadState::get_calling_uid();
        Ok(resolve_activity(
            &self.packages,
            caller,
            intent,
            resolved_type,
            flags,
            user,
        ))
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
    /// dexopt bookkeeping, which ATL does not have: an accepted report is dropped, counted `answered IPackageManager.notifyDexLoad (not recorded)` (the app gate's evidence it arrived).
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
        } else {
            atl_diag::answered("IPackageManager", "notifyDexLoad (not recorded)");
        }
        Ok(())
    }

    /// ComputerEngine.getInstalledPackages: every package the caller may see, its PackageInfo cut to `flags`, in install order; empty for a user that does not exist.
    /// MATCH_UNINSTALLED_PACKAGES changes nothing: no package here is uninstalled while keeping its data.
    fn r#getInstalledPackages(
        &self,
        flags: i64,
        user: i32,
    ) -> binder::Result<Option<ParceledListSlice>> {
        let caller = ThreadState::get_calling_uid();
        let list: Vec<PackageInfo> = if user != USER_SYSTEM {
            Vec::new()
        } else {
            self.packages
                .all()
                .iter()
                .filter(|p| self.packages.visible(caller, p.name()).is_some())
                .map(|p| self.package_info(p, flags))
                .collect()
        };
        Ok(Some(ParceledListSlice::new(list)))
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

    /// PMS.requestChecksumsInternal:1372: NameNotFound for a package the caller may not see, else ApkChecksums.getChecksums over the APK files delivers through the (oneway) listener. Answered
    /// here with an empty list: the services cannot read app files (no device -> host path in this process), which is ApkChecksums' own answer when each file read fails (getAvailableApkChecksums
    /// and calculateChecksumIfRequested log and skip). Counted as a divergence while `required` asks for checksums AOSP would compute. trustedInstallers only filters installer checksums: none here.
    fn r#requestPackageChecksums(
        &self,
        name: Option<&str>,
        _include_splits: bool,
        _optional: i32,
        required: i32,
        _trusted_installers: Option<&atl_parcelables::java::util::List>,
        listener: Option<&binder::Strong<dyn IOnChecksumsReadyListener>>,
        user: i32,
    ) -> binder::Result<()> {
        let Some(listener) = listener else {
            return Err(Status::new_exception_str(
                ExceptionCode::NULL_POINTER,
                Some("requestPackageChecksums: null listener".to_owned()),
            ));
        };
        if self.visible(name, user).is_none() {
            // AOSP: ParcelableException(NameNotFoundException), rethrown as NameNotFoundException by ApplicationPackageManager; no EX_PARCELABLE writer here, so the nearest checked failure
            atl_diag::count(
                atl_diag::DIVERGENCE,
                "IPackageManager.requestPackageChecksums",
                Some("NameNotFound as IllegalArgumentException"),
            );
            return Err(Status::new_exception_str(
                ExceptionCode::ILLEGAL_ARGUMENT,
                Some(format!("{name:?} not found")),
            ));
        }
        if required != 0 {
            atl_diag::count(
                atl_diag::DIVERGENCE,
                "IPackageManager.requestPackageChecksums",
                Some("required checksums not computed"),
            );
        }
        atl_diag::answered("IPackageManager", "requestPackageChecksums");
        // oneway: no reply to wait for; a dead listener is the caller's loss, as in AOSP (ApkChecksums logs the RemoteException)
        if let Err(e) = listener.onChecksumsReady(Some(&[])) {
            crate::activity::log(format_args!("requestPackageChecksums: listener: {e:?}"));
        }
        Ok(())
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

    /// queryIntentServicesInternal: implicit -> the caller's visible packages' matching `<service>` filters, higher priority first, disabled ones dropped, an invisible package's left out;
    /// explicit -> getServiceInfo; nothing found -> an empty list (Doodle's BillingClient asks for vending's billing service). Expected match = MATCH_CATEGORY_EMPTY + MATCH_ADJUSTMENT_NORMAL
    /// (0x100000 + 0x8000, IntentFilter.java:246-251): action-only filter, data-less Intent.
    #[test]
    fn query_intent_services_resolves_as_component_resolver() {
        const SYNC: &str = "a.SYNC";
        let service = |pkg: &str, class: &str, enabled: bool| {
            let mut c = component(pkg, class);
            c.enabled = enabled;
            Some(ServiceInfo {
                component: c,
                ..ServiceInfo::default()
            })
        };
        let intents = |class: &str, priority: i32| {
            let mut f = filter(&[SYNC], &[]);
            if let Some(f) = f.intent_filter.as_mut() {
                f.priority = priority;
            }
            crate::installed::ComponentIntents {
                kind: Kind::ServiceIntents,
                name: class.to_owned(),
                intents: vec![f],
            }
        };
        let mut a = record("a");
        a.info.services = Some(vec![
            service("a", "a.Low", true),
            service("a", "a.High", true),
            service("a", "a.Off", false),
        ]);
        a.intents = vec![
            intents("a.Low", 0),
            intents("a.High", 5),
            intents("a.Off", 9),
        ];
        let mut b = record("b");
        b.uid = 10_002;
        b.info.services = Some(vec![service("b", "b.Sync", true)]);
        b.intents = vec![intents("b.Sync", 7)];
        let packages = Arc::new(Packages::new(vec![a, b], BTreeMap::new()));
        let pm = PackageManager::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages, &[])),
            Features::default(),
            0,
        );
        let caller = 10_001;
        let classes = |intent: &Intent, flags| -> Vec<(String, i32)> {
            pm.query_intent_services(caller, intent, None, flags, USER_SYSTEM)
                .iter()
                .map(|r| match &r.info {
                    Resolved::Service(s) => {
                        (s.component.item.name.clone().unwrap_or_default(), r.r#match)
                    }
                    _ => (String::new(), 0),
                })
                .collect()
        };
        let implicit = Intent {
            action: Some(SYNC.to_owned()),
            ..Intent::default()
        };
        let m = 0x10_8000;
        assert_eq!(
            classes(&implicit, 0),
            [("a.High".to_owned(), m), ("a.Low".to_owned(), m)]
        );
        assert_eq!(classes(&implicit, MATCH_DISABLED_COMPONENTS).len(), 3);
        // MATCH_DEFAULT_ONLY: no filter here lists CATEGORY_DEFAULT
        assert!(classes(&implicit, MATCH_DEFAULT_ONLY).is_empty());
        // another user does not exist
        assert!(
            pm.query_intent_services(caller, &implicit, None, 0, 10)
                .is_empty()
        );
        let explicit = Intent {
            component: Some(ComponentName {
                package: "a".to_owned(),
                class: "a.Low".to_owned(),
            }),
            ..Intent::default()
        };
        assert_eq!(classes(&explicit, 0), [("a.Low".to_owned(), 0)]);
        let billing = Intent {
            action: Some("com.android.vending.billing.InAppBillingService.BIND".to_owned()),
            package: Some("com.android.vending".to_owned()),
            ..Intent::default()
        };
        assert!(classes(&billing, 0).is_empty());
    }

    /// Implicit bind (ActiveServices.retrieveServiceLocked -> resolveServiceInternal, STOCK_PM_FLAGS, no MATCH_DEFAULT_ONLY): a filter without CATEGORY_DEFAULT still matches, and the first
    /// result (query.get(0)) is the highest priority. Match = MATCH_CATEGORY_EMPTY + MATCH_ADJUSTMENT_NORMAL (0x100000 + 0x8000, IntentFilter.java:246-251), action-only filter, data-less Intent.
    #[test]
    fn query_intent_services_implicit_bind_matches_without_default_category() {
        const SYNC: &str = "a.SYNC";
        let with_priority = |class: &str, priority: i32| {
            let mut f = filter(&[SYNC], &[]);
            if let Some(f) = f.intent_filter.as_mut() {
                f.priority = priority;
            }
            crate::installed::ComponentIntents {
                kind: Kind::ServiceIntents,
                name: class.to_owned(),
                intents: vec![f],
            }
        };
        let mut a = record("a");
        a.info.services = Some(vec![
            Some(ServiceInfo {
                component: component("a", "a.Low"),
                ..ServiceInfo::default()
            }),
            Some(ServiceInfo {
                component: component("a", "a.High"),
                ..ServiceInfo::default()
            }),
        ]);
        a.intents = vec![with_priority("a.Low", 0), with_priority("a.High", 5)];
        let packages = Packages::new(vec![a], BTreeMap::new());
        let implicit = Intent {
            action: Some(SYNC.to_owned()),
            ..Intent::default()
        };
        let found = query_intent_services(
            &packages,
            10_001,
            &implicit,
            None,
            STOCK_PM_FLAGS,
            USER_SYSTEM,
        );
        let names: Vec<(String, i32)> = found
            .iter()
            .map(|r| match &r.info {
                Resolved::Service(s) => {
                    (s.component.item.name.clone().unwrap_or_default(), r.r#match)
                }
                _ => (String::new(), 0),
            })
            .collect();
        assert_eq!(
            names,
            [
                ("a.High".to_owned(), 0x10_8000),
                ("a.Low".to_owned(), 0x10_8000)
            ]
        );
    }

    /// resolveIntent (ResolveIntentHelper.chooseBestActivity): the Facebook SDK's LoginManager.resolveIntent shape -- explicit own component + an action no filter declares, flags 0 -- resolves to
    /// that activity (an unimplemented resolveIntent answered null and the SDK threw "FacebookActivity could not be started"); an undeclared class -> null; implicit: a higher priority wins, an
    /// equal pair is the chooser -> null.
    #[test]
    fn resolve_intent_explicit_and_best_activity() {
        const VIEW: &str = "a.VIEW";
        let mut a = record("a");
        a.info.activities = Some(vec![
            Some(activity("a", "com.facebook.FacebookActivity")),
            Some(activity("a", "a.Low")),
            Some(activity("a", "a.High")),
        ]);
        let intents = |class: &str, priority: i32| {
            let mut f = filter(&[VIEW], &[]);
            if let Some(f) = f.intent_filter.as_mut() {
                f.priority = priority;
            }
            crate::installed::ComponentIntents {
                kind: Kind::ActivityIntents,
                name: class.to_owned(),
                intents: vec![f],
            }
        };
        a.intents = vec![intents("a.Low", 0), intents("a.High", 5)];
        let mut packages = Packages::new(vec![a.clone()], BTreeMap::new());
        let name = |r: Option<ResolveInfo>| match r.map(|r| r.info) {
            Some(Resolved::Activity(x)) => x.component.item.name,
            _ => None,
        };
        let resolve = |p: &Packages, intent: &Intent| {
            name(resolve_activity(p, 10_001, intent, None, 0, USER_SYSTEM))
        };
        let explicit = |class: &str| Intent {
            action: Some("NATIVE_WITH_FALLBACK".to_owned()),
            component: Some(ComponentName {
                package: "a".to_owned(),
                class: class.to_owned(),
            }),
            ..Intent::default()
        };
        assert_eq!(
            resolve(&packages, &explicit("com.facebook.FacebookActivity")).as_deref(),
            Some("com.facebook.FacebookActivity")
        );
        assert_eq!(resolve(&packages, &explicit("a.Missing")), None);
        let implicit = Intent {
            action: Some(VIEW.to_owned()),
            ..Intent::default()
        };
        assert_eq!(resolve(&packages, &implicit).as_deref(), Some("a.High"));
        a.intents = vec![intents("a.Low", 5), intents("a.High", 5)];
        packages = Packages::new(vec![a], BTreeMap::new());
        assert_eq!(resolve(&packages, &implicit), None);
    }

    /// PackageInfoCommonUtils.generate: activities only with GET_ACTIVITIES, disabled ones dropped without MATCH_DISABLED_COMPONENTS.
    #[test]
    fn package_info_lists_follow_the_flags() {
        let packages = Arc::new(packages());
        let pm = PackageManager::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages, &[])),
            Features::default(),
            0,
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
        let pm = PackageManager::new(
            Arc::clone(&packages),
            Arc::clone(&permissions),
            Features::default(),
            0,
        );
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

    /// hasSystemFeature compares versions; getSystemAvailableFeatures lists the features, then the GL ES entry with no name (PackageManagerService :2954, :5398).
    #[test]
    fn features_answer_as_package_manager_service() {
        let packages = Arc::new(packages());
        let features = Features::from_texts([
            r#"<permissions><feature name="android.hardware.touchscreen" /><feature name="v.level" version="1" /></permissions>"#,
        ])
        .expect("features");
        let pm = PackageManager::new(
            Arc::clone(&packages),
            Arc::new(Permissions::new(&packages, &[])),
            features,
            0x30002,
        );
        assert!(
            pm.hasSystemFeature(Some("android.hardware.touchscreen"), 0)
                .unwrap()
        );
        assert!(pm.hasSystemFeature(Some("v.level"), 1).unwrap());
        assert!(!pm.hasSystemFeature(Some("v.level"), 2).unwrap());
        assert!(
            !pm.hasSystemFeature(Some("android.hardware.wifi"), 0)
                .unwrap()
        );
        assert!(!pm.hasSystemFeature(None, 0).unwrap());
        let all = pm.system_available_features();
        let names: Vec<_> = all.iter().map(|f| f.name.as_deref()).collect();
        assert_eq!(
            names,
            [Some("android.hardware.touchscreen"), Some("v.level"), None]
        );
        assert_eq!(all[1].version, 1);
        assert_eq!(all[2].req_gl_es_version, 0x30002);
    }
}
