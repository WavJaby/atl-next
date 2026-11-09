// SPDX-License-Identifier: MIT
//! The installed packages the services know. Given by the host (atld's install step, from the manifests `xtask` read with AOSP's `aapt2 dump xmltree`; a libandroidfw reader replaces that for real installs, owner D3); one user (0).
//! A package is its [`Manifest`] (what the APK declares) plus where the host installed it ([`PackageRecord`]).

use std::collections::BTreeMap;
use std::ops::Deref;

use serde::{Deserialize, Serialize};

/// `<uses-library android:name android:required>` (AndroidManifest.xml; `required` defaults to true).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsesLibrary {
    pub name: String,
    pub required: bool,
}

/// `<instrumentation>` (ParsedInstrumentation, frameworks/base core/java/android/content/pm/parsing/component/ParsedInstrumentation.java).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentationDecl {
    /// `android:name`, resolved against the package name: the Instrumentation class.
    pub name: String,
    /// `android:targetPackage`.
    pub target_package: String,
    /// `android:targetProcesses`; None = the target package's default process.
    pub target_processes: Option<String>,
    /// `android:handleProfiling`, `android:functionalTest` (both default false).
    pub handle_profiling: bool,
    pub functional_test: bool,
}

/// ActivityInfo.SCREEN_ORIENTATION_* (ActivityInfo.java:824-909) that the services and the manifest reader name. `-1` is what an unwritten `screenOrientation` means (PackageParser.java:4457-4459).
pub const SCREEN_ORIENTATION_UNSPECIFIED: i32 = -1;
pub const SCREEN_ORIENTATION_LANDSCAPE: i32 = 0;
pub const SCREEN_ORIENTATION_PORTRAIT: i32 = 1;
pub const SCREEN_ORIENTATION_SENSOR_LANDSCAPE: i32 = 6;
pub const SCREEN_ORIENTATION_SENSOR_PORTRAIT: i32 = 7;
pub const SCREEN_ORIENTATION_REVERSE_LANDSCAPE: i32 = 8;
pub const SCREEN_ORIENTATION_REVERSE_PORTRAIT: i32 = 9;
pub const SCREEN_ORIENTATION_USER_LANDSCAPE: i32 = 11;
pub const SCREEN_ORIENTATION_USER_PORTRAIT: i32 = 12;

fn unspecified_orientation() -> i32 {
    SCREEN_ORIENTATION_UNSPECIFIED
}

/// Records written before the field existed read as resizable, the AOSP default (see `ActivityDecl::resizeable`).
fn resizeable_by_default() -> bool {
    true
}

/// `<activity>` (ParsedActivity, reduced): its class resolved against the package, whether it handles MAIN + LAUNCHER, and its boolean attributes as written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityDecl {
    /// `android:name`, resolved against the package name: the Activity class.
    pub name: String,
    /// It handles MAIN + LAUNCHER (the first such activity is the package's launcher activity).
    pub launcher: bool,
    /// `android:theme` as a resource id (ActivityInfo.theme); 0 = none written, ActivityInfo.getThemeResource then takes the application's.
    pub theme: i32,
    /// ActivityInfo.configChanges: `android:configChanges` with the recreateOnConfigChanges rule applied (ParsedActivityUtils.getActivityConfigChanges).
    pub config_changes: i32,
    /// Its boolean attributes by name, only those the manifest wrote (`hardwareAccelerated`, ...); the flags they set are in manifest_flags.
    pub booleans: BTreeMap<String, bool>,
    /// `android:launchMode` as ActivityInfo.launchMode: 0 = standard (LAUNCH_MULTIPLE), the default when the manifest does not write it.
    #[serde(default)]
    pub launch_mode: i32,
    /// `android:taskAffinity`; None = the package name (ActivityRecord's default affinity).
    #[serde(default)]
    pub task_affinity: Option<String>,
    /// `android:process` resolved the way the application's is (`:name` -> `<package>:name`); None = the package's process.
    #[serde(default)]
    pub process: Option<String>,
    /// `<activity>`'s `<meta-data>` in manifest order, typed as `<application>`'s (e.g. NativeActivity's `android.app.lib_name`).
    #[serde(default)]
    pub meta_data: Vec<(String, MetaValue)>,
    /// `android:screenOrientation` as ActivityInfo.screenOrientation; unwritten = SCREEN_ORIENTATION_UNSPECIFIED.
    #[serde(default = "unspecified_orientation")]
    pub screen_orientation: i32,
    /// ActivityInfo.isResizeableMode of the resize mode PackageParser.setActivityResizeMode picks (PackageParser.java:4677-4715): `android:resizeableActivity` on the activity, else on `<application>`, else resizable.
    /// The targetSdk default is resizable in both of its branches (RESIZE_MODE_RESIZEABLE_VIA_SDK_VERSION from targetSdk >= 24, FORCE_RESIZABLE_* below it; isResizeableMode lists both, ActivityInfo.java:2073-2080), so no targetSdk is read.
    #[serde(default = "resizeable_by_default")]
    pub resizeable: bool,
    /// `android:minAspectRatio`, else `<application android:minAspectRatio>` (PackageParser.setMinAspectRatio, :4758-4768); None = not declared.
    #[serde(default)]
    pub min_aspect_ratio: Option<f32>,
    /// `android:maxAspectRatio`, else `<application android:maxAspectRatio>` (PackageParser.setMaxAspectRatio, :4721-4752); None = not declared.
    /// Not modelled: the pre-O default 1.86 for targetSdk < 26 (DEFAULT_PRE_O_MAX_ASPECT_RATIO, PackageParser.java:176, :4724-4725) and the `android.max_aspect` meta-data (:4730-4747). For the initial-bounds rule it only matters on displays wider than 1.86:1.
    #[serde(default)]
    pub max_aspect_ratio: Option<f32>,
}

/// `<intent-filter>` of a component (ParsedIntentInfo's IntentFilter, reduced to what broadcast matching reads): actions, categories, `<data android:scheme>` and `<data android:mimeType>`.
/// None = the filter names none (IntentFilter's null lists), which matchData treats differently from an empty list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentFilterDecl {
    pub actions: Vec<String>,
    pub categories: Option<Vec<String>>,
    pub schemes: Option<Vec<String>>,
    pub types: Option<Vec<String>>,
}

/// `<receiver>` (ParsedActivity built by ParsedActivityUtils.parseReceiver, reduced): class, exported, permission, process, filters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiverDecl {
    /// `android:name`, resolved against the package name: the BroadcastReceiver class.
    pub name: String,
    /// `android:exported`; unwritten = whether it has an intent filter (ParsedActivityUtils: `exported` defaults to `intents.size() > 0`).
    pub exported: bool,
    /// `android:enabled` (default true).
    pub enabled: bool,
    /// `android:permission`: what a sender must hold (ActivityInfo.permission).
    pub permission: Option<String>,
    /// `android:process` resolved as the application's; None = the package's process.
    pub process: Option<String>,
    pub filters: Vec<IntentFilterDecl>,
}

/// `<provider>` (ParsedProviderUtils.parseProvider, reduced to what ProviderInfo carries; no uri-permission patterns or path permissions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderDecl {
    /// `android:name`, resolved against the package name: the ContentProvider class.
    pub name: String,
    /// `android:authorities`, `;`-separated as ProviderInfo.authority; None = queryProviders skips it.
    pub authority: Option<String>,
    /// `android:exported`; unwritten = targetSdk < 17 (parseProvider).
    pub exported: bool,
    /// `android:enabled` (default true).
    pub enabled: bool,
    /// `android:process` resolved as the application's; None = the package's process.
    pub process: Option<String>,
    /// `android:readPermission`, else `android:permission`, else `<application android:permission>`.
    pub read_permission: Option<String>,
    /// `android:writePermission`, same fallbacks.
    pub write_permission: Option<String>,
    pub grant_uri_permissions: bool,
    pub force_uri_permissions: bool,
    pub multiprocess: bool,
    /// `android:initOrder` (default 0): higher starts first.
    pub init_order: i32,
    pub syncable: bool,
    pub direct_boot_aware: bool,
    /// `<provider>`'s `<meta-data>` in manifest order, typed as `<application>`'s (androidx.startup lists its initializers here).
    pub meta_data: Vec<(String, MetaValue)>,
}

/// What an APK's AndroidManifest.xml declares that the services use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version_code: i64,
    pub version_name: Option<String>,
    pub min_sdk: i32,
    pub target_sdk: i32,
    /// `<application android:process>`; None = the package name (ApplicationInfo.processName's default).
    pub process_name: Option<String>,
    /// `<application android:theme>` as a resource id (ApplicationInfo.theme); 0 = none written.
    pub application_theme: i32,
    /// `<application android:name>`, resolved against the package (ApplicationInfo.className and .name): the Application subclass LoadedApk.makeApplication instantiates; None = android.app.Application.
    #[serde(default)]
    pub application_class: Option<String>,
    /// `<application android:appComponentFactory>`, resolved (ApplicationInfo.appComponentFactory): the factory that instantiates the app's components; None = AppComponentFactory.
    #[serde(default)]
    pub app_component_factory: Option<String>,
    /// `<application>`'s `<meta-data>` in manifest order (ParsingPackageUtils.parseMetaData); a later entry of the same name replaces an earlier one, as Property.toBundle's put does.
    #[serde(default)]
    pub application_meta_data: Vec<(String, MetaValue)>,
    /// `<application>`'s boolean attributes by name, only those the manifest wrote; the flags they set are in manifest_flags.
    pub application_booleans: BTreeMap<String, bool>,
    /// Every `<activity>`, in manifest order.
    pub activities: Vec<ActivityDecl>,
    /// Every `<receiver>`, in manifest order.
    #[serde(default)]
    pub receivers: Vec<ReceiverDecl>,
    /// Every `<provider>`, in manifest order.
    #[serde(default)]
    pub providers: Vec<ProviderDecl>,
    /// `<uses-permission android:name>`, in order.
    pub requested_permissions: Vec<String>,
    /// `<permission>` elements: the permissions the package defines (the platform's are framework-res's), the install-time grant rule's input (permission::install_grants).
    #[serde(default)]
    pub permissions: Vec<PermissionDecl>,
    pub libraries: Vec<UsesLibrary>,
    pub instrumentation: Vec<InstrumentationDecl>,
}

/// `<permission android:name android:protectionLevel>` (ParsedPermissionUtils): the level as written, base type in the low 4 bits (PermissionInfo.PROTECTION_MASK_BASE).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecl {
    pub name: String,
    pub protection_level: u32,
}

/// A `<meta-data>` value as ParsingPackageUtils.parseMetaData types it: `android:resource` -> its id as an Int; `android:value` by TypedValue type: string, boolean, any int type (decimal, hex, color) -> Int, float.
/// A reference in `android:value` is resolved before parseMetaData sees it; other value types are dropped there with a warning, so they have no variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MetaValue {
    String(String),
    Int(i32),
    Bool(bool),
    Float(f32),
}

impl Manifest {
    /// The package's launcher activity: the first `<activity>` that handles MAIN + LAUNCHER.
    pub fn launch_activity(&self) -> Option<&ActivityDecl> {
        self.activities.iter().find(|a| a.launcher)
    }
}

/// One installed package: its manifest and where and as whom it is installed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageRecord {
    pub manifest: Manifest,
    pub uid: u32,
    /// base.apk path (ApplicationInfo.sourceDir)
    pub source_dir: String,
    /// the ABI of the installed native libraries (ApplicationInfo.primaryCpuAbi), none without native code
    pub primary_cpu_abi: Option<String>,
    /// `/data/app/<package>/lib/<isa>` (ApplicationInfo.nativeLibraryDir; its parent is nativeLibraryRootDir), set with or without native code
    pub native_library_dir: String,
    /// credential-protected storage (ApplicationInfo.dataDir, credentialProtectedDataDir: /data/user/<user>/<package>)
    pub data_dir: String,
    /// device-protected storage (ApplicationInfo.deviceProtectedDataDir: /data/user_de/<user>/<package>): the code cache dir, which graphics setup opens first
    pub device_protected_data_dir: String,
    /// The install-time grants (the host's install step, permission::install_grants): what checkPermission answers GRANTED for this uid.
    #[serde(default)]
    pub granted_permissions: Vec<String>,
    /// The GL driver the app process gets (gl_driver.rs): the owner list, else the ES1 rule; the install step sets it.
    #[serde(default)]
    pub gl_driver: crate::gl_driver::GlDriver,
}

impl Deref for PackageRecord {
    type Target = Manifest;
    fn deref(&self) -> &Manifest {
        &self.manifest
    }
}

impl PackageRecord {
    /// The process name: `<application android:process>`, else the package name (ApplicationInfo.processName's default).
    pub fn process_name(&self) -> &str {
        self.process_name.as_deref().unwrap_or(&self.name)
    }
}

/// A shared library as SystemConfig reads it from `<library name file dependency>` (frameworks/base/data/etc/platform.xml: `android.test.runner` depends on `android.test.base:android.test.mock`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    /// its jar as the app processes see it
    pub file: String,
    /// the libraries it needs, by name: a package that uses it gets them loaded for it too
    pub dependencies: Vec<String>,
}

impl Library {
    /// The command-line form `<name>=<file>[,<dependency>...]` (atld's `--shared-library` and the services'): the name, the file, and the names of the libraries it depends on.
    pub fn parse_arg(s: &str) -> Result<(String, Library), String> {
        let (name, rest) = s
            .split_once('=')
            .ok_or_else(|| format!("{s}: not <name>=<file>[,<dependency>...]"))?;
        let mut parts = rest.split(',');
        let file = parts.next().unwrap_or_default();
        if name.is_empty() || file.is_empty() {
            return Err(format!("{s}: not <name>=<file>[,<dependency>...]"));
        }
        Ok((
            name.to_owned(),
            Library {
                file: file.to_owned(),
                dependencies: parts.map(str::to_owned).collect(),
            },
        ))
    }

    /// [`Library::parse_arg`]'s form of this library, with `file` in place of its own (atld installs the jar elsewhere).
    pub fn to_arg(&self, name: &str, file: &str) -> String {
        std::iter::once(format!("{name}={file}"))
            .chain(self.dependencies.iter().cloned())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// A library a package uses and the ones it depends on ([`Packages::used_libraries`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryNode {
    pub name: String,
    pub file: String,
    pub dependencies: Vec<LibraryNode>,
}

/// The package set and the shared libraries they may use.
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
        self.list.iter().find(|p| p.name == name)
    }

    /// Packages whose uid has `app_id` (shared uids give several).
    pub fn by_app_id(&self, app_id: u32) -> impl Iterator<Item = &PackageRecord> {
        self.list
            .iter()
            .filter(move |p| crate::permission::app_id(p.uid) == app_id)
    }

    /// isSameApp (PackageManagerInternal): `package` runs under `uid`'s app id, shared uids included.
    pub fn is_same_app(&self, package: &str, uid: u32) -> bool {
        self.by_app_id(crate::permission::app_id(uid))
            .any(|p| p.manifest.name == package)
    }

    /// Every package, in install order.
    pub fn all(&self) -> &[PackageRecord] {
        &self.list
    }

    /// The instrumentation `class` of `package` and the package declaring it (ComputerEngine's mInstrumentation, keyed by ComponentName; ParsedInstrumentation names are resolved against the package).
    pub fn instrumentation(
        &self,
        package: &str,
        class: &str,
    ) -> Option<(&PackageRecord, &InstrumentationDecl)> {
        let p = self.by_name(package)?;
        let decl = p.instrumentation.iter().find(|i| i.name == class)?;
        Some((p, decl))
    }

    /// Every instrumentation, with its package, in install order (ComputerEngine.queryInstrumentationAsUser's iteration).
    pub fn all_instrumentation(
        &self,
    ) -> impl Iterator<Item = (&PackageRecord, &InstrumentationDecl)> {
        self.list
            .iter()
            .flat_map(|p| p.instrumentation.iter().map(move |i| (p, i)))
    }

    /// Shared library `name`.
    pub fn library(&self, name: &str) -> Option<&Library> {
        self.libraries.get(name)
    }

    /// The libraries `p` uses (its `<uses-library>`s that name an installed library, in manifest order), each with the libraries it depends on below it: the shape of
    /// ApplicationInfo.sharedLibraryInfos (ComputerEngine / SharedLibraryUtils.findSharedLibraries), which LoadedApk turns into one class loader per library, dependencies as its delegates.
    /// A dependency cycle is cut where it closes.
    pub fn used_libraries(&self, p: &PackageRecord) -> Vec<LibraryNode> {
        let names: Vec<&str> = p.libraries.iter().map(|l| l.name.as_str()).collect();
        self.nodes(&names, &mut Vec::new())
    }

    fn nodes<'a>(&'a self, names: &[&'a str], path: &mut Vec<&'a str>) -> Vec<LibraryNode> {
        let mut out = Vec::new();
        for &name in names {
            let Some(lib) = self.library(name) else {
                continue;
            };
            if path.contains(&name) {
                continue;
            }
            path.push(name);
            let deps: Vec<&str> = lib.dependencies.iter().map(String::as_str).collect();
            out.push(LibraryNode {
                name: name.to_owned(),
                file: lib.file.clone(),
                dependencies: self.nodes(&deps, path),
            });
            path.pop();
        }
        out
    }

    /// What `caller` may see of `package` (ComputerEngine shouldFilterApplication): a uid below the first application uid everything (AppsFilterImpl.shouldFilterApplication does not filter
    /// them), an app its own package only (no <queries> yet).
    pub fn visible(&self, caller: u32, package: &str) -> Option<&PackageRecord> {
        let p = self.by_name(package)?;
        let own = crate::permission::app_id(p.uid) == crate::permission::app_id(caller);
        (crate::permission::sees_all_packages(caller) || own).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, uid: u32) -> PackageRecord {
        PackageRecord {
            manifest: Manifest {
                name: name.into(),
                version_code: 1,
                version_name: None,
                min_sdk: 21,
                target_sdk: 34,
                process_name: None,
                application_theme: 0,
                application_class: None,
                app_component_factory: None,
                application_meta_data: vec![],
                application_booleans: BTreeMap::new(),
                activities: vec![],
                receivers: vec![],
                providers: vec![],
                requested_permissions: vec![],
                permissions: vec![],
                libraries: vec![],
                instrumentation: vec![],
            },
            uid,
            source_dir: String::new(),
            primary_cpu_abi: None,
            native_library_dir: String::new(),
            data_dir: String::new(),
            device_protected_data_dir: String::new(),
            granted_permissions: vec![],
            gl_driver: crate::gl_driver::GlDriver::Native,
        }
    }

    #[test]
    fn visibility() {
        let p = Packages::new(
            vec![record("a", 10_001), record("b", 10_002)],
            BTreeMap::new(),
        );
        assert!(p.visible(10_001, "a").is_some());
        assert!(p.visible(10_001, "b").is_none());
        // system and shell (AID_SHELL 2000) see every package; an unknown name is none
        assert!(p.visible(1000, "b").is_some());
        assert!(p.visible(2000, "b").is_some());
        assert!(p.visible(1000, "c").is_none());
        assert_eq!(p.by_app_id(10_002).count(), 1);
    }

    #[test]
    fn used_libraries_follow_the_manifest_and_carry_their_dependencies() {
        let lib = |file: &str, deps: &[&str]| Library {
            file: file.into(),
            dependencies: deps.iter().map(|d| (*d).to_owned()).collect(),
        };
        let libraries = BTreeMap::from([
            ("runner".to_owned(), lib("/f/runner.jar", &["base", "mock"])),
            ("base".to_owned(), lib("/f/base.jar", &[])),
            ("mock".to_owned(), lib("/f/mock.jar", &[])),
            // a cycle is cut where it closes
            ("a".to_owned(), lib("/f/a.jar", &["b"])),
            ("b".to_owned(), lib("/f/b.jar", &["a"])),
        ]);
        let mut p = record("p", 10_001);
        // manifest order; "absent" names no installed library and adds nothing
        p.manifest.libraries = ["runner", "absent", "base", "a"]
            .iter()
            .map(|n| UsesLibrary {
                name: (*n).to_owned(),
                required: true,
            })
            .collect();
        let packages = Packages::new(vec![p.clone()], libraries);
        let node = |name: &str, file: &str, dependencies: Vec<LibraryNode>| LibraryNode {
            name: name.into(),
            file: file.into(),
            dependencies,
        };
        assert_eq!(
            packages.used_libraries(&p),
            vec![
                node(
                    "runner",
                    "/f/runner.jar",
                    vec![
                        node("base", "/f/base.jar", vec![]),
                        node("mock", "/f/mock.jar", vec![])
                    ]
                ),
                node("base", "/f/base.jar", vec![]),
                node("a", "/f/a.jar", vec![node("b", "/f/b.jar", vec![])]),
            ]
        );
    }

    #[test]
    fn library_arguments() {
        let (name, lib) =
            Library::parse_arg("android.test.runner=/f/r.jar,android.test.base,android.test.mock")
                .expect("parse");
        assert_eq!(name, "android.test.runner");
        assert_eq!(lib.file, "/f/r.jar");
        assert_eq!(lib.dependencies, ["android.test.base", "android.test.mock"]);
        assert_eq!(
            lib.to_arg(
                &name,
                &format!("{}/r.jar", atl_android_paths::SYSTEM_FRAMEWORK)
            ),
            "android.test.runner=/system/framework/r.jar,android.test.base,android.test.mock"
        );
        assert_eq!(
            Library::parse_arg("x=/y.jar")
                .expect("parse")
                .1
                .dependencies,
            Vec::<String>::new()
        );
        for bad in ["x", "=y", "x=", "x=,dep"] {
            assert!(Library::parse_arg(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn process_name_defaults_to_the_package() {
        let mut r = record("a", 10_001);
        assert_eq!(r.process_name(), "a");
        r.manifest.process_name = Some("a:remote".into());
        assert_eq!(r.process_name(), "a:remote");
    }

    /// The launcher activity is the first activity that handles MAIN + LAUNCHER; none when no activity does.
    #[test]
    fn launch_activity_is_the_first_launcher() {
        let activity = |name: &str, launcher: bool| ActivityDecl {
            name: name.into(),
            launcher,
            theme: 0,
            config_changes: 0,
            booleans: BTreeMap::new(),
            launch_mode: 0,
            task_affinity: None,
            process: None,
            meta_data: vec![],
            screen_orientation: SCREEN_ORIENTATION_UNSPECIFIED,
            resizeable: true,
            min_aspect_ratio: None,
            max_aspect_ratio: None,
        };
        let mut r = record("a", 10_001);
        assert!(r.launch_activity().is_none());
        r.manifest.activities = vec![
            activity("a.Other", false),
            activity("a.Main", true),
            activity("a.Second", true),
        ];
        assert_eq!(r.launch_activity().map(|a| a.name.as_str()), Some("a.Main"));
    }

    /// The record file atld reads is JSON of these types: a record survives the round trip.
    #[test]
    fn record_round_trips_as_json() {
        let mut r = record("a", 10_001);
        r.manifest.instrumentation.push(InstrumentationDecl {
            name: "a.Runner".into(),
            target_package: "a".into(),
            target_processes: None,
            handle_profiling: false,
            functional_test: true,
        });
        let json = serde_json::to_string(&r).expect("encode");
        assert_eq!(
            serde_json::from_str::<PackageRecord>(&json).expect("decode"),
            r
        );
    }

    /// A record written before the activity fields existed reads the AOSP defaults: unspecified orientation (-1, PackageParser.java:4457), resizable, no aspect limits.
    #[test]
    fn activity_fields_default_when_a_record_lacks_them() {
        let old = r#"{"name":"a.A","launcher":true,"theme":0,"config_changes":0,"booleans":{}}"#;
        let a: ActivityDecl = serde_json::from_str(old).expect("decode");
        assert_eq!(
            (
                a.screen_orientation,
                a.resizeable,
                a.min_aspect_ratio,
                a.max_aspect_ratio
            ),
            (-1, true, None, None)
        );
    }
}
