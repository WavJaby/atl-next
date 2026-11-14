// SPDX-License-Identifier: MIT
//! One installed package as the services hold it (owner option I): the installer's PackageInfo (PackageInfoCommonUtils.generate over AOSP's own parse) with PMS's per-install fields set on it once
//! ([`InstallFields::apply`]), the intent filters and `<uses-library>` lists PackageInfo does not carry (installer frames), and the install's uid, instruction set, grants and GL driver.
//! Every IPackageManager answer is a clone of a stored info, filtered by the caller's flags ([`answer`]): nothing is re-derived from the manifest.

use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, InstrumentationInfo, PackageInfo, PermissionInfo, ProviderInfo,
    ServiceInfo,
};
use std::collections::BTreeMap;
use std::sync::Mutex;

use atl_parcelables::parts::{ComponentInfo, ParsedIntentInfo};
use atl_services_types::gl_driver::GlDriver;
use atl_services_types::installer_output::Kind;
use atl_services_types::packages::{Library, LibraryNode, PermissionDecl, used_libraries};
use atl_services_types::permission::{self, PermissionState};

use crate::activity_start::Declared;
use crate::desktop_bounds::{ActivityInput, ActivityOrientation};
use crate::installed::{ComponentIntents, UsesLibrary};

/// PackageManager.GET_META_DATA and MATCH_DISABLED_COMPONENTS (PackageManager.java).
pub const GET_META_DATA: i64 = 0x80;
pub const MATCH_DISABLED_COMPONENTS: i64 = 0x200;
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";
const ACTION_MANAGE_PERMISSIONS: &str = "android.intent.action.MANAGE_PERMISSIONS";
const CATEGORY_DEFAULT: &str = "android.intent.category.DEFAULT";
pub(crate) use atl_services_types::system_packages::{FLAG_SYSTEM, PRIVATE_FLAG_PRIVILEGED};

/// One installed package.
#[derive(Debug, Clone)]
pub struct PackageRecord {
    /// generate's PackageInfo with every GET_* the installer asks for (INFO_FLAGS), PMS's per-install fields applied
    pub info: PackageInfo,
    pub intents: Vec<ComponentIntents>,
    pub libraries: Vec<UsesLibrary>,
    pub uid: u32,
    /// The instruction set the process runs (arches.toml row key): primaryCpuAbi's, else the device's first (InstructionSets.getPrimaryInstructionSet); zygote's `--instruction-set`.
    pub instruction_set: String,
    pub gl_driver: GlDriver,
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

    /// The launcher entry: the first enabled activity or alias, in manifest order, with a MAIN + LAUNCHER filter (PMS queryIntentActivities' order for the launcher's query; a disabled
    /// component matches no query). The caller starts its `targetActivity` when set (an alias), keeping the alias as the component name.
    pub fn launcher(&self) -> Option<&ActivityInfo> {
        self.activities().filter(|a| a.component.enabled).find(|a| {
            let name = a.component.item.name.as_deref().unwrap_or_default();
            self.intents(Kind::ActivityIntents, name)
                .iter()
                .any(launches)
        })
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

    /// getServiceInfo's answer (ComputerEngine.getServiceInfo): the stored ServiceInfo filtered by `flags` ([`answer`]); None for a class the package does not declare or a disabled one without
    /// MATCH_DISABLED_COMPONENTS.
    pub fn service_info(&self, class: &str, flags: i64) -> Option<ServiceInfo> {
        let s = self.service(class)?;
        let mut out = s.clone();
        answer(&mut out.component, flags).then_some(out)
    }
}

/// The installed packages and the shared libraries (SystemConfig `<library>`) they may use; one user (0).
#[derive(Debug, Clone, Default)]
pub struct Packages {
    list: Vec<PackageRecord>,
    libraries: BTreeMap<String, Library>,
}

impl Packages {
    pub fn new(list: Vec<PackageRecord>, libraries: BTreeMap<String, Library>) -> Self {
        Packages { list, libraries }
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
                    .filter(|a| a.component.enabled)
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

    /// isSameApp (PackageManagerInternal): `package` runs under `uid`'s app id, shared uids included.
    pub fn is_same_app(&self, package: &str, uid: u32) -> bool {
        self.by_app_id(permission::app_id(uid))
            .any(|p| p.name() == package)
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

    /// What `caller` may see of `package` (ComputerEngine shouldFilterApplication): a uid below the first application uid everything (AppsFilterImpl.shouldFilterApplication does not filter
    /// them), an app its own package only (no <queries> yet).
    pub fn visible(&self, caller: u32, package: &str) -> Option<&PackageRecord> {
        let p = self.by_name(package)?;
        let own = permission::app_id(p.uid) == permission::app_id(caller);
        (permission::sees_all_packages(caller) || own).then_some(p)
    }
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
pub struct Permissions {
    registry: permission::Registry,
    /// app id -> its permission states, by name
    states: Mutex<BTreeMap<u32, BTreeMap<String, PermissionState>>>,
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
            registry,
            states: Mutex::new(states),
        }
    }

    fn states(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<u32, BTreeMap<String, PermissionState>>> {
        self.states.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn state(&self, uid: u32, p: &str) -> Option<PermissionState> {
        self.states()
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
        self.states()
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
        let definition = self
            .registry
            .get(p)
            .ok_or_else(|| GrantError::UnknownPermission(p.to_owned()))?;
        if !definition.is_runtime() {
            return Err(GrantError::NotChangeable {
                permission: p.to_owned(),
                package: package.name().to_owned(),
            });
        }
        let mut states = self.states();
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

    /// `permission::holds` against `uid`'s grants.
    pub fn holds(&self, uid: u32, permissions: &[&str], any: bool) -> bool {
        permission::holds(uid, permissions, any, |p| self.granted(uid, p))
    }

    /// `permission::holds` of one permission against `uid`'s grants; a null permission is not held.
    pub fn check(&self, uid: u32, p: Option<&str>) -> bool {
        p.is_some_and(|p| self.holds(uid, &[p], false))
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
    }
}

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

/// A stored component cut to the caller's `flags` (PackageInfoUtils.generate*Info): meta-data only with GET_META_DATA (the component's and its ApplicationInfo's); false = filtered out, a disabled
/// component without MATCH_DISABLED_COMPONENTS (PackageStateUtils.isEnabledAndMatches, reduced to the component's own state).
pub fn answer(c: &mut ComponentInfo, flags: i64) -> bool {
    if !c.enabled && flags & MATCH_DISABLED_COMPONENTS == 0 {
        return false;
    }
    if flags & GET_META_DATA == 0 {
        c.item.meta_data = None;
        c.application_info.item.meta_data = None;
    }
    true
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
            uid: 10_001,
            instruction_set: "x86_64".to_owned(),
            gl_driver: GlDriver::Native,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use atl_parcelables::android::os::Bundle;
    use std::sync::Arc;

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
        let l = r.launcher().expect("launcher");
        assert_eq!(
            l.component.item.name.as_deref(),
            Some("org.fossify.notes.activities.SplashActivity.Green")
        );
        assert_eq!(
            l.target_activity.as_deref(),
            Some("org.fossify.notes.activities.SplashActivity")
        );
        // break: without the filters nothing launches
        r.intents.clear();
        assert!(r.launcher().is_none());
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
        let with = r.service_info(class, GET_META_DATA).expect("declared");
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
        let without = r.service_info(class, 0).expect("declared");
        assert!(without.component.item.meta_data.is_none());
        // break: a class the package does not declare is not found by name
        assert!(
            r.service_info("com.google.firebase.components.Other", GET_META_DATA)
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
        assert!(!answer(&mut c.clone(), 0));
        assert!(answer(&mut c, MATCH_DISABLED_COMPONENTS));
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
