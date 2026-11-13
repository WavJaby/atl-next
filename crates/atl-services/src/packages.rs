// SPDX-License-Identifier: MIT
//! One installed package as the services hold it (owner option I): the installer's PackageInfo (PackageInfoCommonUtils.generate over AOSP's own parse) with PMS's per-install fields set on it once
//! ([`InstallFields::apply`]), the intent filters and `<uses-library>` lists PackageInfo does not carry (installer frames), and the install's uid, instruction set, grants and GL driver.
//! Every IPackageManager answer is a clone of a stored info, filtered by the caller's flags ([`answer`]): nothing is re-derived from the manifest.

use atl_parcelables::android::content::pm::{
    ActivityInfo, ApplicationInfo, InstrumentationInfo, PackageInfo, PermissionInfo, ProviderInfo,
    ServiceInfo,
};
use std::collections::BTreeMap;
use std::sync::Arc;

use atl_parcelables::parts::{ComponentInfo, ParsedIntentInfo};
use atl_services_types::gl_driver::GlDriver;
use atl_services_types::installer_output::Kind;
use atl_services_types::packages::{Library, LibraryNode, used_libraries};
use atl_services_types::permission;

use crate::activity_start::Declared;
use crate::desktop_bounds::{ActivityInput, ActivityOrientation};
use crate::installed::{ComponentIntents, UsesLibrary};

/// PackageManager.GET_META_DATA and MATCH_DISABLED_COMPONENTS (PackageManager.java).
pub const GET_META_DATA: i64 = 0x80;
pub const MATCH_DISABLED_COMPONENTS: i64 = 0x200;
const ACTION_MAIN: &str = "android.intent.action.MAIN";
const CATEGORY_LAUNCHER: &str = "android.intent.category.LAUNCHER";

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
    /// The install-time grants (permission::install_grants): what checkPermission answers GRANTED for this uid.
    pub granted_permissions: Vec<String>,
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

/// The permission state every service consults: the install-time grants of the installed packages, by uid (a shared uid's packages pool their grants, as PMS's shared user does).
pub struct Permissions {
    packages: Arc<Packages>,
}

impl Permissions {
    pub fn new(packages: Arc<Packages>) -> Self {
        Permissions { packages }
    }

    fn granted(&self, uid: u32, p: &str) -> bool {
        self.packages
            .by_app_id(permission::app_id(uid))
            .any(|r| r.granted_permissions.iter().any(|g| g == p))
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
    i.intent_filter.as_ref().is_some_and(|f| {
        f.actions.iter().flatten().any(|a| a == ACTION_MAIN)
            && f.categories
                .iter()
                .flatten()
                .flatten()
                .any(|c| c == CATEGORY_LAUNCHER)
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
        a.source_dir = Some(base.clone());
        a.public_source_dir = Some(base);
        a.split_source_dirs = splits.clone();
        a.split_public_source_dirs = splits;
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
            granted_permissions: Vec::new(),
            gl_driver: GlDriver::Native,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use atl_parcelables::android::os::Bundle;

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
}
