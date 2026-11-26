// SPDX-License-Identifier: MIT
//! One installed package as the services hold it (maintainer option I): the installer's PackageInfo (PackageInfoCommonUtils.generate over AOSP's own parse) with PMS's per-install fields set on it once
//! ([`InstallFields::apply`]), the intent filters and `<uses-library>` lists PackageInfo does not carry (installer frames), and the install's uid, instruction set, grants and GL driver.
//! Every IPackageManager answer is a clone of a stored info, filtered by the caller's flags ([`answer`]): nothing is re-derived from the manifest.

use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, InstrumentationInfo, PackageInfo, PermissionInfo, ProviderInfo,
    ServiceInfo,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use atl_parcelables::parts::{ComponentInfo, ParsedIntentInfo};
use atl_services_types::installer_output::Kind;
use atl_services_types::packages::{Library, LibraryNode, PermissionDecl, used_libraries};
use atl_services_types::permission::{self, PermissionState};

use crate::activity::{filter_of, intent_key};
use crate::activity_start::Declared;
use crate::broadcast;
use crate::desktop_bounds::{ActivityInput, ActivityOrientation};
use crate::installed::{ComponentIntents, Queries, UsesLibrary};
use crate::package_state::{PackageUserStates, is_enabled};

/// PackageManager.GET_META_DATA (PackageManager.java).
pub const GET_META_DATA: i64 = 0x80;
pub use crate::package_state::MATCH_DISABLED_COMPONENTS;
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";
const ACTION_MANAGE_PERMISSIONS: &str = "android.intent.action.MANAGE_PERMISSIONS";
const CATEGORY_DEFAULT: &str = "android.intent.category.DEFAULT";
/// Build.VERSION_CODES.R: FILTER_APPLICATION_QUERY is enabled from R on (PackageManager.java:5684-5686), so an app below it sees every package.
const VERSION_CODES_R: i32 = 30;
pub(crate) use atl_services_types::system_packages::{FLAG_SYSTEM, PRIVATE_FLAG_PRIVILEGED};
/// (package, app id) of the platform packages a device image installs but ATL ships no APK for, as getPackageUid answers them there. com.android.shell: frameworks/base/packages/Shell
/// AndroidManifest.xml sharedUserId android.uid.shell = SHELL_UID; `am start` (atl-am) names it as its calling package (ActivityManagerShellCommand SHELL_PACKAGE_NAME).
const PLATFORM_PACKAGES: [(&str, u32); 1] = [("com.android.shell", permission::SHELL_UID)];

/// One installed package.
#[derive(Debug, Clone)]
pub struct PackageRecord {
    /// generate's PackageInfo with every GET_* the installer asks for (INFO_FLAGS), PMS's per-install fields applied
    pub info: PackageInfo,
    pub intents: Vec<ComponentIntents>,
    pub libraries: Vec<UsesLibrary>,
    /// manifest `forceQueryable` (AndroidPackage.isForceQueryable); honoured for system packages only (`Packages::visible`)
    pub force_queryable: bool,
    /// manifest `<queries>`: what this package may see of others (`Packages::visible`)
    pub queries: Queries,
    /// manifest `<protected-broadcast>`s, kept for a system package only (ScanPackageUtils.java:936-939 clears them for the rest): `Packages::is_protected_broadcast`
    pub protected_broadcasts: Vec<String>,
    pub uid: u32,
    /// The instruction set the process runs (arches.toml row key): primaryCpuAbi's, else the device's first (InstructionSets.getPrimaryInstructionSet); zygote's `--instruction-set`.
    pub instruction_set: String,
}

fn listed<T>(v: &Option<Vec<Option<T>>>) -> impl Iterator<Item = &T> {
    v.iter().flatten().flatten()
}

impl PackageRecord {
    pub fn name(&self) -> &str {
        self.info.package_name.as_deref().unwrap_or_default()
    }

    /// The package's ApplicationInfo; the installer's output always carries one (installed::decode).
    pub fn app(&self) -> &ApplicationInfo {
        static NONE: std::sync::OnceLock<ApplicationInfo> = std::sync::OnceLock::new();
        self.info
            .application_info
            .as_ref()
            .unwrap_or_else(|| NONE.get_or_init(ApplicationInfo::default))
    }

    /// ApplicationInfo.processName: generate fills it (`<application android:process>`, else the package name).
    pub fn process_name(&self) -> &str {
        self.app().process_name.as_deref().unwrap_or(self.name())
    }

    /// Activities and activity-aliases in manifest order (an alias has `targetActivity`).
    pub fn activities(&self) -> impl Iterator<Item = &ActivityInfo> {
        listed(&self.info.activities)
    }

    pub fn receivers(&self) -> impl Iterator<Item = &ActivityInfo> {
        listed(&self.info.receivers)
    }

    pub fn services(&self) -> impl Iterator<Item = &ServiceInfo> {
        listed(&self.info.services)
    }

    pub fn providers(&self) -> impl Iterator<Item = &ProviderInfo> {
        listed(&self.info.providers)
    }

    pub fn instrumentation(&self) -> impl Iterator<Item = &InstrumentationInfo> {
        listed(&self.info.instrumentation)
    }

    /// `<uses-permission>` as the parse kept them (maxSdkVersion and required-feature already applied).
    pub fn requested_permissions(&self) -> impl Iterator<Item = &str> {
        listed(&self.info.requested_permissions).map(String::as_str)
    }

    /// The `<permission>`s the package defines.
    pub fn permission_decls(&self) -> impl Iterator<Item = &PermissionInfo> {
        listed(&self.info.permissions)
    }

    /// The `<intent-filter>`s of component `name` of `kind` (empty when it has none).
    pub fn intents(&self, kind: Kind, name: &str) -> &[ParsedIntentInfo] {
        self.intents
            .iter()
            .find(|c| c.kind == kind && c.name == name)
            .map_or(&[], |c| c.intents.as_slice())
    }

    pub fn activity(&self, class: &str) -> Option<&ActivityInfo> {
        self.activities().find(|a| named(&a.component, class))
    }

    pub fn receiver(&self, class: &str) -> Option<&ActivityInfo> {
        self.receivers().find(|a| named(&a.component, class))
    }

    pub fn service(&self, class: &str) -> Option<&ServiceInfo> {
        self.services().find(|s| named(&s.component, class))
    }

    pub fn provider(&self, class: &str) -> Option<&ProviderInfo> {
        self.providers().find(|p| named(&p.component, class))
    }

    /// The package declares a component (activity or alias, receiver, service or provider) named `class` (AndroidPackageUtils.hasComponentClassName).
    pub fn has_component(&self, class: &str) -> bool {
        self.activity(class).is_some()
            || self.receiver(class).is_some()
            || self.service(class).is_some()
            || self.provider(class).is_some()
    }

    /// The component `class` is declared and not exported (BroadcastHelper.getAllNotExportedComponents' test; an undeclared name counts as exported there).
    pub fn not_exported(&self, class: &str) -> bool {
        let comps = self
            .activity(class)
            .map(|a| &a.component)
            .or_else(|| self.receiver(class).map(|r| &r.component))
            .or_else(|| self.service(class).map(|s| &s.component))
            .or_else(|| self.provider(class).map(|p| &p.component));
        comps.is_some_and(|c| !c.exported)
    }
}

/// PackageManagerService.isProtectedBroadcast's prefix rules (PackageManagerService.java:5529-5532), before the declared list.
const PROTECTED_BROADCAST_PREFIXES: [&str; 4] = [
    "android.net.netmon.lingerExpired",
    "com.android.server.sip.SipWakeupTimer",
    "com.android.internal.telephony.data-reconnect",
    "android.net.netmon.launchCaptivePortalApp",
];

/// The installed packages, the shared libraries (SystemConfig `<library>`) they may use, and user 0's package state over them ([`PackageUserStates`]); one user (0).
#[derive(Debug, Default)]
pub struct Packages {
    list: Vec<PackageRecord>,
    libraries: BTreeMap<String, Library>,
    /// framework-res's `<protected-broadcast>`s: package `android` is not scanned (atl-services-types system_packages), so its manifest's list comes as a services input ([`Self::with_platform_broadcasts`])
    platform_broadcasts: BTreeSet<String>,
    /// the table's state, shared by every list of one table: [`PackageTable::publish`] refuses a list that does not carry it
    user: Arc<PackageUserStates>,
}

impl Packages {
    /// The packages with an empty package state kept in memory only ([`Packages::with_user_state`] gives the saved one).
    pub fn new(list: Vec<PackageRecord>, libraries: BTreeMap<String, Library>) -> Self {
        Packages {
            list,
            libraries,
            platform_broadcasts: BTreeSet::new(),
            user: Arc::new(PackageUserStates::default()),
        }
    }

    /// The platform's (framework-res's) protected broadcasts, added as PMS adds a scanned system package's (InstallPackageHelper.java:642-647).
    pub fn with_platform_broadcasts(mut self, actions: impl IntoIterator<Item = String>) -> Self {
        self.platform_broadcasts.extend(actions);
        self
    }

    /// PackageManagerService.isProtectedBroadcast (:5526-5540): the prefix rules, then mProtectedBroadcasts = framework-res's and every system package's `<protected-broadcast>`s. A null action is not protected.
    pub fn is_protected_broadcast(&self, action: Option<&str>) -> bool {
        let Some(action) = action else {
            return false;
        };
        PROTECTED_BROADCAST_PREFIXES
            .iter()
            .any(|p| action.starts_with(p))
            || self.platform_broadcasts.contains(action)
            || self
                .list
                .iter()
                .any(|p| p.protected_broadcasts.iter().any(|a| a == action))
    }

    /// These packages with `user` as user 0's package state (the services' start: [`PackageUserStates::load`]).
    pub fn with_user_state(self, user: PackageUserStates) -> Self {
        Packages {
            user: Arc::new(user),
            ..self
        }
    }

    /// These packages with the table's shared state ([`PackageTable::user_state_arc`]): the kind of list [`PackageTable::publish`] takes.
    pub fn with_user_state_arc(self, user: Arc<PackageUserStates>) -> Self {
        Packages { user, ..self }
    }

    /// These packages with `records` in place: each replaces the stored record of its name, a new name is appended. Libraries, platform broadcasts and the user state are kept
    /// (the list is cloned: tens of packages).
    pub fn with_records(&self, records: Vec<PackageRecord>) -> Packages {
        let mut list = self.list.clone();
        for r in records {
            match list.iter().position(|p| p.name() == r.name()) {
                Some(i) => list[i] = r,
                None => list.push(r),
            }
        }
        Packages {
            list,
            libraries: self.libraries.clone(),
            platform_broadcasts: self.platform_broadcasts.clone(),
            user: Arc::clone(&self.user),
        }
    }

    /// User 0's package state (IPackageManager's component-state calls read and change it).
    pub fn user_state(&self) -> &PackageUserStates {
        &self.user
    }

    /// PackageUserStateUtils.isEnabled for the stored component `c` (its package and class names, its manifest `enabled` and its application's) against user 0's state: the one test every resolution
    /// applies (queries through [`Packages::answer`], the launcher, starts, binds and broadcasts).
    pub fn enabled(&self, c: &ComponentInfo, flags: i64) -> bool {
        let package = c.item.package_name.as_deref().unwrap_or_default();
        let class = c.item.name.as_deref().unwrap_or_default();
        is_enabled(
            c.application_info.enabled,
            c.enabled,
            self.user.component(package, class),
            flags,
        )
    }

    /// A stored component cut to the caller's `flags` (PackageInfoUtils.generate*Info): meta-data only with GET_META_DATA (the component's and its ApplicationInfo's); false = filtered out, a
    /// component [`Packages::enabled`] rejects for `flags` (PackageStateUtils.isEnabledAndMatches).
    pub fn answer(&self, c: &mut ComponentInfo, flags: i64) -> bool {
        if !self.enabled(c, flags) {
            return false;
        }
        if flags & GET_META_DATA == 0 {
            c.item.meta_data = None;
            c.application_info.item.meta_data = None;
        }
        true
    }

    /// The launcher entry of `p`: the first enabled activity or alias, in manifest order, with a MAIN + LAUNCHER filter (PMS queryIntentActivities' order for the launcher's query; a disabled
    /// component matches no query). The caller starts its `targetActivity` when set (an alias), keeping the alias as the component name.
    pub fn launcher<'a>(&self, p: &'a PackageRecord) -> Option<&'a ActivityInfo> {
        p.activities()
            .filter(|a| self.enabled(&a.component, 0))
            .find(|a| {
                let name = a.component.item.name.as_deref().unwrap_or_default();
                p.intents(Kind::ActivityIntents, name).iter().any(launches)
            })
    }

    /// Whether `class` of `p` is an enabled activity or alias with a MAIN + LAUNCHER filter (ShortcutService.injectIsMainActivity's query).
    pub fn is_launcher(&self, p: &PackageRecord, class: &str) -> bool {
        p.activities().any(|a| {
            named(&a.component, class)
                && self.enabled(&a.component, 0)
                && p.intents(Kind::ActivityIntents, class).iter().any(launches)
        })
    }

    /// getServiceInfo's answer (ComputerEngine.getServiceInfo): `p`'s stored ServiceInfo filtered by `flags` ([`Packages::answer`]); None for a class the package does not declare or a disabled one
    /// without MATCH_DISABLED_COMPONENTS.
    pub fn service_info(&self, p: &PackageRecord, class: &str, flags: i64) -> Option<ServiceInfo> {
        let mut out = p.service(class)?.clone();
        self.answer(&mut out.component, flags).then_some(out)
    }

    /// getPermissionControllerPackageName: PMS.getRequiredPermissionControllerLPr (PackageManagerService.java:2700-2716) resolves ACTION_MANAGE_PERMISSIONS + CATEGORY_DEFAULT among system
    /// packages (MATCH_SYSTEM_ONLY: ApplicationInfo FLAG_SYSTEM) and wants exactly one match, a privileged app. AOSP throws at boot otherwise; here None = no controller installed (no package
    /// is a system package until runtime-permissions S4 installs PermissionController).
    pub fn permission_controller(&self) -> Option<&str> {
        let mut matches = self
            .list
            .iter()
            .filter(|p| p.app().flags & FLAG_SYSTEM != 0)
            .flat_map(|p| {
                p.activities()
                    .filter(|a| self.enabled(&a.component, 0))
                    .filter(move |a| {
                        let name = a.component.item.name.as_deref().unwrap_or_default();
                        p.intents(Kind::ActivityIntents, name)
                            .iter()
                            .any(|i| has(i, ACTION_MANAGE_PERMISSIONS, CATEGORY_DEFAULT))
                    })
                    .map(move |_| p)
            });
        let (Some(only), None) = (matches.next(), matches.next()) else {
            return None;
        };
        (only.app().private_flags & PRIVATE_FLAG_PRIVILEGED != 0).then(|| only.name())
    }

    pub fn by_name(&self, name: &str) -> Option<&PackageRecord> {
        self.list.iter().find(|p| p.name() == name)
    }

    /// Packages whose uid has `app_id` (shared uids give several).
    pub fn by_app_id(&self, app_id: u32) -> impl Iterator<Item = &PackageRecord> {
        self.list
            .iter()
            .filter(move |p| permission::app_id(p.uid) == app_id)
    }

    /// isSameApp (PackageManagerInternal, PackageManagerService.java:7204-7216): getPackageUid(`package`) has `uid`'s app id, shared uids included; the platform packages ATL ships no APK for
    /// ([`PLATFORM_PACKAGES`]) answer their system image uid.
    pub fn is_same_app(&self, package: &str, uid: u32) -> bool {
        let app_id = permission::app_id(uid);
        PLATFORM_PACKAGES.contains(&(package, app_id))
            || self.by_app_id(app_id).any(|p| p.name() == package)
    }

    /// Every package, in install order.
    pub fn all(&self) -> &[PackageRecord] {
        &self.list
    }

    /// The instrumentation `class` of `package` and the package declaring it (ComputerEngine's mInstrumentation, keyed by ComponentName).
    pub fn instrumentation(
        &self,
        package: &str,
        class: &str,
    ) -> Option<(&PackageRecord, &InstrumentationInfo)> {
        let p = self.by_name(package)?;
        let i = p
            .instrumentation()
            .find(|i| i.item.name.as_deref() == Some(class))?;
        Some((p, i))
    }

    /// Every instrumentation, with its package, in install order (ComputerEngine.queryInstrumentationAsUser's iteration).
    pub fn all_instrumentation(
        &self,
    ) -> impl Iterator<Item = (&PackageRecord, &InstrumentationInfo)> {
        self.list
            .iter()
            .flat_map(|p| p.instrumentation().map(move |i| (p, i)))
    }

    /// The installed libraries `p` uses (its `<uses-library>`s, required first, as the parse keeps them), dependencies below each.
    pub fn used_libraries(&self, p: &PackageRecord) -> Vec<LibraryNode> {
        used_libraries(&self.libraries, p.libraries.iter().map(|l| l.name.as_str()))
    }

    /// What `caller` may see of `package` (ComputerEngine shouldFilterApplication, AppsFilterBase.shouldFilterApplicationInternal in AOSP's order): a caller below the first application uid, an app its own package, a target below it (AppsFilterBase:420-427), a force-queryable system package (AppsFilterImpl.addPackageInternal:602-610; config_forceSystemPackagesQueryable and the device list are AOSP's defaults: false, empty), and any package of the caller's app id that `grants` it (a shared uid pools its members' rules, :433-451).
    pub fn visible(&self, caller: u32, package: &str) -> Option<&PackageRecord> {
        let p = self.by_name(package)?;
        let own = permission::app_id(p.uid) == permission::app_id(caller);
        let below_app_uids = permission::app_id(p.uid) < permission::FIRST_APPLICATION_UID;
        let force_queryable = p.force_queryable && p.app().flags & FLAG_SYSTEM != 0;
        let granted = self
            .by_app_id(permission::app_id(caller))
            .any(|c| grants(c, p));
        (permission::sees_all_packages(caller)
            || own
            || below_app_uids
            || force_queryable
            || granted)
            .then_some(p)
    }
}

/// The package list the services read (PackageManagerService.snapshotComputer): a whole Packages at a time, replaced by publish; snapshots already taken keep theirs.
#[derive(Debug)]
pub struct PackageTable {
    current: Mutex<Arc<Packages>>,
}

impl PackageTable {
    /// The list published at start; its user state is the table's for life.
    pub fn new(first: Packages) -> Self {
        PackageTable {
            current: Mutex::new(Arc::new(first)),
        }
    }

    /// The list as published now: a clone of its Arc, the lock held for the clone only.
    pub fn snapshot(&self) -> Arc<Packages> {
        Arc::clone(&self.current.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The user state every list of this table carries: a list for [`PackageTable::publish`] is built with [`Packages::with_user_state_arc`].
    /// Fixed for the table's life (publish keeps it), so the Arc taken here stays the table's.
    pub fn user_state_arc(&self) -> Arc<PackageUserStates> {
        Arc::clone(&self.current.lock().unwrap_or_else(|e| e.into_inner()).user)
    }

    /// Replaces the list. Component state belongs to the table (PMS's Settings, not the list): `next` must carry the table's state, else panic (caller bug: the state set so far would be dropped).
    /// Snapshots already taken keep their list; they share the state, so they read its writes too.
    pub fn publish(&self, next: Packages) {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            Arc::ptr_eq(&next.user, &current.user),
            "PackageTable::publish: next must carry the table's user state (PackageTable::user_state_arc); component state belongs to the table"
        );
        *current = Arc::new(next);
    }
}

/// Whether caller package `c` may see `p`: AppsFilterBase.shouldFilterApplicationInternal's grants in AOSP's order. A target-SDK below R (FILTER_APPLICATION_QUERY off), QUERY_ALL_PACKAGES, `<queries><package>`,
/// `<queries>` intents and providers, instrumentation either way (AppsFilterImpl.pkgInstruments), a permission `c` requests that `p` declares (mQueryableViaUsesPermission, AppsFilterImpl.addPackageInternal:618-670).
/// Not modelled: installer and update-owner (no install source kept), implicit grants, overlays, static libraries, `<library>` providers, protected broadcasts (no list kept).
fn grants(c: &PackageRecord, p: &PackageRecord) -> bool {
    c.app().target_sdk_version < VERSION_CODES_R
        || c.requested_permissions()
            .any(|r| r == permission::QUERY_ALL_PACKAGES)
        || c.queries.packages.iter().any(|n| n == p.name())
        || queries_component(c, p)
        || instruments(c, p)
        || instruments(p, c)
        || c.requested_permissions().any(|r| {
            p.permission_decls()
                .any(|d| d.item.name.as_deref() == Some(r))
        })
}

/// AppsFilterUtils.canQueryViaComponents: a `<queries>` intent matches an intent filter of an exported component of `p` (matchesAnyComponents; IntentFilter.match, no default-category
/// requirement), or a `<queries>` provider authority is a `;` token of an exported provider's authority (matchesProviders). Receivers match without the protected-broadcast exclusion.
fn queries_component(c: &PackageRecord, p: &PackageRecord) -> bool {
    let by_intent = c.queries.intents.iter().any(|q| {
        let key = intent_key(q, None);
        p.intents.iter().any(|ci| {
            exported(p, ci.kind, &ci.name)
                && ci.intents.iter().any(|ii| {
                    ii.intent_filter
                        .as_ref()
                        .is_some_and(|f| broadcast::match_value(&filter_of(f), &key).is_some())
                })
        })
    });
    let by_provider =
        p.providers().any(|pr| {
            pr.component.exported
                && pr.authority.iter().flat_map(|a| a.split(';')).any(|token| {
                    !token.is_empty() && c.queries.providers.iter().any(|q| q == token)
                })
        });
    by_intent || by_provider
}

/// The component `name` of `kind` in `p` is exported (ParsedMainComponent.isExported).
fn exported(p: &PackageRecord, kind: Kind, name: &str) -> bool {
    match kind {
        Kind::ActivityIntents => p.activity(name).is_some_and(|a| a.component.exported),
        Kind::ReceiverIntents => p.receiver(name).is_some_and(|a| a.component.exported),
        Kind::ServiceIntents => p.service(name).is_some_and(|s| s.component.exported),
        Kind::ProviderIntents => p.provider(name).is_some_and(|x| x.component.exported),
        Kind::Package | Kind::PackageFields => false,
    }
}

/// AppsFilterImpl.pkgInstruments: `source` declares an instrumentation whose target package is `target`.
fn instruments(source: &PackageRecord, target: &PackageRecord) -> bool {
    source
        .instrumentation()
        .any(|i| i.target_package.as_deref() == Some(target.name()))
}

/// Why grantRuntimePermission refuses (PermissionManagerServiceImpl.grantRuntimePermissionInternal): `UnknownPermission` is its IllegalArgumentException, the other two its SecurityExceptions.
/// The messages are AOSP's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    UnknownPermission(String),
    NotChangeable { permission: String, package: String },
    NotRequested { permission: String, package: String },
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrantError::UnknownPermission(p) => write!(f, "Unknown permission: {p}"),
            GrantError::NotChangeable {
                permission,
                package,
            } => write!(
                f,
                "Permission {permission} requested by {package} is not a changeable permission type"
            ),
            GrantError::NotRequested {
                permission,
                package,
            } => write!(
                f,
                "Package {package} has not requested permission {permission}"
            ),
        }
    }
}

/// The permission state every service consults (PermissionManagerServiceImpl's registry + UidPermissionState): the registry of definitions, and each app id's permission states, first
/// `permission::initial_states` of its packages (a shared uid's packages pool theirs, as PMS's shared user does), then changed by runtime grants. One user (0); not persisted (plan S6).
/// Registry and states sit under one lock: a batch of installs changes both at once ([`Permissions::packages_added`]).
pub struct Permissions {
    table: Mutex<Table>,
}

/// What [`Permissions`] guards together.
struct Table {
    registry: permission::Registry,
    /// app id -> its permission states, by name
    states: BTreeMap<u32, BTreeMap<String, PermissionState>>,
}

impl Permissions {
    /// `platform` = framework-res's `<permission>` definitions, registered before every package's.
    pub fn new(packages: &Packages, platform: &[PermissionDecl]) -> Self {
        let registry = permission::registry(
            platform
                .iter()
                .map(|d| {
                    (
                        permission::PLATFORM_PACKAGE,
                        d.name.as_str(),
                        d.protection_level,
                    )
                })
                .chain(packages.all().iter().flat_map(|o| {
                    o.permission_decls().filter_map(move |d| {
                        Some((o.name(), d.item.name.as_deref()?, d.protection_level as u32))
                    })
                })),
        );
        let mut states: BTreeMap<u32, BTreeMap<String, PermissionState>> = BTreeMap::new();
        for p in packages.all() {
            let uid_states = states.entry(permission::app_id(p.uid)).or_default();
            for s in permission::initial_states(
                p.name(),
                p.app().target_sdk_version,
                p.requested_permissions(),
                &registry,
            ) {
                uid_states.entry(s.name.clone()).or_insert(s);
            }
        }
        Permissions {
            table: Mutex::new(Table { registry, states }),
        }
    }

    /// The install-order step of a batch: `names` are the batch's packages, already in `next`; one lock for the whole step.
    /// (1) their definitions (addPermissions :442-598, trimPermissions :600): a new name is defined, first definer wins; a batch member's own changed definition replaces the old one;
    /// a definition no longer declared, or one whose protection base changes, drops every app id's state of it. (2) each one's initial states into its app id, pooled as [`Permissions::new`] pools
    /// them; a kept request keeps its state (grant and flags). (3) per batch app id: trimPermissionStates (:682), and a REVIEW_REQUIRED grant is not kept by a targetSdk >= M update (:1103).
    /// (4) each permission the batch defined or changed that a package outside the batch requests gets its initial state there, except a NORMAL one for a non-system package: AOSP marks it INSTALL_REVOKED.
    /// One batch of every package equals `new` over `next` (boot's start-with-all).
    /// Not modelled: group-change revoke (addPermissions :517-551), storage / media revoke (:703); a definition change that keeps its base keeps the states (AOSP re-evaluates each one).
    pub fn packages_added(&self, next: &Packages, names: &[String]) {
        let mut table = self.table();
        let Table { registry, states } = &mut *table;
        let (batch, others): (Vec<&PackageRecord>, Vec<&PackageRecord>) = next
            .all()
            .iter()
            .partition(|p| names.iter().any(|n| n == p.name()));
        let mut defined = BTreeSet::new();
        for &p in &batch {
            // trimPermissions (:600): this package's definitions it no longer declares go, and with them every app id's state of them
            let declared: BTreeSet<&str> = p
                .permission_decls()
                .filter_map(|d| d.item.name.as_deref())
                .collect();
            let gone: Vec<String> = registry
                .iter()
                .filter(|(name, d)| d.owner == p.name() && !declared.contains(name.as_str()))
                .map(|(name, _)| name.clone())
                .collect();
            for name in &gone {
                registry.remove(name);
                drop_states(states, name);
            }
            for d in p.permission_decls() {
                let Some(name) = d.item.name.as_deref() else {
                    continue;
                };
                let level = d.protection_level as u32;
                if permission::define(registry, p.name(), name, level) {
                    defined.insert(name);
                } else if let Some(old) = permission::redefine(registry, p.name(), name, level) {
                    // addPermissions :521-548 (a protection change revokes) with :144-146 (changed names re-evaluated): a changed base drops every app id's state of it; the
                    // requesters are re-derived below
                    if registry
                        .get(name)
                        .is_some_and(|def| def.base() != old.base())
                    {
                        drop_states(states, name);
                    }
                    defined.insert(name);
                }
            }
        }
        for &p in &batch {
            let uid_states = states.entry(permission::app_id(p.uid)).or_default();
            for s in permission::initial_states(
                p.name(),
                p.app().target_sdk_version,
                p.requested_permissions(),
                registry,
            ) {
                uid_states.entry(s.name.clone()).or_insert(s);
            }
        }
        // trimPermissionStates (:682): a state of a permission no package of the app id requests goes. evaluatePermissionState's modern branch (:1103-1110): a REVIEW_REQUIRED grant
        // (API view of LEGACY_GRANTED + IMPLICIT) is not kept when every requester is targetSdk >= M; a plain legacy grant (the app's own permission) is kept
        let batch_ids: BTreeSet<u32> = batch.iter().map(|p| permission::app_id(p.uid)).collect();
        for app_id in batch_ids {
            let members: Vec<&PackageRecord> = next.by_app_id(app_id).collect();
            let Some(uid_states) = states.get_mut(&app_id) else {
                continue;
            };
            let requested =
                |p: &PackageRecord, name: &str| p.requested_permissions().any(|r| r == name);
            uid_states.retain(|name, _| members.iter().any(|p| requested(p, name.as_str())));
            for s in uid_states.values_mut() {
                let modern = members
                    .iter()
                    .filter(|p| requested(p, s.name.as_str()))
                    .all(|p| {
                        p.app().target_sdk_version >= permission::TARGET_SDK_RUNTIME_PERMISSIONS
                    });
                if s.runtime
                    && s.granted
                    && (s.flags & permission::FLAG_PERMISSION_REVIEW_REQUIRED) != 0
                    && modern
                {
                    s.granted = false;
                    s.flags &= !(permission::FLAG_PERMISSION_REVIEW_REQUIRED
                        | permission::FLAG_PERMISSION_REVOKED_COMPAT);
                }
            }
        }
        for p in others {
            let wanted: Vec<&str> = p
                .requested_permissions()
                .filter(|r| defined.contains(r))
                .collect();
            let system = p.app().flags & FLAG_SYSTEM != 0;
            let uid_states = states.entry(permission::app_id(p.uid)).or_default();
            for s in
                permission::initial_states(p.name(), p.app().target_sdk_version, wanted, registry)
            {
                if s.granted && !s.runtime && !system {
                    continue;
                }
                uid_states.entry(s.name.clone()).or_insert(s);
            }
        }
    }

    /// The table's lock; a poisoned one is taken anyway (plain data, no invariant a panic can break).
    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn state(&self, uid: u32, p: &str) -> Option<PermissionState> {
        self.table()
            .states
            .get(&permission::app_id(uid))
            .and_then(|s| s.get(p))
            .cloned()
    }

    /// UidPermissionState.isPermissionGranted for `uid`'s app id.
    pub fn granted(&self, uid: u32, p: &str) -> bool {
        self.state(uid, p).is_some_and(|s| s.granted)
    }

    /// The names `uid` holds now (the broker's sandbox rights read them at process start).
    pub fn granted_names(&self, uid: u32) -> Vec<String> {
        self.table()
            .states
            .get(&permission::app_id(uid))
            .map(|s| {
                s.values()
                    .filter(|s| s.granted)
                    .map(|s| s.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// UidPermissionState.getPermissionFlags: 0 for a permission without state.
    pub fn flags(&self, uid: u32, p: &str) -> u32 {
        self.state(uid, p).map_or(0, |s| s.flags)
    }

    /// grantRuntimePermissionInternal after the caller and package checks: the permission must be defined and runtime (role and development permissions are not modelled) and requested by
    /// `package` (or already in its uid's state); a legacy app's runtime permission is left as it is (granted, with its review flag), as AOSP returns early for targetSdk < M.
    pub fn grant_runtime(&self, package: &PackageRecord, p: &str) -> Result<(), GrantError> {
        let mut table = self.table();
        let Table { registry, states } = &mut *table;
        let definition = registry
            .get(p)
            .ok_or_else(|| GrantError::UnknownPermission(p.to_owned()))?;
        if !definition.is_runtime() {
            return Err(GrantError::NotChangeable {
                permission: p.to_owned(),
                package: package.name().to_owned(),
            });
        }
        let uid_states = states.entry(permission::app_id(package.uid)).or_default();
        if !uid_states.contains_key(p) && !package.requested_permissions().any(|r| r == p) {
            return Err(GrantError::NotRequested {
                permission: p.to_owned(),
                package: package.name().to_owned(),
            });
        }
        if package.app().target_sdk_version < permission::TARGET_SDK_RUNTIME_PERMISSIONS {
            return Ok(());
        }
        uid_states
            .entry(p.to_owned())
            .or_insert_with(|| PermissionState {
                name: p.to_owned(),
                runtime: true,
                granted: false,
                flags: 0,
            })
            .granted = true;
        Ok(())
    }

    /// `uid`'s grant of runtime permission `p` when its state holds one (the app requested it), else None: the permissions PermissionPolicyService syncs app-op modes from.
    pub fn runtime_grant(&self, uid: u32, p: &str) -> Option<bool> {
        self.state(uid, p).filter(|s| s.runtime).map(|s| s.granted)
    }

    /// The registered definition is runtime or an app-op permission (AppOpsService.shouldCollectNotes' test); an undefined one is neither.
    pub fn is_runtime_or_app_op(&self, p: &str) -> bool {
        self.table()
            .registry
            .get(p)
            .is_some_and(|d| d.is_runtime() || d.is_app_op())
    }

    /// `permission::holds` against `uid`'s grants.
    pub fn holds(&self, uid: u32, permissions: &[&str], any: bool) -> bool {
        permission::holds(uid, permissions, any, |p| self.granted(uid, p))
    }

    /// BroadcastSkipPolicy.isSignaturePerm (:680-705): null is false; every permission must be defined and signature-only (an empty list is vacuously true).
    pub fn is_signature_only(&self, perms: Option<&[String]>) -> bool {
        let table = self.table();
        perms.is_some_and(|list| {
            list.iter()
                .all(|p| table.registry.get(p).is_some_and(|d| d.is_signature_only()))
        })
    }

    /// `permission::holds` of one permission against `uid`'s grants; a null permission is not held.
    pub fn check(&self, uid: u32, p: Option<&str>) -> bool {
        p.is_some_and(|p| self.holds(uid, &[p], false))
    }
}

/// `name`'s state at every app id: setPermissionFlags(appId, userId, name, 0) for each app id (AppIdPermissionPolicy.kt :511-515, :548, :655-660).
fn drop_states(states: &mut BTreeMap<u32, BTreeMap<String, PermissionState>>, name: &str) {
    for uid_states in states.values_mut() {
        uid_states.remove(name);
    }
}

/// ActivityInfo.CONFIG_SCREEN_SIZE, CONFIG_SMALLEST_SCREEN_SIZE (ActivityInfo.java) and Build.VERSION_CODES.HONEYCOMB_MR2.
const CONFIG_SCREEN_SIZE: u32 = 0x400;
const CONFIG_SMALLEST_SCREEN_SIZE: u32 = 0x800;
const HONEYCOMB_MR2: i32 = 13;

/// ActivityInfo.getRealConfigChanged: what `a` handles itself, the bits a relaunch decision reads; an app below 13 predates the size bits, so it handles size changes too.
pub fn real_config_changes(a: &ActivityInfo) -> u32 {
    let declared = a.config_changes as u32;
    if a.component.application_info.target_sdk_version < HONEYCOMB_MR2 {
        declared | CONFIG_SCREEN_SIZE | CONFIG_SMALLEST_SCREEN_SIZE
    } else {
        declared
    }
}

/// ActivityInfo.RESIZE_MODE_* that ActivityInfo.isResizeableMode lists (ActivityInfo.java:358-397, :2073): RESIZEABLE_VIA_SDK_VERSION 1, RESIZEABLE 2, FORCE_RESIZEABLE 4, FORCE_RESIZABLE_LANDSCAPE_ONLY 5,
/// FORCE_RESIZABLE_PORTRAIT_ONLY 6, FORCE_RESIZABLE_PRESERVE_ORIENTATION 7.
pub fn is_resizeable_mode(mode: i32) -> bool {
    matches!(mode, 1 | 2 | 4..=7)
}

/// What the initial-bounds rule reads of an installed activity: orientation, resize mode, aspect limits as the parse set them (PackageParser's resize-mode and aspect rules, the pre-O default max
/// aspect included); an aspect of 0 is ActivityInfo's "not set".
pub fn bounds_input(a: &ActivityInfo) -> ActivityInput {
    let set = |v: f32| (v != 0.0).then_some(v);
    ActivityInput {
        orientation: ActivityOrientation::from_screen_orientation(a.screen_orientation),
        resizeable: is_resizeable_mode(a.resize_mode),
        min_aspect: set(a.min_aspect_ratio),
        max_aspect: set(a.max_aspect_ratio),
    }
}

/// What a start's check reads of the activity it starts (activity_start, which runs without the converter's types).
pub fn declared(a: &ActivityInfo) -> Declared {
    Declared {
        name: a.component.item.name.clone().unwrap_or_default(),
        launch_mode: a.launch_mode,
        process: a.component.process_name.clone(),
        task_affinity: a.task_affinity.clone(),
        no_history: a.flags & FLAG_NO_HISTORY != 0,
    }
}

/// ActivityInfo.FLAG_NO_HISTORY (manifest android:noHistory).
const FLAG_NO_HISTORY: i32 = 0x0080;

fn named(c: &ComponentInfo, class: &str) -> bool {
    c.item.name.as_deref() == Some(class)
}

/// An intent filter with action MAIN and category LAUNCHER.
fn launches(i: &ParsedIntentInfo) -> bool {
    has(i, ACTION_MAIN, CATEGORY_LAUNCHER)
}

/// The filter lists `action` and `category`.
fn has(i: &ParsedIntentInfo, action: &str, category: &str) -> bool {
    i.intent_filter.as_ref().is_some_and(|f| {
        f.actions.iter().flatten().any(|a| a == action)
            && f.categories
                .iter()
                .flatten()
                .flatten()
                .any(|c| c == category)
    })
}

/// PMS's per-install fields (ScanPackageUtils / PackageInfoUtils.generateApplicationInfo for a non-system app on user 0): code and data paths, uid, native-library dirs; set on the package's
/// ApplicationInfo and on every component's copy (generate shares one object, PackageInfoCommonUtils.generate:235; the decode gives each component its own).
pub struct InstallFields<'a> {
    pub uid: u32,
    /// `/data/app/<package>`
    pub code_dir: &'a str,
    /// the instruction set's directory name under `<code>/lib`
    pub isa: &'a str,
    /// `/data/user/0/<package>`
    pub data_dir: &'a str,
    /// `/data/user_de/0/<package>`
    pub device_protected_data_dir: &'a str,
    /// The APK paths: PackageInstallerSession's uniform names below `code_dir` (an install), or the parse's own (a system scan: the partition's files keep their names).
    pub uniform_names: bool,
    /// ApplicationInfo.flags / privateFlags to add (the system scan's, by location: atl-services-types `system_packages`); 0 for an install.
    pub flags: i32,
    pub private_flags: i32,
}

impl InstallFields<'_> {
    pub fn apply(&self, info: &mut PackageInfo) {
        let set = |a: &mut ApplicationInfo| self.set(a);
        if let Some(a) = info.application_info.as_mut() {
            set(a);
        }
        let comps = info
            .activities
            .iter_mut()
            .chain(info.receivers.iter_mut())
            .flatten()
            .flatten()
            .map(|a| &mut a.component)
            .chain(
                info.services
                    .iter_mut()
                    .flatten()
                    .flatten()
                    .map(|s| &mut s.component),
            )
            .chain(
                info.providers
                    .iter_mut()
                    .flatten()
                    .flatten()
                    .map(|p| &mut p.component),
            );
        for c in comps {
            set(&mut c.application_info);
        }
        let app = info.application_info.clone().unwrap_or_default();
        for i in info.instrumentation.iter_mut().flatten().flatten() {
            // PackageInfoUtils.generateInstrumentationInfo copies these from the package
            i.source_dir = app.source_dir.clone();
            i.public_source_dir = app.public_source_dir.clone();
            i.split_names = app.split_names.clone();
            i.split_source_dirs = app.split_source_dirs.clone();
            i.split_public_source_dirs = app.split_public_source_dirs.clone();
            i.data_dir = app.data_dir.clone();
            i.device_protected_data_dir = app.device_protected_data_dir.clone();
            i.credential_protected_data_dir = app.credential_protected_data_dir.clone();
            i.primary_cpu_abi = app.primary_cpu_abi.clone();
            i.native_library_dir = app.native_library_dir.clone();
        }
    }

    fn set(&self, a: &mut ApplicationInfo) {
        let base = format!("{}/base.apk", self.code_dir);
        let splits = a.split_names.as_ref().map(|n| {
            n.iter()
                .map(|s| {
                    s.as_ref()
                        .map(|s| format!("{}/split_{s}.apk", self.code_dir))
                })
                .collect::<Vec<_>>()
        });
        a.uid = i32::try_from(self.uid).unwrap_or(i32::MAX);
        if self.uniform_names {
            a.source_dir = Some(base.clone());
            a.public_source_dir = Some(base);
            a.split_source_dirs = splits.clone();
            a.split_public_source_dirs = splits;
        } else {
            a.public_source_dir = a.source_dir.clone();
            a.split_public_source_dirs = a.split_source_dirs.clone();
        }
        a.flags |= self.flags;
        a.private_flags |= self.private_flags;
        // PackageAbiHelperImpl.deriveNativeLibraryPaths for a non-system app: root `<code>/lib`, dir `<root>/<isa>`, set with or without native code (LoadedApk.makePaths needs a non-null dir)
        let root = format!(
            "{}/{}",
            self.code_dir,
            atl_services_types::native_libs::LIB_DIR_NAME
        );
        a.native_library_dir = Some(format!("{root}/{}", self.isa));
        a.native_library_root_dir = Some(root);
        a.native_library_root_requires_isa = true;
        a.data_dir = Some(self.data_dir.to_owned());
        a.credential_protected_data_dir = Some(self.data_dir.to_owned());
        a.device_protected_data_dir = Some(self.device_protected_data_dir.to_owned());
    }
}

/// PackageInfo built directly for unit tests: expected values stay literal from the manifest text the test names, never from the installer.
#[cfg(test)]
pub mod fixture {
    use super::*;
    use atl_parcelables::android::content::IntentFilter;
    use atl_parcelables::parts::PackageItemInfo;

    pub fn component(package: &str, class: &str) -> ComponentInfo {
        ComponentInfo {
            item: PackageItemInfo {
                name: Some(class.to_owned()),
                package_name: Some(package.to_owned()),
                ..Default::default()
            },
            application_info: app(package),
            process_name: Some(package.to_owned()),
            enabled: true,
            ..Default::default()
        }
    }

    /// A current app (targetSdk 34: no pre-Honeycomb config-change bits).
    pub fn app(package: &str) -> ApplicationInfo {
        let mut a = ApplicationInfo {
            process_name: Some(package.to_owned()),
            target_sdk_version: 34,
            enabled: true,
            ..Default::default()
        };
        a.item.package_name = Some(package.to_owned());
        a
    }

    pub fn activity(package: &str, class: &str) -> ActivityInfo {
        ActivityInfo {
            component: component(package, class),
            ..Default::default()
        }
    }

    pub fn filter(actions: &[&str], categories: &[&str]) -> ParsedIntentInfo {
        let s = |v: &[&str]| v.iter().map(|a| Some((*a).to_owned())).collect::<Vec<_>>();
        ParsedIntentInfo {
            intent_filter: Some(IntentFilter {
                actions: s(actions),
                categories: Some(s(categories)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    pub fn record(package: &str) -> PackageRecord {
        PackageRecord {
            info: PackageInfo {
                package_name: Some(package.to_owned()),
                application_info: Some(app(package)),
                ..Default::default()
            },
            intents: Vec::new(),
            libraries: Vec::new(),
            force_queryable: false,
            queries: Queries::default(),
            protected_broadcasts: Vec::new(),
            uid: 10_001,
            instruction_set: "x86_64".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::package_state::{
        COMPONENT_ENABLED_STATE_DEFAULT, COMPONENT_ENABLED_STATE_DISABLED,
        COMPONENT_ENABLED_STATE_ENABLED,
    };
    use atl_parcelables::android::content::Intent;
    use atl_parcelables::android::os::Bundle;
    use atl_parcelables::parts::PackageItemInfo;
    use std::sync::Arc;

    /// `org.caller` (uid 10_002, targetSdk 34) with `edit` applied, against `target`: does the caller see it?
    fn caller_sees_target(edit: impl FnOnce(&mut PackageRecord), target: PackageRecord) -> bool {
        let mut caller = record("org.caller");
        caller.uid = 10_002;
        edit(&mut caller);
        Packages::new(vec![caller, target], BTreeMap::new())
            .visible(10_002, "org.target")
            .is_some()
    }

    /// `org.target` (uid 10_001, not force-queryable) with `edit` applied: its exported components, providers, permissions and instrumentation.
    fn target(edit: impl FnOnce(&mut PackageRecord)) -> PackageRecord {
        let mut r = record("org.target");
        edit(&mut r);
        r
    }

    /// A snapshot taken before a publish keeps the list it saw; one taken after sees the new list.
    #[test]
    fn snapshot_is_the_published_list() {
        let table = PackageTable::new(Packages::new(vec![record("a")], BTreeMap::new()));
        let before = table.snapshot();
        table.publish(
            Packages::new(vec![record("a"), record("b")], BTreeMap::new())
                .with_user_state_arc(table.user_state_arc()),
        );
        assert_eq!(before.all().len(), 1);
        assert!(before.by_name("b").is_none());
        assert_eq!(table.snapshot().all().len(), 2);
        assert!(table.snapshot().by_name("b").is_some());
    }

    /// publish's rule: the component state belongs to the table, so a list on another state would drop the state set so far; the panic names the rule.
    #[test]
    #[should_panic(expected = "must carry the table's user state")]
    fn publish_refuses_a_list_with_another_user_state() {
        let table = PackageTable::new(Packages::new(vec![record("a")], BTreeMap::new()));
        table.publish(Packages::new(
            vec![record("a"), record("b")],
            BTreeMap::new(),
        ));
    }

    /// The component state belongs to the table, not to one list: a state set through an old snapshot is read through a snapshot taken after the publish.
    #[test]
    fn component_state_is_shared_by_snapshots() {
        let table = PackageTable::new(Packages::new(vec![record("a")], BTreeMap::new()));
        let before = table.snapshot();
        let changed =
            before
                .user_state()
                .set_component("a", "a.C", COMPONENT_ENABLED_STATE_DISABLED);
        assert!(changed);
        table.publish(
            Packages::new(vec![record("a"), record("b")], BTreeMap::new())
                .with_user_state_arc(table.user_state_arc()),
        );
        let after = table.snapshot();
        assert_eq!(after.all().len(), 2);
        assert_eq!(
            after.user_state().component("a", "a.C"),
            COMPONENT_ENABLED_STATE_DISABLED
        );
    }

    /// AppsFilterUtils.canQueryViaPackage: `<queries><package>` names the package it may see; another name grants nothing.
    #[test]
    fn queries_package_names_a_visible_package() {
        let names = |n: &str| {
            caller_sees_target(|c| c.queries.packages = vec![n.to_owned()], target(|_| {}))
        };
        assert!(names("org.target"));
        assert!(!names("org.other"));
    }

    /// The billing shape (AppsFilterUtils.matchesAnyComponents): a `<queries><intent>` with the BIND action sees the target whose exported service filters that action. A non-exported service,
    /// or no query, does not.
    #[test]
    fn queries_intent_sees_the_exported_service_that_filters_its_action() {
        const BIND: &str = "com.android.vending.billing.InAppBillingService.BIND";
        let service = |exported: bool| {
            target(|t| {
                t.info.services = Some(vec![Some(ServiceInfo {
                    component: ComponentInfo {
                        exported,
                        ..component("org.target", "org.target.Billing")
                    },
                    ..Default::default()
                })]);
                t.intents.push(ComponentIntents {
                    kind: Kind::ServiceIntents,
                    name: "org.target.Billing".into(),
                    intents: vec![filter(&[BIND], &[])],
                });
            })
        };
        let bind = Queries {
            intents: vec![Intent {
                action: Some(BIND.into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(caller_sees_target(
            |c| c.queries = bind.clone(),
            service(true)
        ));
        assert!(!caller_sees_target(|_| {}, service(true)));
        assert!(!caller_sees_target(|c| c.queries = bind, service(false)));
    }

    /// AppsFilterUtils.matchesProviders: an exported provider's `;` authorities count one by one; a non-exported provider does not.
    #[test]
    fn queries_provider_names_an_exported_authority() {
        let provider = |exported: bool| {
            target(|t| {
                t.info.providers = Some(vec![Some(ProviderInfo {
                    component: ComponentInfo {
                        exported,
                        ..component("org.target", "org.target.P")
                    },
                    authority: Some("auth.one;auth.two".into()),
                    ..Default::default()
                })]);
            })
        };
        let asks = |authority: &str, exported: bool| {
            caller_sees_target(
                |c| c.queries.providers = vec![authority.to_owned()],
                provider(exported),
            )
        };
        assert!(asks("auth.two", true));
        assert!(!asks("auth.three", true));
        assert!(!asks("auth.two", false));
    }

    /// AppsFilterBase.shouldFilterApplicationInternal, the FILTER_APPLICATION_QUERY check (AppsFilterImpl.java:5684-5686, EnabledSince R): a caller targeting 29 sees every package, one targeting R does not.
    #[test]
    fn caller_below_r_sees_every_package() {
        let sees_with_target_sdk = |sdk: i32| {
            caller_sees_target(
                |c| {
                    c.info
                        .application_info
                        .as_mut()
                        .expect("app")
                        .target_sdk_version = sdk
                },
                target(|_| {}),
            )
        };
        assert!(sees_with_target_sdk(29));
        assert!(!sees_with_target_sdk(VERSION_CODES_R));
    }

    /// AppsFilterUtils.requestsQueryAllPackages: a caller requesting QUERY_ALL_PACKAGES sees every package.
    #[test]
    fn query_all_packages_sees_every_package() {
        let requests = Some(vec![Some(permission::QUERY_ALL_PACKAGES.to_owned())]);
        assert!(caller_sees_target(
            |c| c.info.requested_permissions = requests.clone(),
            target(|_| {})
        ));
        assert!(!caller_sees_target(|_| {}, target(|_| {})));
    }

    /// AppsFilterImpl.addPackageInternal:618-670 (mQueryableViaUsesPermission): a permission the caller requests and the target declares makes it visible; an undeclared one does not.
    #[test]
    fn requested_permission_the_target_declares_is_visible() {
        let declares = || {
            target(|t| {
                t.info.permissions = Some(vec![Some(PermissionInfo {
                    item: PackageItemInfo {
                        name: Some(CAMERA.into()),
                        ..Default::default()
                    },
                    ..Default::default()
                })]);
            })
        };
        let requests = |name: &str| {
            caller_sees_target(
                |c| c.info.requested_permissions = Some(vec![Some(name.into())]),
                declares(),
            )
        };
        assert!(requests(CAMERA));
        assert!(!requests(INTERNET));
    }

    /// AppsFilterImpl.pkgInstruments: an instrumentation that targets the other package makes the two visible to each other, from either side.
    #[test]
    fn instrumentation_makes_both_packages_visible() {
        let instrumenting = |package: &str| {
            Some(vec![Some(InstrumentationInfo {
                target_package: Some(package.into()),
                ..Default::default()
            })])
        };
        assert!(caller_sees_target(
            |_| {},
            target(|t| t.info.instrumentation = instrumenting("org.caller"))
        ));
        assert!(caller_sees_target(
            |c| c.info.instrumentation = instrumenting("org.target"),
            target(|_| {})
        ));
        assert!(!caller_sees_target(
            |_| {},
            target(|t| t.info.instrumentation = instrumenting("org.other"))
        ));
    }

    /// A shared uid pools its members' rules (AppsFilterBase.shouldFilterApplicationInternal, callingSharedPkgSettings): one member's `<queries>` is the uid's, another uid gets nothing.
    #[test]
    fn shared_uid_member_query_covers_the_uid() {
        let mut member = record("org.member");
        member.uid = 10_002;
        member.queries.packages = vec!["org.target".into()];
        let mut sibling = record("org.sibling");
        sibling.uid = 10_002;
        let packages = Packages::new(vec![sibling, member, target(|_| {})], BTreeMap::new());
        assert!(packages.visible(10_002, "org.target").is_some());
        assert!(packages.visible(10_003, "org.target").is_none());
    }

    /// AppsFilterBase.shouldFilterApplicationInternal:420-427: a target below the first application uid is never filtered, whoever asks.
    #[test]
    fn package_below_the_application_uids_is_visible() {
        let mut system = target(|_| {});
        system.uid = 1_000;
        assert!(caller_sees_target(|_| {}, system));
    }

    /// An app sees another package only when it is system and force-queryable (AppsFilterImpl:607-608): both conditions, each alone is not enough.
    #[test]
    fn force_queryable_system_package_is_visible_to_apps() {
        let app_uid = 10_002;
        for (system, force, seen) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
        ] {
            let mut r = record("org.provider");
            r.force_queryable = force;
            if let Some(a) = r.info.application_info.as_mut() {
                a.flags = if system { FLAG_SYSTEM } else { 0 };
            }
            let packages = Packages::new(vec![r], BTreeMap::new());
            assert_eq!(
                packages.visible(app_uid, "org.provider").is_some(),
                seen,
                "system {system} force {force}"
            );
        }
    }

    /// PackageManagerService.isProtectedBroadcast (:5526-5540): the platform's list, a (system) package's list as the install kept it, the hard-coded prefixes; null and others are not protected.
    #[test]
    fn protected_broadcasts_are_the_platforms_the_system_packages_and_the_prefixes() {
        let mut bt = record("com.android.bluetooth");
        bt.protected_broadcasts = vec!["android.bluetooth.adapter.action.STATE_CHANGED".into()];
        let packages = Packages::new(vec![bt, record("org.app")], BTreeMap::new())
            .with_platform_broadcasts(["android.intent.action.BATTERY_CHANGED".to_owned()]);
        for (action, want) in [
            (Some("android.intent.action.BATTERY_CHANGED"), true),
            (Some("android.bluetooth.adapter.action.STATE_CHANGED"), true),
            (Some("android.net.netmon.lingerExpired_7"), true),
            (Some("android.intent.action.BATTERY_CHANGED2"), false),
            (Some("org.app.PING"), false),
            (None, false),
        ] {
            assert_eq!(packages.is_protected_broadcast(action), want, "{action:?}");
        }
    }

    const INTERNET: &str = "android.permission.INTERNET";
    const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";
    const CAMERA: &str = "android.permission.CAMERA";

    /// One package `org.x` with `target_sdk` requesting INTERNET and POST_NOTIFICATIONS (AB2's two platform ones of each kind), against framework-res's levels as aapt2 prints them (INTERNET normal 0x0,
    /// POST_NOTIFICATIONS and CAMERA dangerous 0x1).
    fn permissions(target_sdk: i32) -> (Permissions, PackageRecord) {
        let mut r = record("org.x");
        r.info.requested_permissions =
            Some(vec![Some(INTERNET.into()), Some(POST_NOTIFICATIONS.into())]);
        if let Some(a) = r.info.application_info.as_mut() {
            a.target_sdk_version = target_sdk;
        }
        let platform =
            [(INTERNET, 0x0), (POST_NOTIFICATIONS, 0x1), (CAMERA, 0x1)].map(|(name, level)| {
                PermissionDecl {
                    name: name.into(),
                    protection_level: level,
                }
            });
        let packages = Arc::new(Packages::new(vec![r.clone()], BTreeMap::new()));
        (Permissions::new(&packages, &platform), r)
    }

    /// A package of the install-order tests: uid, targetSdk, FLAG_SYSTEM when `system`, its `<permission>`s (name, protectionLevel) and its `<uses-permission>`s.
    fn pkg(
        name: &str,
        uid: u32,
        sdk: i32,
        system: bool,
        defines: &[(&str, i32)],
        requests: &[&str],
    ) -> PackageRecord {
        let mut r = record(name);
        r.uid = uid;
        r.info.requested_permissions =
            Some(requests.iter().map(|p| Some((*p).to_owned())).collect());
        r.info.permissions = Some(
            defines
                .iter()
                .map(|&(n, level)| {
                    Some(PermissionInfo {
                        item: PackageItemInfo {
                            name: Some(n.to_owned()),
                            ..Default::default()
                        },
                        protection_level: level,
                        ..Default::default()
                    })
                })
                .collect(),
        );
        if let Some(a) = r.info.application_info.as_mut() {
            a.target_sdk_version = sdk;
            if system {
                a.flags |= FLAG_SYSTEM;
            }
        }
        r
    }

    /// S (uid 1000, system) defines normal `org.n` and requests normal `org.m`; A (10001) defines dangerous `org.d` and requests `org.m`; B (10002) defines normal `org.m` and requests
    /// dangerous `org.d`. targetSdk 30 for all: no literal below depends on S's.
    fn install_order_set() -> [PackageRecord; 3] {
        [
            pkg("org.s", 1000, 30, true, &[("org.n", 0)], &["org.m"]),
            pkg("org.a", 10_001, 30, false, &[("org.d", 1)], &["org.m"]),
            pkg("org.b", 10_002, 30, false, &[("org.m", 0)], &["org.d"]),
        ]
    }

    /// framework-res's one permission in these tests: `org.p0`, normal.
    fn platform_decls() -> [PermissionDecl; 1] {
        [PermissionDecl {
            name: "org.p0".into(),
            protection_level: 0,
        }]
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| (*n).to_owned()).collect()
    }

    /// (granted, flags, runtime_grant) of `name` for `uid` as the services read it; no state reads as (false, 0, None).
    fn view(p: &Permissions, uid: u32, name: &str) -> (bool, u32, Option<bool>) {
        (
            p.granted(uid, name),
            p.flags(uid, name),
            p.runtime_grant(uid, name),
        )
    }

    /// One batch of all three on an empty table equals the start-with-all `Permissions::new` of the same list: same definitions, same states, and the literal states. Step 3 never
    /// runs here (no package outside the batch); `a_definition_after_its_requester_is_not_granted` covers it.
    #[test]
    fn packages_added_equals_a_start_with_all() {
        let all = Packages::new(install_order_set().to_vec(), BTreeMap::new());
        let start = Permissions::new(&all, &platform_decls());
        let batch = Permissions::new(&Packages::new(vec![], BTreeMap::new()), &platform_decls());
        batch.packages_added(&all, &names(&["org.s", "org.a", "org.b"]));
        assert_eq!(batch.table().states, start.table().states);
        assert_eq!(batch.table().registry, start.table().registry);
        assert_eq!(view(&batch, 1000, "org.n"), (false, 0, None));
        assert_eq!(view(&batch, 1000, "org.m"), (true, 0, None));
        assert_eq!(view(&batch, 1000, "org.d"), (false, 0, None));
        assert_eq!(view(&batch, 10_001, "org.n"), (false, 0, None));
        assert_eq!(view(&batch, 10_001, "org.m"), (true, 0, None));
        assert_eq!(view(&batch, 10_001, "org.d"), (false, 0, None));
        assert_eq!(view(&batch, 10_002, "org.n"), (false, 0, None));
        assert_eq!(view(&batch, 10_002, "org.m"), (false, 0, None));
        assert_eq!(view(&batch, 10_002, "org.d"), (false, 0, Some(false)));
    }

    /// A permission defined after its requester was installed: batch 1 is S and A (`org.m` not yet defined), batch 2 is B, which defines it. The system requester gets the normal
    /// permission; the non-system one does not (AOSP's INSTALL_REVOKED); B's own dangerous request is denied as a start denies it.
    #[test]
    fn a_definition_after_its_requester_is_not_granted() {
        let [s, a, b] = install_order_set();
        let table = Permissions::new(&Packages::new(vec![], BTreeMap::new()), &platform_decls());
        let first = Packages::new(vec![s.clone(), a.clone()], BTreeMap::new());
        table.packages_added(&first, &names(&["org.s", "org.a"]));
        let second = Packages::new(vec![s, a, b], BTreeMap::new());
        table.packages_added(&second, &names(&["org.b"]));
        assert_eq!(view(&table, 1000, "org.m"), (true, 0, None));
        assert_eq!(view(&table, 10_001, "org.m"), (false, 0, None));
        assert_eq!(view(&table, 10_002, "org.d"), (false, 0, Some(false)));
    }

    /// An update keeps the runtime grants its requests still have (a kept request keeps its state, flags included). A guard: it passes before the change; a rebuild of the states from
    /// initial_states fails it.
    #[test]
    fn an_update_keeps_runtime_grants() {
        let v1 = pkg("org.a", 10_001, 30, false, &[("org.d", 1)], &["org.d"]);
        let v2 = pkg("org.a", 10_001, 30, false, &[("org.d", 1)], &["org.d"]);
        let p = Permissions::new(
            &Packages::new(vec![v1.clone()], BTreeMap::new()),
            &platform_decls(),
        );
        p.grant_runtime(&v1, "org.d").expect("requested");
        p.packages_added(
            &Packages::new(vec![v2], BTreeMap::new()),
            &names(&["org.a"]),
        );
        assert_eq!(view(&p, 10_001, "org.d"), (true, 0, Some(true)));
    }

    /// An update that stops requesting a runtime permission drops its state at the app id (trimPermissionStates, AOSP AppIdPermissionPolicy.kt :682): the grant goes with it.
    #[test]
    fn a_dropped_request_loses_its_grant() {
        let v1 = pkg(
            "org.a",
            10_001,
            30,
            false,
            &[("org.d", 1)],
            &["org.d", "org.p0"],
        );
        let v2 = pkg("org.a", 10_001, 30, false, &[("org.d", 1)], &["org.p0"]);
        let p = Permissions::new(
            &Packages::new(vec![v1.clone()], BTreeMap::new()),
            &platform_decls(),
        );
        p.grant_runtime(&v1, "org.d").expect("requested");
        p.packages_added(
            &Packages::new(vec![v2], BTreeMap::new()),
            &names(&["org.a"]),
        );
        assert_eq!(view(&p, 10_001, "org.d"), (false, 0, None));
        assert_eq!(view(&p, 10_001, "org.p0"), (true, 0, None));
    }

    /// A package that stops declaring its permission takes its state from every app id (trimPermissions :600-662): another app's grant of it goes too.
    #[test]
    fn a_removed_definition_drops_every_state() {
        let a1 = pkg("org.a", 10_001, 30, false, &[("org.d", 1)], &["org.d"]);
        let b = pkg("org.b", 10_002, 30, false, &[], &["org.d"]);
        let a2 = pkg("org.a", 10_001, 30, false, &[], &[]);
        let p = Permissions::new(
            &Packages::new(vec![a1.clone(), b.clone()], BTreeMap::new()),
            &platform_decls(),
        );
        p.grant_runtime(&a1, "org.d").expect("requested");
        p.grant_runtime(&b, "org.d").expect("requested");
        p.packages_added(
            &Packages::new(vec![a2, b], BTreeMap::new()),
            &names(&["org.a"]),
        );
        assert_eq!(view(&p, 10_001, "org.d"), (false, 0, None));
        assert_eq!(view(&p, 10_002, "org.d"), (false, 0, None));
        assert!(!p.is_runtime_or_app_op("org.d"));
    }

    /// A definition that becomes runtime drops every app id's state of it (addPermissions :520-551); its requesters then hold the state of a new runtime permission: not granted.
    #[test]
    fn a_definition_that_becomes_dangerous_drops_every_state() {
        let a1 = pkg("org.a", 10_001, 30, false, &[("org.n", 0)], &["org.n"]);
        let b = pkg("org.b", 10_002, 30, false, &[], &["org.n"]);
        let a2 = pkg("org.a", 10_001, 30, false, &[("org.n", 1)], &["org.n"]);
        let p = Permissions::new(
            &Packages::new(vec![a1, b.clone()], BTreeMap::new()),
            &platform_decls(),
        );
        assert_eq!(view(&p, 10_002, "org.n"), (true, 0, None));
        p.packages_added(
            &Packages::new(vec![a2, b], BTreeMap::new()),
            &names(&["org.a"]),
        );
        assert_eq!(view(&p, 10_001, "org.n"), (false, 0, Some(false)));
        assert_eq!(view(&p, 10_002, "org.n"), (false, 0, Some(false)));
    }

    /// A definition that changes its protection from NORMAL to SIGNATURE re-evaluates every requester (AppIdPermissionPolicy.kt :144-146): the unrelated requester `b` loses its
    /// install grant (a non-owner gets no signature state, initial_states), and the definer keeps its grant (same package, same certificate). Before the change, `or_insert` kept the NORMAL state.
    #[test]
    fn a_normal_definition_that_becomes_signature_drops_an_unrelated_grant() {
        let a1 = pkg("org.a", 10_001, 30, false, &[("org.n", 0)], &["org.n"]);
        let b = pkg("org.b", 10_002, 30, false, &[], &["org.n"]);
        let a2 = pkg("org.a", 10_001, 30, false, &[("org.n", 2)], &["org.n"]);
        let p = Permissions::new(
            &Packages::new(vec![a1, b.clone()], BTreeMap::new()),
            &platform_decls(),
        );
        assert_eq!(view(&p, 10_002, "org.n"), (true, 0, None));
        p.packages_added(
            &Packages::new(vec![a2, b], BTreeMap::new()),
            &names(&["org.a"]),
        );
        assert_eq!(view(&p, 10_002, "org.n"), (false, 0, None));
        assert_eq!(view(&p, 10_001, "org.n"), (true, 0, None));
    }

    /// A legacy app's runtime grant of a platform permission carries REVIEW_REQUIRED (AOSP's LEGACY_GRANTED + IMPLICIT, API view). A targetSdk >= M update does not keep that grant
    /// (evaluatePermissionState :1103-1110); a grant of the app's own permission (no flags) is kept.
    #[test]
    fn a_review_required_grant_does_not_carry_into_a_modern_update() {
        let platform = [
            PermissionDecl {
                name: "org.p0".into(),
                protection_level: 0,
            },
            PermissionDecl {
                name: "org.pd".into(),
                protection_level: 1,
            },
        ];
        let v1 = pkg(
            "org.a",
            10_001,
            22,
            false,
            &[("org.d", 1)],
            &["org.pd", "org.d"],
        );
        let v2 = pkg(
            "org.a",
            10_001,
            36,
            false,
            &[("org.d", 1)],
            &["org.pd", "org.d"],
        );
        let p = Permissions::new(&Packages::new(vec![v1], BTreeMap::new()), &platform);
        assert_eq!(view(&p, 10_001, "org.pd"), (true, 0x48, Some(true)));
        assert_eq!(view(&p, 10_001, "org.d"), (true, 0, Some(true)));
        p.packages_added(
            &Packages::new(vec![v2], BTreeMap::new()),
            &names(&["org.a"]),
        );
        assert_eq!(view(&p, 10_001, "org.pd"), (false, 0, Some(false)));
        assert_eq!(view(&p, 10_001, "org.d"), (true, 0, Some(true)));
    }

    /// targetSdk >= M: the runtime permission starts denied and grantRuntimePermission grants it; the install permission is granted and not changeable; grantRuntimePermissionInternal's refusals
    /// (unknown permission, not requested) keep the state as it was.
    #[test]
    fn runtime_permission_is_denied_until_granted() {
        let (p, r) = permissions(36);
        assert!(p.granted(r.uid, INTERNET));
        assert!(!p.granted(r.uid, POST_NOTIFICATIONS));
        assert_eq!(p.flags(r.uid, POST_NOTIFICATIONS), 0);
        assert!(matches!(
            p.grant_runtime(&r, INTERNET),
            Err(GrantError::NotChangeable { .. })
        ));
        assert!(matches!(
            p.grant_runtime(&r, "org.z.UNDEFINED"),
            Err(GrantError::UnknownPermission(_))
        ));
        assert!(matches!(
            p.grant_runtime(&r, CAMERA),
            Err(GrantError::NotRequested { .. })
        ));
        assert!(!p.granted(r.uid, CAMERA));
        assert_eq!(p.grant_runtime(&r, POST_NOTIFICATIONS), Ok(()));
        assert!(p.granted(r.uid, POST_NOTIFICATIONS));
        assert_eq!(p.granted_names(r.uid), [INTERNET, POST_NOTIFICATIONS]);
    }

    /// targetSdk < M: the platform runtime permission is granted at install with REVIEW_REQUIRED | REVOKED_COMPAT (0x40 | 0x8), and a grant leaves it so.
    #[test]
    fn legacy_runtime_permission_is_granted_for_review() {
        let (p, r) = permissions(22);
        assert!(p.granted(r.uid, POST_NOTIFICATIONS));
        assert_eq!(p.flags(r.uid, POST_NOTIFICATIONS), 0x48);
        assert_eq!(p.grant_runtime(&r, POST_NOTIFICATIONS), Ok(()));
        assert_eq!(p.flags(r.uid, POST_NOTIFICATIONS), 0x48);
    }

    const MAIN: &[&str] = &[ACTION_MAIN];
    const LAUNCHER: &[&str] = &[CATEGORY_LAUNCHER];

    /// fossify-notes' manifest (aapt2 xmltree of the row's APK): `SplashActivity` has no launcher filter; the first launcher entry is the enabled alias `SplashActivity.Green` (targetActivity
    /// SplashActivity), other colour aliases follow disabled.
    #[test]
    fn launcher_is_the_first_enabled_alias_or_activity_with_main_launcher() {
        let pkg = "org.fossify.notes";
        let mut r = record(pkg);
        let splash = activity(pkg, "org.fossify.notes.activities.SplashActivity");
        let mut red = activity(pkg, "org.fossify.notes.activities.SplashActivity.Red");
        red.component.enabled = false;
        red.target_activity = splash.component.item.name.clone();
        let mut green = activity(pkg, "org.fossify.notes.activities.SplashActivity.Green");
        green.target_activity = splash.component.item.name.clone();
        r.info.activities = Some(vec![Some(splash), Some(red), Some(green)]);
        for n in ["SplashActivity.Red", "SplashActivity.Green"] {
            r.intents.push(ComponentIntents {
                kind: Kind::ActivityIntents,
                name: format!("org.fossify.notes.activities.{n}"),
                intents: vec![filter(MAIN, LAUNCHER)],
            });
        }
        let packages = Packages::default();
        let l = packages.launcher(&r).expect("launcher");
        assert_eq!(
            l.component.item.name.as_deref(),
            Some("org.fossify.notes.activities.SplashActivity.Green")
        );
        assert_eq!(
            l.target_activity.as_deref(),
            Some("org.fossify.notes.activities.SplashActivity")
        );
        // the user state: Green disabled -> no launcher (Red is disabled in the manifest); Red enabled -> Red, the first in manifest order; DEFAULT on both -> Green again
        let state = packages.user_state();
        let (red, green) = (
            "org.fossify.notes.activities.SplashActivity.Red",
            "org.fossify.notes.activities.SplashActivity.Green",
        );
        state.set_component(pkg, green, COMPONENT_ENABLED_STATE_DISABLED);
        assert!(packages.launcher(&r).is_none());
        state.set_component(pkg, red, COMPONENT_ENABLED_STATE_ENABLED);
        assert_eq!(
            packages
                .launcher(&r)
                .and_then(|l| l.component.item.name.as_deref()),
            Some(red)
        );
        state.set_component(pkg, red, COMPONENT_ENABLED_STATE_DEFAULT);
        state.set_component(pkg, green, COMPONENT_ENABLED_STATE_DEFAULT);
        assert_eq!(
            packages
                .launcher(&r)
                .and_then(|l| l.component.item.name.as_deref()),
            Some(green)
        );
        // break: without the filters nothing launches
        r.intents.clear();
        assert!(packages.launcher(&r).is_none());
    }

    /// getRequiredPermissionControllerLPr: exactly one system package with an enabled MANAGE_PERMISSIONS + DEFAULT activity, privileged. PermissionController's manifest declares that filter
    /// on ManagePermissionsActivity (packages/modules/Permission/PermissionController AndroidManifest.xml).
    #[test]
    fn permission_controller_is_the_one_privileged_system_manage_permissions_package() {
        let pkg = "com.android.permissioncontroller";
        let class = "com.android.permissioncontroller.permission.ui.ManagePermissionsActivity";
        let controller = |flags: i32, private_flags: i32| {
            let mut r = record(pkg);
            r.info.activities = Some(vec![Some(activity(pkg, class))]);
            r.intents.push(ComponentIntents {
                kind: Kind::ActivityIntents,
                name: class.to_owned(),
                intents: vec![filter(&[ACTION_MANAGE_PERMISSIONS], &[CATEGORY_DEFAULT])],
            });
            if let Some(a) = r.info.application_info.as_mut() {
                a.flags = flags;
                a.private_flags = private_flags;
            }
            r
        };
        let of = |list: Vec<PackageRecord>| {
            Packages::new(list, BTreeMap::new())
                .permission_controller()
                .map(str::to_owned)
        };
        assert_eq!(
            of(vec![
                record("org.x"),
                controller(FLAG_SYSTEM, PRIVATE_FLAG_PRIVILEGED)
            ])
            .as_deref(),
            Some(pkg)
        );
        // what every install has today: no system package -> none
        assert_eq!(
            of(vec![
                record("org.x"),
                controller(0, PRIVATE_FLAG_PRIVILEGED)
            ]),
            None
        );
        assert_eq!(of(vec![controller(FLAG_SYSTEM, 0)]), None);
        let mut second = controller(FLAG_SYSTEM, PRIVATE_FLAG_PRIVILEGED);
        second.info.package_name = Some("org.other".into());
        assert_eq!(
            of(vec![
                controller(FLAG_SYSTEM, PRIVATE_FLAG_PRIVILEGED),
                second
            ]),
            None
        );
    }

    /// Temple Run's base (aapt2 xmltree, row `temple-run`): `com.google.firebase.components.ComponentDiscoveryService` exported=false directBootAware=true, whose meta-data lists the Firebase
    /// registrars, each with the value `com.google.firebase.components.ComponentRegistrar` (the first `...analytics.connector.internal.AnalyticsConnectorRegistrar`, among them
    /// `...crashlytics.CrashlyticsRegistrar`).
    #[test]
    fn service_info_keeps_meta_data_only_with_get_meta_data() {
        let pkg = "com.imangi.templerun";
        let class = "com.google.firebase.components.ComponentDiscoveryService";
        let registrar = "com.google.firebase.components.ComponentRegistrar";
        let keys = [
            "com.google.firebase.components:com.google.firebase.analytics.connector.internal.AnalyticsConnectorRegistrar",
            "com.google.firebase.components:com.google.firebase.crashlytics.CrashlyticsRegistrar",
        ];
        let mut r = record(pkg);
        let mut c = component(pkg, class);
        c.direct_boot_aware = true;
        c.item.meta_data = Some(Bundle::from_strings(&[
            (keys[0], registrar),
            (keys[1], registrar),
        ]));
        r.info.services = Some(vec![Some(ServiceInfo {
            component: c,
            ..Default::default()
        })]);
        let packages = Packages::default();
        let with = packages
            .service_info(&r, class, GET_META_DATA)
            .expect("declared");
        assert!(with.component.direct_boot_aware && !with.component.exported);
        let entries = with
            .component
            .item
            .meta_data
            .expect("meta-data")
            .entries()
            .expect("entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, keys[0]);
        let without = packages.service_info(&r, class, 0).expect("declared");
        assert!(without.component.item.meta_data.is_none());
        // break: a class the package does not declare is not found by name
        assert!(
            packages
                .service_info(&r, "com.google.firebase.components.Other", GET_META_DATA)
                .is_none()
        );
    }

    /// altos-odyssey's `androidx.work.impl.background.systemalarm.ConstraintProxyUpdateReceiver` is `enabled=false` (aapt2 xmltree, a resource the parse resolves to false): filtered without
    /// MATCH_DISABLED_COMPONENTS, answered with it.
    #[test]
    fn disabled_component_needs_match_disabled_components() {
        let mut c = component(
            "com.noodlecake.altosodyssey",
            "androidx.work.impl.background.systemalarm.ConstraintProxyUpdateReceiver",
        );
        c.enabled = false;
        let packages = Packages::default();
        assert!(!packages.answer(&mut c.clone(), 0));
        assert!(packages.answer(&mut c.clone(), MATCH_DISABLED_COMPONENTS));
        // setComponentEnabledSetting ENABLED overrides the manifest (PackageUserStateUtils.isEnabled: an explicit state before the manifest default); DEFAULT restores it
        let (package, class) = (
            "com.noodlecake.altosodyssey",
            "androidx.work.impl.background.systemalarm.ConstraintProxyUpdateReceiver",
        );
        let state = packages.user_state();
        state.set_component(package, class, COMPONENT_ENABLED_STATE_ENABLED);
        assert!(packages.answer(&mut c.clone(), 0));
        state.set_component(package, class, COMPONENT_ENABLED_STATE_DEFAULT);
        assert!(!packages.answer(&mut c.clone(), 0));
        // an enabled component disabled by the user state: filtered, answered with MATCH_DISABLED_COMPONENTS
        c.enabled = true;
        state.set_component(package, class, COMPONENT_ENABLED_STATE_DISABLED);
        assert!(!packages.answer(&mut c.clone(), 0));
        assert!(packages.answer(&mut c.clone(), MATCH_DISABLED_COMPONENTS));
        // a disabled `<application>` disables its components whatever their state
        state.set_component(package, class, COMPONENT_ENABLED_STATE_ENABLED);
        c.application_info.enabled = false;
        assert!(!packages.answer(&mut c, 0));
    }

    /// PMS's paths for package `a` with split `config.x86_64` installed on x86_64 (literal: `<code>/base.apk`, `split_<name>.apk`, `lib/<isa>`), on the app and a component's copy. The roots are arbitrary inputs, not the device layout.
    #[test]
    fn install_fields_reach_every_copy() {
        let mut r = record("a");
        r.info.application_info.as_mut().expect("app").split_names =
            Some(vec![Some("config.x86_64".into())]);
        let mut act = activity("a", "a.Main");
        act.component.application_info.split_names = Some(vec![Some("config.x86_64".into())]);
        r.info.activities = Some(vec![Some(act)]);
        InstallFields {
            uid: 10_005,
            code_dir: "/code/a",
            isa: "x86_64",
            data_dir: "/ce/a",
            device_protected_data_dir: "/de/a",
            uniform_names: true,
            flags: 0,
            private_flags: 0,
        }
        .apply(&mut r.info);
        for a in [
            r.app(),
            &r.activity("a.Main")
                .expect("activity")
                .component
                .application_info,
        ] {
            assert_eq!(a.uid, 10_005);
            assert_eq!(a.source_dir.as_deref(), Some("/code/a/base.apk"));
            assert_eq!(
                a.split_source_dirs,
                Some(vec![Some("/code/a/split_config.x86_64.apk".to_owned())])
            );
            assert_eq!(a.native_library_dir.as_deref(), Some("/code/a/lib/x86_64"));
            assert_eq!(a.native_library_root_dir.as_deref(), Some("/code/a/lib"));
            assert_eq!(a.device_protected_data_dir.as_deref(), Some("/de/a"));
        }
    }

    /// A system scan's fields: the parse's APK path kept (no uniform rename), the location's flags added to the parse's own on the app and a component's copy.
    #[test]
    fn scan_fields_keep_the_parsed_path_and_add_flags() {
        let mut r = record("a");
        let parsed = Some("/part/app/A/A.apk".to_owned());
        r.info.application_info.as_mut().expect("app").source_dir = parsed.clone();
        r.info.application_info.as_mut().expect("app").flags = 1 << 2;
        let mut act = activity("a", "a.Main");
        act.component.application_info.source_dir = parsed.clone();
        r.info.activities = Some(vec![Some(act)]);
        InstallFields {
            uid: 10_001,
            code_dir: "/part/app/A",
            isa: "x86_64",
            data_dir: "/ce/a",
            device_protected_data_dir: "/de/a",
            uniform_names: false,
            flags: FLAG_SYSTEM,
            private_flags: PRIVATE_FLAG_PRIVILEGED,
        }
        .apply(&mut r.info);
        for a in [
            r.app(),
            &r.activity("a.Main")
                .expect("activity")
                .component
                .application_info,
        ] {
            assert_eq!(a.source_dir, parsed);
            assert_eq!(a.public_source_dir, parsed);
            // FLAG_SYSTEM = 1<<0, PRIVATE_FLAG_PRIVILEGED = 1<<3 (ApplicationInfo.java:189, :535)
            assert_eq!(a.flags & 1, 1);
            assert_eq!(a.private_flags & (1 << 3), 1 << 3);
            assert_eq!(
                a.native_library_dir.as_deref(),
                Some("/part/app/A/lib/x86_64")
            );
        }
        // the parse's own flag (1<<2) is kept beside the added one
        assert_eq!(r.app().flags, (1 << 2) | 1);
    }
}
