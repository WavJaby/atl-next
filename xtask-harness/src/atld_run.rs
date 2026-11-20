// SPDX-License-Identifier: MIT
//! What the runtime gates that start atld share (`xtask test app`, `xtask test instrument`): the converter's ART staged as an install lays it out (host_art.rs), servicemanager, atl-system-services, atl-binderd and atld,
//! the Java pipeline's boot jars, aapt2, d8 and the SDK's android.jar, the test fonts, an APK built the way an app is built, and atld's command line for it.
//! The APK is built against the SDK's android.jar of the platform's API level (prebuilts/sdk/<ro.build.version.sdk>/public): aapt2 link (manifest only), javac + d8 (the Java pipeline's), classes.dex added uncompressed.
//! Native inputs: gate_goals.rs `runtime`, plus `gralloc` for [`Runtime::with_gralloc`] / `with_gl_driver` and `nb_guest` for [`Runtime::with_native_bridge_guest`]; each gate's composition is its `GATES` row.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_layout::Layout;
use atl_services_types::display_model::DisplayModel;
use atl_services_types::native_libs::SupportedAbi;
use atl_services_types::packages::Library;
use serde::Serialize;

use crate::gate_goals::{
    ANGLE_FRONTS, EXTRACTOR_PLUGINS, GL_FRONTS, NB_EXE, NB_GUEST_LIBS, NB_LINKER, NDK_PROXIED,
};
use crate::host_art::{self, HostArt};
use crate::{manifest, out, packages, remote, tree};

/// The keystore modules (gate_goals.rs `keystore`), each installed at `<out>/device/<module>/<module>`.
const KEYSTORE2: &str = "keystore2";
const KEYMINT_HAL: &str = "android.hardware.security.keymint-service.nonsecure";
/// The `vintf_fragment_modules` of keystore2 (system/security/keystore2/Android.bp `keystore2_defaults`) and of the HAL (hardware/interfaces/security/keymint/aidl/default/Android.bp), by partition, below the tree.
const KEYSTORE_FRAGMENTS: [(&str, &str); 4] = [
    (
        "system",
        "system/security/keystore2/android.system.keystore2-service.xml",
    ),
    (
        "vendor",
        "hardware/interfaces/security/keymint/aidl/default/android.hardware.security.keymint-service.xml",
    ),
    (
        "vendor",
        "hardware/interfaces/security/keymint/aidl/default/android.hardware.security.sharedsecret-service.xml",
    ),
    (
        "vendor",
        "hardware/interfaces/security/keymint/aidl/default/android.hardware.security.secureclock-service.xml",
    ),
];

/// AOSP's default health HAL (gate_goals.rs `runtime`), installed at `<out>/device/<module>/<module>`, and its Android.bp `vintf_fragments` file (vendor), below the tree: every run starts it (atld `--health`),
/// the battery services' source.
const HEALTH_HAL: &str = "android.hardware.health-service.example";
const HEALTH_FRAGMENT: &str =
    "hardware/interfaces/health/aidl/default/android.hardware.health-service.example.xml";

/// [`Runtime::with_keystore`]'s inputs.
#[derive(Clone)]
struct Keystore {
    keystore2: PathBuf,
    keymint: PathBuf,
    /// (partition, fragment file)
    fragments: Vec<(String, PathBuf)>,
}

/// framework-res.apk in the Java pipeline's out: built as Soong builds it (manifest_fixer, link, merge, align; bp2ninja app.go), unsigned.
const FRAMEWORK_RES: &str = "device/framework-res/framework-res.apk";
/// SettingsProvider's install path below a partitions root (`certificate: "platform"`, `privileged: true` in its Android.bp).
const SETTINGS_PROVIDER: &str = "system/priv-app/SettingsProvider/SettingsProvider.apk";
/// The device's ABI list (`ro.product.cpu.abilist`'s role), each with its instruction set (arches.toml row key): the host row's `android_abis` first (native), then the other `[guest]` rows' as
/// translated, 64-bit before 32-bit, rows in key order within each (arches.toml: preference follows host match and pointer_bits). An app whose only native code is foreign thus installs with
/// that ISA, which the broker passes the launcher; host-native libraries still win when an APK carries both.
pub fn device_abis(arches: &crate::arches::Arches, host: &str) -> Result<Vec<SupportedAbi>> {
    let native = arches
        .get(host)
        .and_then(|a| a.guest.as_ref())
        .with_context(|| format!("{host}: no [guest] row in arches.toml"))?;
    let mut translated: Vec<(&String, &crate::arches::Arch)> = arches
        .iter()
        .filter(|(name, a)| name.as_str() != host && a.guest.is_some())
        .collect();
    // stable: key order kept within one pointer width
    translated.sort_by_key(|(_, a)| std::cmp::Reverse(a.pointer_bits));
    let rows = std::iter::once((host, native)).chain(
        translated
            .into_iter()
            .filter_map(|(name, a)| a.guest.as_ref().map(|g| (name.as_str(), g))),
    );
    Ok(rows
        .flat_map(|(isa, g)| {
            g.android_abis.iter().map(move |abi| SupportedAbi {
                abi: abi.clone(),
                isa: isa.to_owned(),
            })
        })
        .collect())
}

/// The QEMU engine in a test image, as (host file, Android path): the engine `.so` `so` at atl-android-paths `engine_library(isa)`, the boot executable `boot` at `engine_boot(isa)`, where `engine::select` of guest ISA `isa` boots them.
/// Test images only: the engine `.so` is the QEMU bridge (GPL-3.0-or-later), and no product image ships it until its licence row and corresponding-source archive exist (QEMU Q6 plan D-8).
pub fn qemu_engine_files(isa: &str, so: &Path, boot: &Path) -> [(PathBuf, String); 2] {
    [
        (so.to_owned(), atl_android_paths::engine_library(isa)),
        (boot.to_owned(), atl_android_paths::engine_boot(isa)),
    ]
}

/// Where `xtask native` puts GL front `module` under `out`.
pub fn gl_front(out: &Path, module: &str) -> PathBuf {
    out.join("device").join(module).join(format!("{module}.so"))
}

/// ANGLE's ES1 library module, one of gate_goals.rs `ANGLE_FRONTS`: the driver's ES1 library.
const ANGLE_GLES1: &str = "libGLESv1_CM_angle";

/// Where the driver's ES1 library sits (Loader.cpp's `libGLESv1_CM_<ro.hardware.egl>.so` in the driver dir).
fn es1_driver_lib() -> String {
    format!(
        "{}/{}",
        atl_android_paths::VENDOR_EGL,
        atl_android_paths::GL_DRIVER_GLES1
    )
}

/// What the runtime reads from the staged ART `art`: its tree and library dir (whole, remote.rs `Inputs`), the launcher, the property service's input and the boot jars.
fn art_inputs(art: &HostArt) -> remote::Inputs {
    remote::Inputs {
        files: [
            art.launcher.clone(),
            art.property_service.clone(),
            art.build_prop.clone(),
        ]
        .into_iter()
        .chain(art.boot.iter().cloned())
        .collect(),
        dirs: vec![art.art.clone(), art.lib_path.clone()],
    }
}

/// The guest file of nb goal `module` (gate_goals `NB_GUEST_LIBS`): the vdso's own name, a proxy (`libnative_bridge_guest_<stem>`) under its stem, any other module under its name.
fn nb_lib_file(module: &str) -> String {
    if module == "libnative_bridge_vdso" {
        return atl_native_bridge::engine::VDSO_FILE.to_owned();
    }
    let stem = module
        .strip_prefix("libnative_bridge_guest_")
        .unwrap_or(module);
    format!("{stem}.so")
}

/// AOSP's per-feature permission files (PRODUCT_COPY_FILES sources on a device).
const FEATURE_FILES_DIR: &str = "frameworks/native/data/etc";
/// The system features every ATL run has, whatever the host: AOSP's own files as-is, placed in /system/etc/permissions where SystemConfig reads them (atl_services_types::system_config).
/// Host-dependent features (touchscreen per the mouse-touch setting, opengles.aep per the GL probe) are atld's vendor file, not rows here. A row is added only for what ATL provides:
/// a desktop window takes either orientation (AOSP's pc_core_hardware.xml declares both); audio output is the audio service over PipeWire.
const FEATURE_FILES: [&str; 3] = [
    "android.hardware.screen.landscape.xml",
    "android.hardware.screen.portrait.xml",
    "android.hardware.audio.output.xml",
];

/// Where every run places the WebView provider ([`webview_provider_row`]), as AOSP's product image does (media_product.mk `webview`): `<partition>/app/<module>/<module>.apk` (build/soong/java/app.go installPath).
const WEBVIEW_PROVIDER: &str = "app/webview/webview.apk";

/// The /system files of the media stack an app process reads: the extractor plugins (MediaExtractorFactory loads `lib64/extractors`) and AOSP's codec lists (`media_codecs.xml` is the software list; the Codec2 list is `media_codecs_c2.xml`, read from the parser's default search dirs, which include /system/etc).
/// And the system features' permission files ([`FEATURE_FILES`]).
/// One list for every /system placement: atld `--file` rows (every [`Runtime`]) and native-load's test layout (binder.rs `link_rows`). Hard error when a host file is missing: xtask native builds them.
pub(crate) fn platform_rows(out: &Path, aosp: &Path) -> Result<Vec<(PathBuf, String)>> {
    let data = aosp.join("frameworks/av/media/libstagefright/data");
    let mut rows = Vec::new();
    for plugin in EXTRACTOR_PLUGINS {
        rows.push((
            out.join("device").join(plugin).join(format!("{plugin}.so")),
            format!("{}/extractors/{plugin}.so", atl_android_paths::SYSTEM_LIB),
        ));
    }
    let system = atl_android_paths::SYSTEM;
    rows.push((
        data.join("media_codecs_sw.xml"),
        format!("{system}/etc/media_codecs.xml"),
    ));
    rows.push((
        data.join("media_codecs_google_c2.xml"),
        format!("{system}/etc/media_codecs_c2.xml"),
    ));
    let features = aosp.join(FEATURE_FILES_DIR);
    for file in FEATURE_FILES {
        rows.push((
            features.join(file),
            format!("{system}/etc/permissions/{file}"),
        ));
    }
    // org.apache.http.legacy (in every runtime's goals): its implementation jar at the Soong install path, and the permission file the converter writes from the java_sdk_library (sdklibxml.go)
    rows.push((
        out.join("device/org.apache.http.legacy.impl/aligned/org.apache.http.legacy.jar"),
        format!("{system}/framework/org.apache.http.legacy.jar"),
    ));
    rows.push((
        out.join("gen/etc/permissions/org.apache.http.legacy.xml"),
        format!("{system}/etc/permissions/org.apache.http.legacy.xml"),
    ));
    for (host, _) in &rows {
        ensure!(
            host.is_file(),
            "missing input {} (the media stack's /system files: xtask native builds the plugins; the permission files: the pinned AOSP tree)",
            host.display()
        );
    }
    Ok(rows)
}

/// The converter's AOSP SettingsProvider at its Soong install path, as a device's /system always has it: the system scan finds it (priv-app: privileged) and the `settings` authority is served by its
/// own process, started at the first getContentProvider. Every [`Runtime`] places it (PhoneWindow's constructor reads Settings.Global, so any Activity needs it); not in [`platform_rows`], whose
/// native-load layout reads no APK.
fn settings_provider_row(out: &Path) -> Result<(PathBuf, String)> {
    let built = out.join("partitions").join(SETTINGS_PROVIDER);
    ensure!(
        built.is_file(),
        "missing input {} (xtask native builds goal SettingsProvider)",
        built.display()
    );
    Ok((built, format!("/{SETTINGS_PROVIDER}")))
}

/// The converter's WebView provider (repo `java/webview-provider`, system-packages R2) at its product install path, as AOSP's product image ships it: every [`Runtime`] places it (webview plan
/// S4.2, W3c); whether apps reach it is FEATURE_WEBVIEW's, which atld declares when the web engine module loads.
fn webview_provider_row(out: &Path) -> Result<(PathBuf, String)> {
    let built = out.join("partitions/product").join(WEBVIEW_PROVIDER);
    ensure!(
        built.is_file(),
        "missing input {} (xtask native builds goal webview)",
        built.display()
    );
    Ok((
        built,
        format!("{}/{WEBVIEW_PROVIDER}", atl_android_paths::PRODUCT),
    ))
}

/// The billing-unavailable responder (repo `java/vending`, module `Vending`) at its product install path (`<partition>/app/<module>/<module>.apk`, build/soong/java/app.go installPath).
const VENDING: &str = "app/Vending/Vending.apk";

/// [`VENDING`]'s row: placed only by [`Runtime::with_billing_unavailable`].
fn vending_row(out: &Path) -> Result<(PathBuf, String)> {
    let built = out.join("partitions/product").join(VENDING);
    ensure!(
        built.is_file(),
        "missing input {} (xtask native builds goal Vending)",
        built.display()
    );
    Ok((built, format!("{}/{VENDING}", atl_android_paths::PRODUCT)))
}

/// The namespace script of [`Runtime::atld_command`]: `$0` fake `/sys/bus`, `$1` fake udev database, `$2` a directory for the host's `/dev`, then `name host` pairs (char devices), `--`, atld's command.
/// `/dev` is rebuilt, not bound over: a mount point cannot be made in the host's devtmpfs from a user namespace, so a tmpfs takes `/dev`'s place and every host entry is bound back into it.
const FAKE_KERNEL: &str = r#"set -eu
mount --bind "$0" /sys/bus
mount --bind "$1" /run/udev/data
mount --rbind /dev "$2"
mount -t tmpfs tmpfs /dev
for n in $(ls -A "$2"); do
  if [ -L "$2/$n" ]; then ln -s "$(readlink "$2/$n")" "/dev/$n"
  elif [ -d "$2/$n" ]; then mkdir "/dev/$n" && mount --rbind "$2/$n" "/dev/$n"
  else : > "/dev/$n" && mount --bind "$2/$n" "/dev/$n"
  fi
done
shift 2
while [ "$1" != "--" ]; do : > "/dev/$1" && mount --bind "$2" "/dev/$1"; shift 2; done
shift
exec "$@""#;

/// An offline run's network setup inside its fresh namespace (`unshare -Urn`): loopback up, then exec the command (no fork: the pid stays atld's).
const OFFLINE_NET: &str = r#"set -eu
ip link set lo up
exec "$@""#;

/// The sensors gate's fake kernel views (see [`Runtime::with_fake_devices`]).
#[derive(Clone)]
struct FakeDevices {
    sys_bus: PathBuf,
    udev_data: PathBuf,
    /// `(name under /dev, host node)`.
    nodes: Vec<(String, PathBuf)>,
}

/// The staged runtime and the tools around it. Clone: a run's wiring goes on a copy ([`crate::run_apk::run_on`]).
#[derive(Clone)]
pub struct Runtime {
    pub art: HostArt,
    /// The native build directory: the gates keep their work below it.
    pub out: PathBuf,
    /// The host arch: a foreign guest's native-bridge graph is read from its own dir (native::graph_dir).
    host: String,
    pub java_out: PathBuf,
    pub apk: ApkTools,
    servicemanager: PathBuf,
    services: PathBuf,
    /// atl-am, atld's `--am`: the control command `start`
    am: PathBuf,
    /// atl-screencap, atld's `--screencap`: the control command `capture`
    screencap: PathBuf,
    binderd: PathBuf,
    sandbox_stub: PathBuf,
    /// atl-sandbox-join and the `flatpak-spawn` shim: atld's `--sandbox-join` / `--flatpak-spawn` (the apps' web-process channel, atl_broker::webspawn)
    sandbox_join: PathBuf,
    flatpak_spawn: PathBuf,
    /// Whether atld gets the two above; false only for the deliberate break that shows WebKit's abort without the channel.
    webspawn: bool,
    atld: PathBuf,
    /// atl-installer and the api-impl dex: atld's `--installer`, `--installer-class-path`
    installer: PathBuf,
    api_impl: PathBuf,
    framework_res: PathBuf,
    /// The pinned test fonts atld reads as the desktop's (test_fonts.rs): its FONTCONFIG_FILE and `--font-data`.
    fonts: crate::test_fonts::TestFonts,
    /// [`Runtime::for_user_launch`]: atld reads the desktop's own fontconfig, not the pinned set (`--font-data` stays AOSP's).
    desktop_fonts: bool,
    /// AOSP's keyboards dir: its [`tree::KEYBOARD_FILES`] are linked into /system (the input service loads the maps, the app's input thread the layout).
    keychars: PathBuf,
    /// The graphics allocator HAL and the directory of `mapper.atl.so`, when [`Runtime::with_gralloc`] asked for them.
    gralloc: Option<(PathBuf, PathBuf)>,
    /// keystore2, the KeyMint HAL and their VINTF fragments, when [`Runtime::with_keystore`] asked for them.
    keystore: Option<Keystore>,
    /// The health HAL ([`HEALTH_HAL`]) and its VINTF fragment, in every run.
    health: (PathBuf, PathBuf),
    /// atl-egl-probe, atld's GPU selector (`--gpu-probe`), with [`Runtime::with_gralloc`].
    gpu_probe: Option<PathBuf>,
    /// atl-engine-cpuinfo, atld's guest cpuinfo source (`--cpuinfo-probe`), with [`Runtime::with_qemu_engine`].
    cpuinfo_probe: Option<PathBuf>,
    /// The GL driver and the fronts, when [`Runtime::with_gl_driver`] asked for them: files placed by atld `--file`.
    gl_driver: Vec<(PathBuf, String)>,
    /// A gate's own test files placed by atld `--file` ([`Runtime::with_fixture_file`]): never shipped, so outside `refuse_test_only`.
    fixture_files: Vec<(PathBuf, String)>,
    /// The native-bridge guest placed by atld `--file` ([`Runtime::with_native_bridge_guest`]): (host file, Android path).
    native_bridge_guest: Vec<(PathBuf, String)>,
    /// The media stack's /system files ([`platform_rows`]), placed by every run.
    platform_files: Vec<(PathBuf, String)>,
    /// The directory of the NativeBridge core (atl-native-bridge `LIBRARY`), which ART of every app process opens by name (`ro.dalvik.vm.native.bridge`).
    native_bridge: PathBuf,
    /// The directory of the platform module (`libatl_platform_ui_linux.so`, ui_module.rs `stage_module`), which the launcher's atl-platform-ui facade opens by name: on every app's library path, UI or not (the audio output is in it too).
    platform_module: PathBuf,
    /// The device's ABIs, preferred first (`ro.product.cpu.abilist`'s role), for the install's native-library extraction and each app's instruction set ([`device_abis`]).
    abis: Vec<SupportedAbi>,
    /// Pointer width of each arches.toml `[guest]` row, by ISA: where its guest libraries install (atl-android-paths `native_bridge_lib`).
    guest_bits: std::collections::BTreeMap<String, u32>,
    /// The headless pointer script (atld `--headless-pointer`), when [`Runtime::with_headless_pointer`] set it.
    headless_pointer: Option<String>,
    /// The display the run answers with (atld `--display`, `WxH@hz,dpi`).
    display: Option<String>,
    /// The app processes' UI mode (atld `--ui`), when [`Runtime::with_ui`] set it.
    ui: Option<String>,
    /// Variables the desktop would set for this launch, in atld's environment ([`Runtime::with_desktop_env`]).
    desktop_env: Vec<(String, String)>,
    /// `KEY=VALUE`s for every app process (atld `--app-env`, [`Runtime::with_app_env`]).
    app_env: Vec<String>,
    /// Apps serve AudioFlinger and AudioPolicyService unless this is cleared (`--no-audio-service`, the audio gate's break).
    audio_service: bool,
    /// Apps' AudioFlinger gives tracks a control block one buffer short when this is set (`--break-cblk`, the audio gate's second break).
    audio_break_cblk: bool,
    /// Apps serve media.player unless this is cleared (`--no-media-player-service`, the media player gate's break).
    media_player_service: bool,
    /// Apps' AudioFlinger mixes silence when this is set (`--break-mix`, the audio gate's third break).
    audio_break_mix: bool,
    /// atld's `PIPEWIRE_REMOTE` (the desktop's server socket, which atld grants the apps) when set: a stale socket file makes an unreachable server (audio_app.rs `--unreachable`).
    pipewire_remote: Option<PathBuf>,
    /// atld's `PULSE_SERVER` (where atld finds the pulse socket `share-sound` grants the web process).
    pulse_server: PulseServer,
    /// atld's `PIPEWIRE_PROPS` (libpipewire's launcher variable, read by every `pw_stream_new`): a run's own sink as its streams' target ([`Runtime::with_audio_routing`]); unset = the
    /// session's routing (WirePlumber's default sink).
    stream_target: Option<String>,
    /// The Wayland socket atld's children connect to (the window gate's proxy); the environment's when unset.
    wayland: Option<PathBuf>,
    /// Apps run under atld's app seccomp filter (atl-broker seccomp.rs) unless this is cleared (`--no-app-seccomp`, the deliberate-failure check).
    app_seccomp: bool,
    /// atld probes each render-path candidate without its nodes (`--break-render-path`): the render-path choice's deliberate break.
    break_render_path: bool,
    /// The build's `[test_only]` artifacts (licenses.toml): never bound into the runtime root.
    test_only: std::collections::BTreeSet<PathBuf>,
    /// Input method clients get a death link unless this is cleared (`--no-ime-death-link`, the ime gate's deliberate break).
    ime_death_link: bool,
    /// A start may reuse a singleTop / singleTask instance unless this is cleared (`--no-new-intent`, the start-finish stage's deliberate break).
    new_intent: bool,
    /// Starts owe their callers a result unless this is cleared (`--no-activity-result`, the app gate's deliberate break).
    activity_result: bool,
    /// bindService works unless this is cleared (`--no-bind-service`, the app gate's bound-service break).
    bind_service: bool,
    /// The jobscheduler runs jobs unless this is cleared (`--no-job-runner`, the app gate's job-runner break).
    job_runner: bool,
    /// An unhandled Back finishes an activity unless this is cleared (`--no-back-finish`, the dialog gate's back sub-gate's deliberate break).
    back_finish: bool,
    /// An unhandled Back on the root closes it instead of minimizing (atld `--root-back close`, the dialog gate's minimize sub-gate's deliberate break).
    root_back_close: bool,
    /// The services register `sensorservice` unless this is cleared (`--no-sensor-service`, the sensors gate's deliberate break).
    sensor_service: bool,
    /// Services left unregistered (`--omit-service`, the app gate's per-service breaks).
    omitted_services: Vec<String>,
    /// atld `--tilt-keys` plus these extra atld args (`--tilt-max-deg` ...), when set.
    tilt_keys: Option<Vec<String>>,
    /// atld `--mouse-touch`: the left button is one finger. Off unless a run asks: a gate asserts exact AOSP input by default.
    mouse_touch: bool,
    /// atld `--esc-back`: an unhandled Esc falls back to Back (the keyboard map's overlay). Off unless a run asks, as `mouse_touch`.
    esc_back: bool,
    /// atld `--exit-with-last-app`: the run ends with the last app. Off unless a run asks: a gate ends its run itself (`stop`), and an app's death mid-gate is an event, not the end.
    exit_with_last_app: bool,
    /// Sensors gate kernel fakes: atld runs in a private user + mount namespace (`unshare -Urm`) that shows these as the fixed paths the product reads.
    fake_devices: Option<FakeDevices>,
    /// An offline run ([`Runtime::offline`]): atld in a user + network namespace of its own, loopback only.
    offline: bool,
    /// The gate this runtime was staged for: each run writes its record (remote.rs `record_path`) under `out`.
    gate: String,
    /// The AOSP tree (`src/aosp`): the record's root for the `src/aosp/` bundle names.
    tree: PathBuf,
    /// The LLVM runtimes this host's outputs link against (remote.rs `llvm_lib_dir`): the record's `lib` root, the bundle's `lib/`.
    llvm_lib: PathBuf,
    /// Where atld runs: this host, or a target running it from the bundle (`remote test`, remote.rs [`remote::Site`]).
    site: remote::Site,
    /// The persistent data root (atld `--data-root`) when [`Runtime::with_data_root`] set it: kept across runs. Unset: a fresh one beside the run's root ([`fresh_data_root`], the gates), never atld's default (the user's own).
    data_root: Option<PathBuf>,
}

/// A command the shell runs once the system is up (`am instrument ...`).
pub struct Shell {
    pub class: String,
    pub uid: u32,
    /// its log, `log/<name>.log`
    pub name: String,
    pub args: Vec<String>,
    /// the dex jars holding `class`
    pub boot_jars: Vec<PathBuf>,
}

/// One package: its files, base first. The services install it (the installer's parse is the package); no manifest reaches atld.
pub struct Package {
    pub files: Vec<PackageFile>,
}

/// One APK of a package. Which is the base and what each split is called is the installer's parse; atld stages each under its file name (`.apk`, unique in the package).
#[derive(Serialize)]
pub struct PackageFile {
    pub path: PathBuf,
}

impl Package {
    /// A package of one base APK and no splits.
    pub fn base(path: PathBuf) -> Self {
        Self {
            files: vec![PackageFile { path }],
        }
    }
}

/// The first free app uid when no platform package holds one (the services stage, which scans no partition), derived from AOSP's rule (AppIdSettingMap.acquireAndRegisterNewAppId, the services'
/// atl_services::app_id): the lowest free id >= FIRST_APPLICATION_UID (10000), and 10000 is the installer's (atl_broker::INSTALLER_UID) -> 10001. A [`Runtime`] run places platform apps first: [`Runtime::first_app_uid`].
pub const FIRST_APP_UID: u32 = 10_001;

/// Platform packages a run may place that take an app id of their own (no sharedUserId in their manifests: repo `java/webview-provider`, `java/vending`; the settings provider shares uid 1000).
/// The boot scan registers them before any install (PackageManagerService scans the partitions first), so each one placed moves the installed packages' uids up by one.
const OWN_APP_ID_ROWS: [&str; 2] = [WEBVIEW_PROVIDER, VENDING];

/// Platform rows that are packages: the boot scan (atl-services install.rs `scan`) finds each one placed. [`OWN_APP_ID_ROWS`] plus the settings provider (shared uid 1000, so no app id of its own).
const PACKAGE_ROWS: [&str; 3] = [SETTINGS_PROVIDER, WEBVIEW_PROVIDER, VENDING];

/// One atld run.
pub struct Run {
    /// atld's runtime root (atld makes it again); its logs are `<root>/log/<process>.log`.
    pub root: PathBuf,
    /// Where the package files and atld's own log go (outside the root, which atld removes).
    pub scratch: PathBuf,
    /// Each package's files, in install order: on a fresh run the i-th gets uid [`Runtime::first_app_uid`] + i.
    pub packages: Vec<Package>,
    pub launch: Option<String>,
    /// Shared libraries by name; `file` is the host jar.
    pub libraries: Vec<(String, Library)>,
    pub shell: Option<Shell>,
    pub log_tags: String,
}

/// atld's `--package` file.
#[derive(Serialize)]
struct PackageSource<'a> {
    files: &'a [PackageFile],
}

impl Run {
    pub fn atld_log(&self) -> PathBuf {
        self.scratch.join("atld.log")
    }

    pub fn log(&self, name: &str) -> PathBuf {
        self.root.join("log").join(format!("{name}.log"))
    }

    /// One command on atld's control socket (atld main.rs serve_run_control: `stop`, `start <package>/<class>`); its reply line.
    pub fn control(&self, command: &str) -> Result<String> {
        control_at(&self.root, command)
    }

    /// Ends the run the way it ends (owner decision 2026-11-06): `stop` on the control socket, then waits for atld. SIGKILL only when atld does not answer or exit within [`STOP_WAIT`]
    /// (its processes then stay: the caller's stop_apps ends the apps first).
    pub fn stop(&self, atld: &mut Child) -> Result<ExitStatus> {
        if let Some(s) = atld.try_wait()? {
            return Ok(s);
        }
        let asked = self.control("stop");
        let deadline = Instant::now() + STOP_WAIT;
        while Instant::now() < deadline {
            if let Some(s) = atld.try_wait()? {
                return Ok(s);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("atld: no exit {STOP_WAIT:?} after stop ({asked:?}): killed");
        let _ = atld.kill();
        Ok(atld.wait()?)
    }

    /// Whether the first app process the services started has ended (atld's `started ... host pid <n>`, then `process <n> ended`): the run goes on without it, so a gate that ends with the
    /// app reads it here.
    pub fn app_ended(&self) -> bool {
        first_app_ended(&std::fs::read_to_string(self.atld_log()).unwrap_or_default())
    }
}

/// The program names every process atld starts runs as, the broker's stages: bwrap ([`atl_bwrap::PROGRAM`]), and atl-sandbox-join, which runs bwrap for a web process inside its app's
/// namespaces (atl_broker::webspawn). The broker launches nothing else.
const BROKER_CHILD_PROGRAMS: [&str; 2] = [atl_bwrap::PROGRAM, "atl-sandbox-join"];

/// The kernel's comm of a program name: its first TASK_COMM_LEN - 1 = 15 bytes (linux/sched.h TASK_COMM_LEN 16, the NUL included).
fn comm_of(program: &str) -> &str {
    program.get(..15).unwrap_or(program)
}

/// atld's live children that did not come from the broker, as `<pid> <comm>`: every child of `atld` whose comm is no [`BROKER_CHILD_PROGRAMS`] one. Read from `/proc/*/stat` (ppid, comm): `/proc/<pid>/task/*/children` needs CONFIG_PROC_CHILDREN, which the WSL kernel lacks.
/// A process that exits between the directory listing and its stat read is skipped; atld is alive when this runs, so its sandboxes are too. A child between fork and exec carries its forker's comm: match it by pid, not comm.
pub fn children_outside_broker(atld: u32) -> Result<Vec<String>> {
    let mut outside = Vec::new();
    for e in std::fs::read_dir("/proc")? {
        let e = e?;
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        let Some((comm, ppid)) = stat_comm_ppid(&stat) else {
            continue;
        };
        if ppid == atld && !BROKER_CHILD_PROGRAMS.iter().any(|p| comm_of(p) == comm) {
            outside.push(format!("{pid} {comm}"));
        }
    }
    Ok(outside)
}

/// `/proc/<pid>/stat`'s comm and ppid: `pid (comm) state ppid ...`, comm between the first `(` and the last `)` (it may hold either).
fn stat_comm_ppid(stat: &str) -> Option<(&str, u32)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    let ppid = stat
        .get(close + 1..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((comm, ppid))
}

/// atld main.rs CONTROL_SOCKET, below the root.
const CONTROL_SOCKET: &str = "control.sock";
/// How long a control command may take: `start` waits for atl-am's startActivity round trip.
const CONTROL_REPLY: Duration = Duration::from_secs(30);
/// How long atld may take to exit after `stop`: it kills the sandboxes and waits for the services and servicemanager.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// [`Run::control`] for a run's `root`, for a thread that holds no Run (the ime probe).
pub fn control_at(root: &Path, command: &str) -> Result<String> {
    let path = root.join(CONTROL_SOCKET);
    let mut conn =
        UnixStream::connect(&path).with_context(|| format!("connecting {}", path.display()))?;
    conn.set_read_timeout(Some(CONTROL_REPLY))?;
    writeln!(conn, "{command}")?;
    let mut reply = String::new();
    BufReader::new(&conn).read_line(&mut reply)?;
    Ok(reply.trim().to_owned())
}

/// atld's control command `capture <package> <out>`: atl-screencap writes the package's window picture, RGBA rows, to `out` as the shell uid.
/// Returns atld's reply line and the bytes in `out`, empty when atld refused. `out` must not hold a space (the control line splits on it).
pub fn capture_at(root: &Path, package: &str, out: &Path) -> Result<(String, Vec<u8>)> {
    let command = format!("capture {package} {}", out.display());
    let reply = control_at(root, &command)?;
    Ok((reply, std::fs::read(out).unwrap_or_default()))
}

/// atld's end lines for system daemons that died mid-run (atld main.rs `system_processes`), without the `atld: verdict: ` prefix.
fn system_deaths(atld_log: &str) -> Vec<&str> {
    atld_log
        .lines()
        .filter_map(|l| l.strip_prefix("atld: verdict: "))
        .filter(|l| l.starts_with("system process "))
        .collect()
}

/// [`Run::app_ended`] over atld's log text.
fn first_app_ended(atld_log: &str) -> bool {
    let first = atld_log
        .lines()
        .filter(|l| l.contains("started ") && l.contains("(package "))
        .find_map(|l| l.rsplit("host pid ").next()?.trim().parse::<i32>().ok());
    first.is_some_and(|pid| {
        let ended = format!("process {pid} ended");
        atld_log.lines().any(|l| l.contains(&ended))
    })
}

/// The gates' display: the model's default size and density (fixed, so pictures and sizes are the same on every host, and small enough that WSLg keeps the display rate) at the monitor's refresh rate
/// (GDK, atl-platform-host), which the cadence gates judge against. No desktop or no rate: the default rate, and a line says so.
pub fn run_display() -> DisplayModel {
    let hz = match atl_platform_host::first_monitor() {
        Ok(m) => {
            println!("display: monitor {m:?}");
            m.and_then(|m| m.refresh_hz)
        }
        Err(e) => {
            println!("display: {e}, the default refresh rate");
            None
        }
    };
    let default = DisplayModel::default();
    DisplayModel {
        refresh_hz: hz.unwrap_or(default.refresh_hz),
        ..default
    }
}

/// `package`'s `bin`, built plainly (release) into `out`'s cargo target dir, for a cross host's out dir that host's (cdylib::release_build); its path.
fn cargo_bin(root: &Path, out: &Path, package: &str) -> Result<PathBuf> {
    let (mut cmd, release) = crate::cdylib::release_build(root, out, package)?;
    xtask::timing::phase("cargo_bin", || {
        crate::unit_use::exec(cmd.arg("--locked"), &format!("cargo build -p {package}"))
    })?;
    Ok(release.join(package))
}

/// The platform's API level: `ro.build.version.sdk` of the converter's build.prop.
fn sdk_level(build_prop: &Path) -> Result<String> {
    let text = std::fs::read_to_string(build_prop)?;
    text.lines()
        .find_map(|l| l.strip_prefix("ro.build.version.sdk="))
        .map(|v| v.trim().to_owned())
        .with_context(|| format!("no ro.build.version.sdk in {}", build_prop.display()))
}

/// What building an APK needs, and nothing of the runtime: aapt2 and d8 of the Java pipeline, the prebuilt JDK, the SDK's android.jar of the platform's API level. The static fixture gates use it without a staged ART.
#[derive(Clone)]
pub struct ApkTools {
    pub aapt2: PathBuf,
    pub android_jar: PathBuf,
    pub jdk: PathBuf,
    d8: PathBuf,
    pub signer: Signer,
}

impl ApkTools {
    /// `out` = the native build dir (its build.prop names the API level), `java_out` = the Java pipeline build dir, `host` = the arches.toml host row (its prebuilt JDK); every input checked to exist.
    pub fn new(root: &Path, host: &str, out: &Path, java_out: &Path) -> Result<ApkTools> {
        let layout = Layout::of(root)?;
        let (d8, jdk) = host_art::java_tools(root, java_out)?;
        let arches = crate::arches::load(root)?;
        let (build, _) = crate::arches::build_machine(root, &arches)?;
        let t = ApkTools {
            aapt2: java_out
                .join(crate::arches::tool_side(host, build))
                .join("aapt2/aapt2"),
            android_jar: tree::shared_tree(&layout)
                .join("prebuilts/sdk")
                .join(sdk_level(&out.join(host_art::BUILD_PROP))?)
                .join("public/android.jar"),
            signer: Signer::new(&jdk, java_out)?,
            jdk,
            d8,
        };
        for p in [&t.aapt2, &t.android_jar, &t.d8] {
            ensure!(p.is_file(), "missing input {}", p.display());
        }
        ensure!(t.jdk.is_dir(), "missing input {}", t.jdk.display());
        Ok(t)
    }
}

/// AOSP's apksigner (gate_goals.rs APK_SIGNER: tools/apksig's `apksigner` module as the converter builds it, its static libs included) and a debug key, as an app build signs its debug APK: the
/// device's install path verifies every APK (ParsingPackageUtils.getSigningDetails -> ApkSignatureVerifier.verify) and refuses an unsigned one, so every fixture is signed (apksigner's default schemes).
#[derive(Clone)]
pub struct Signer {
    jar: PathBuf,
    keystore: PathBuf,
    java: PathBuf,
}

/// The converter's jar of `apksigner` with its static libs and resources, under the Java pipeline dir.
const APK_SIGNER_JAR: &str = "host/apksigner/withres/apksigner.jar";
/// ApkSignerTool's main class (tools/apksig Android.bp `apksigner` main_class).
const APK_SIGNER_MAIN: &str = "com.android.apksigner.ApkSignerTool";
/// Where the debug keystore is made once and kept, under the Java pipeline dir: fixtures of every run share one signer.
const DEBUG_KEYSTORE: &str = "test-keys/debug.keystore";

/// The Android SDK's debug keystore conventions (the Android Gradle plugin's `debug.keystore`): store and key password `android`, alias `androiddebugkey`, `CN=Android Debug,O=Android,C=US`.
const DEBUG_PASSWORD: &str = "android";
const DEBUG_ALIAS: &str = "androiddebugkey";
const DEBUG_DNAME: &str = "CN=Android Debug,O=Android,C=US";
/// The plugin's validity: 30 years in days.
const DEBUG_VALIDITY_DAYS: &str = "10950";

impl Signer {
    /// The built jar (refused when missing: `native --for <gate>` builds it) and the keystore, made once and kept; written to a temporary name, then renamed, so concurrent gates never see half a file.
    fn new(jdk: &Path, java_out: &Path) -> Result<Signer> {
        let bin = jdk.join("bin");
        let jar = java_out.join(APK_SIGNER_JAR);
        ensure!(jar.is_file(), "missing input {}", jar.display());
        let keystore = java_out.join(DEBUG_KEYSTORE);
        if !keystore.is_file() {
            std::fs::create_dir_all(keystore.parent().context("keystore dir")?)?;
            let out = keystore.with_extension(format!("tmp{}", std::process::id()));
            host_art::exec(
                Command::new(bin.join("keytool"))
                    .args(["-genkeypair", "-keystore"])
                    .arg(&out)
                    .args(["-storepass", DEBUG_PASSWORD, "-keypass", DEBUG_PASSWORD])
                    .args(["-alias", DEBUG_ALIAS, "-dname", DEBUG_DNAME])
                    .args(["-keyalg", "RSA", "-keysize", "2048"])
                    .args(["-validity", DEBUG_VALIDITY_DAYS]),
                "keytool -genkeypair",
            )?;
            std::fs::rename(&out, &keystore)?;
        }
        Ok(Signer {
            jar,
            keystore,
            java: bin.join("java"),
        })
    }

    /// `java -cp <jar> ApkSignerTool`.
    fn tool(&self) -> Command {
        let mut c = Command::new(&self.java);
        c.arg("-cp").arg(&self.jar).arg(APK_SIGNER_MAIN);
        c
    }

    /// Signs `apk` in place (`apksigner sign`, after zipalign as apksigner requires).
    pub fn sign(&self, apk: &Path) -> Result<()> {
        let pass = format!("pass:{DEBUG_PASSWORD}");
        host_art::exec(
            self.tool()
                .args(["sign", "--ks"])
                .arg(&self.keystore)
                .args(["--ks-pass", &pass, "--key-pass", &pass])
                .args(["--ks-key-alias", DEBUG_ALIAS])
                .arg(apk),
            "apksigner sign",
        )
    }

    /// The SHA-256 of `apk`'s signing certificate as `apksigner verify --print-certs` prints it (`Signer #1 certificate SHA-256 digest: <hex>`): a value from AOSP's tool, not from our code.
    pub fn cert_sha256(&self, apk: &Path) -> Result<String> {
        let out = host_art::output(
            self.tool().args(["verify", "--print-certs"]).arg(apk),
            "apksigner verify",
        )?;
        out.lines()
            .find_map(|l| l.strip_prefix("Signer #1 certificate SHA-256 digest: "))
            .map(|h| h.trim().to_owned())
            .context("apksigner verify printed no Signer #1 SHA-256 digest")
    }
}

impl Runtime {
    /// Stages ART into `<out>/<stage>`, builds the cargo binaries, checks every input exists; the test fonts are written to `<out>/fonts`. `gate` names the caller in gate_goals.rs GATES:
    /// the build dir is refused up front unless built with its goals.
    pub fn new(
        root: &Path,
        gate: &str,
        host: &str,
        out: Option<&Path>,
        java_out: Option<&Path>,
        stage: &str,
        verbose: bool,
    ) -> Result<Runtime> {
        let layout = Layout::of(root)?;
        let out = crate::build_state::gate_out(root, out, host, gate)?;
        // a cross host's graph builds its own Java outputs and build-side tools (aapt2 under `build/`) into its out dir; the build dir's are the build machine's graph's
        let java_out = match (java_out, xtask::native::cross_host_of(root, &out)?) {
            (None, Some(_)) => out.clone(),
            (java_out, _) => out::out_dir(root, java_out)?,
        };
        let art = xtask::timing::phase("runtime: host art", || {
            HostArt::new(root, host, Some(&java_out), &out, stage, verbose)
        })?;
        let servicemanager = out.join("device/servicemanager/servicemanager");
        ensure!(
            servicemanager.is_file(),
            "{} missing: build servicemanager",
            servicemanager.display()
        );
        let health = out.join("device").join(HEALTH_HAL).join(HEALTH_HAL);
        ensure!(
            health.is_file(),
            "{} missing: build the runtime goals",
            health.display()
        );
        let health_fragment = tree::shared_tree(&layout).join(HEALTH_FRAGMENT);
        ensure!(
            health_fragment.is_file(),
            "{} missing in the tree",
            health_fragment.display()
        );
        let services = packages::bin(root, &out, "atl-services", "atl-system-services")?;
        let am = packages::bin(root, &out, "atl-services", "atl-am")?;
        let screencap = packages::bin(root, &out, "atl-services", "atl-screencap")?;
        let binderd = cargo_bin(root, &out, "atl-binderd")?;
        let sandbox_stub = cargo_bin(root, &out, "atl-sandbox-stub")?;
        let sandbox_join = cargo_bin(root, &out, "atl-sandbox-join")?;
        // shown in the sandbox as flatpak-spawn (atl_broker::webspawn::SHIM_AT), the name WebKit runs
        let flatpak_spawn = cargo_bin(root, &out, "atl-flatpak-spawn")?;
        let atld = cargo_bin(root, &out, "atld")?;
        let installer = packages::bin(root, &out, "atl-app-launcher", "atl-installer")?;
        let api_impl = host_art::api_impl_dex(&java_out);
        ensure!(
            api_impl.is_file(),
            "no api-impl dex {}: `cargo xtask native` builds it with the Java goals",
            api_impl.display()
        );
        let apk = ApkTools::new(root, host, &out, &java_out)?;
        let framework_res = java_out.join(FRAMEWORK_RES);
        ensure!(
            framework_res.is_file(),
            "missing input {}",
            framework_res.display()
        );
        let fonts =
            crate::test_fonts::stage(&layout, &out.join("fonts"), crate::test_fonts::ROBOTO.2)?;
        let keychars = tree::keyboards(&layout);
        let mut platform_files = platform_rows(&out, &layout.src().join("aosp"))?;
        platform_files.push(settings_provider_row(&out)?);
        platform_files.push(webview_provider_row(&out)?);
        for (file, _) in tree::KEYBOARD_FILES {
            let path = keychars.join(file);
            ensure!(path.is_file(), "missing input {}", path.display());
        }
        let native_bridge = crate::cdylib::stage(
            root,
            &out,
            "atl-native-bridge",
            "native-bridge/lib",
            atl_native_bridge::LIBRARY,
        )?;
        let platform_module = crate::ui_module::stage_module(root, &out)?;
        crate::ui_module::stage_webview(root, &out)?;
        // the staged files, for this out's host: the only gate on a cross host's (riscv64) UI modules, which `xtask check` does not build
        xtask::gtk_imports::check_staged(root, &platform_module)?;
        let arches = crate::arches::load(root)?;
        let abis = device_abis(&arches, host)?;
        let guest_bits = arches
            .iter()
            .filter(|(_, a)| a.guest.is_some())
            .map(|(isa, a)| (isa.clone(), a.pointer_bits))
            .collect();
        let licenses = crate::licenses::load(root, &crate::sources::load(root)?.upstream_names())?;
        let test_only = crate::licenses::test_only_outputs(&licenses, &out)?;
        let llvm_lib = remote::llvm_lib_dir(root, &out)?;
        Ok(Runtime {
            llvm_lib,
            gate: gate.to_owned(),
            tree: tree::shared_tree(&layout),
            test_only,
            native_bridge,
            platform_module,
            abis,
            guest_bits,
            art,
            out,
            host: host.to_owned(),
            java_out,
            apk,
            servicemanager,
            services,
            am,
            screencap,
            binderd,
            sandbox_stub,
            sandbox_join,
            flatpak_spawn,
            keystore: None,
            health: (health, health_fragment),
            webspawn: true,
            atld,
            installer,
            api_impl,
            framework_res,
            fonts,
            desktop_fonts: false,
            keychars,
            gralloc: None,
            gpu_probe: None,
            cpuinfo_probe: None,
            gl_driver: Vec::new(),
            fixture_files: Vec::new(),
            native_bridge_guest: Vec::new(),
            platform_files,
            headless_pointer: None,
            display: None,
            ui: None,
            desktop_env: Vec::new(),
            app_env: Vec::new(),
            audio_service: true,
            audio_break_cblk: false,
            media_player_service: true,
            audio_break_mix: false,
            pipewire_remote: None,
            pulse_server: PulseServer::GateDefault,
            stream_target: None,
            wayland: None,
            app_seccomp: true,
            break_render_path: false,
            ime_death_link: true,
            new_intent: true,
            activity_result: true,
            bind_service: true,
            job_runner: true,
            back_finish: true,
            root_back_close: false,
            sensor_service: true,
            omitted_services: Vec::new(),
            tilt_keys: None,
            mouse_touch: false,
            esc_back: false,
            exit_with_last_app: false,
            fake_devices: None,
            offline: false,
            site: remote::Site::Host,
            data_root: None,
        })
    }

    /// Runs atld on `site` (remote.rs): a target's copy of this runtime's bundle, instead of this host.
    pub fn with_site(mut self, site: remote::Site) -> Self {
        self.site = site;
        self
    }

    /// atld's data root is the persistent dir `dir` (`--data-root`): app data and system state survive the run, and the next run on `dir` sees them (run-apk). Gates keep a fresh one ([`fresh_data_root`]).
    pub fn with_data_root(mut self, dir: &Path) -> Self {
        self.data_root = Some(dir.to_path_buf());
        self
    }

    /// The persistent data root set by [`Runtime::with_data_root`], if any.
    pub fn data_root(&self) -> Option<&Path> {
        self.data_root.as_deref()
    }

    /// Also starts keystore2 and AOSP's Rust software KeyMint HAL (gate_goals.rs `keystore`), declared by their own Android.bp `vintf_fragment_modules` files from the tree: keystore2's in /system, the HAL's keymint / sharedsecret / secureclock in /vendor.
    pub fn with_keystore(mut self, root: &Path) -> Result<Self> {
        let bin = |module: &str| -> Result<PathBuf> {
            let p = self.out.join("device").join(module).join(module);
            ensure!(
                p.is_file(),
                "{} missing: build native goal `{module}` (`native --for keystore`)",
                p.display()
            );
            Ok(p)
        };
        let keystore2 = bin(KEYSTORE2)?;
        let keymint = bin(KEYMINT_HAL)?;
        let tree = tree::shared_tree(&Layout::of(root)?);
        let fragments = KEYSTORE_FRAGMENTS
            .iter()
            .map(|(partition, file)| {
                let f = tree.join(file);
                ensure!(f.is_file(), "{} missing in the tree", f.display());
                Ok(((*partition).to_owned(), f))
            })
            .collect::<Result<Vec<_>>>()?;
        self.keystore = Some(Keystore {
            keystore2,
            keymint,
            fragments,
        });
        Ok(self)
    }

    /// Also starts the graphics allocator HAL under atld (`--gralloc`): the allocator `xtask native` built, and `mapper.atl.so` installed in `<out>/gralloc/lib`, which joins every process's library path.
    /// Needs a DRM device that allocates (atld names what is missing when it does not start).
    pub fn with_gralloc(mut self, root: &Path) -> Result<Self> {
        let allocator = packages::bin(
            root,
            &self.out,
            "atl-gralloc-service",
            "atl-gralloc-allocator",
        )?;
        let lib = crate::gralloc::stage_mapper(root, &self.out)?;
        self.gralloc = Some((allocator, lib));
        self.gpu_probe = Some(cargo_bin(root, &self.out, "atl-egl-probe")?);
        Ok(self)
    }

    /// Also builds and places the GL driver (`libEGL_atl.so` + `libGLESv2_atl.so` over the host's EGL, egl_driver.rs `placement`) where AOSP's libEGL loader finds it: /vendor/lib64/egl and `ro.hardware.egl` in /vendor/build.prop; ANGLE's ES1 library there as `libGLESv1_CM_atl.so` (the driver's ES1 contexts, atl-egl-driver es1.rs).
    /// Also the fronts at Loader.cpp's `SYSTEM_LIB_PATH` (`/system/lib64` in LP64): it dlopens them there as the "wrapper" libraries (`load_wrapper`) and aborts without them; the links name the files already loaded, so glibc returns the loaded copy (same inode).
    pub fn with_gl_driver(self, root: &Path) -> Result<Self> {
        self.with_gl_driver_breaks(root, &[])
    }

    /// [`Self::with_gl_driver`] with the driver built with deliberate breaks `breaks` (atl-egl-driver build.rs `BREAKS`; a gate's red run).
    pub fn with_gl_driver_breaks(mut self, root: &Path, breaks: &[String]) -> Result<Self> {
        let mut files = crate::egl_driver::placement(root, &self.out, breaks)?;
        let system_lib = atl_android_paths::SYSTEM_LIB;
        for &m in GL_FRONTS.iter().chain(ANGLE_FRONTS.iter()) {
            let lib = gl_front(&self.out, m);
            ensure!(
                lib.is_file(),
                "missing input {}: build native goal `{m}` (with_gl_driver needs all of {GL_FRONTS:?})",
                lib.display(),
            );
            files.push((lib, format!("{system_lib}/{m}.so")));
        }
        files.push((gl_front(&self.out, ANGLE_GLES1), es1_driver_lib()));
        self.gl_driver = files;
        Ok(self)
    }

    /// The GL driver without its ES1 library (`xtask test es1-angle`'s break): ES1 contexts must then fail named, not call the fronts.
    pub fn without_es1_driver_lib(mut self) -> Self {
        let at = es1_driver_lib();
        self.gl_driver.retain(|(_, a)| *a != at);
        self
    }

    /// Restages the test fonts with `family` preferred for sans-serif (the font gate's deliberate break: the mono font).
    pub fn with_sans_serif(mut self, layout: &Layout, family: &str) -> Result<Self> {
        self.fonts = crate::test_fonts::stage(layout, &self.out.join("fonts"), family)?;
        Ok(self)
    }

    /// The user's launch (run-apk, local or remote): atld sees the desktop as an installed atld does, one builder so no run-apk path takes only part of it (remote run-apk once missed the fonts and read the pinned set on the target).
    /// - fonts: the desktop's own fontconfig config, not the pinned set (no CJK, emoji or other script face: that text was boxes). Gates keep the pinned set: their text must not depend on the host.
    /// - audio: the session's `PULSE_SERVER`, set or unset: the login environment's audio server choice holds (Linux convention).
    pub fn for_user_launch(mut self) -> Self {
        self.desktop_fonts = true;
        self.pulse_server = PulseServer::Session;
        self
    }

    /// The pinned set's FONTCONFIG_FILE and font files, unless [`Runtime::for_user_launch`]: (conf, files).
    fn pinned_fonts(&self) -> Option<(&Path, &[PathBuf])> {
        (!self.desktop_fonts).then_some((self.fonts.conf.as_path(), self.fonts.files.as_slice()))
    }

    /// Also places the guest `xtask native --native-bridge <isa>` built where the NativeBridge core starts it (atl-native-bridge `Config::installed`): the linker + app_process (atl-android-paths `native_bridge_exes`) under `native_bridge_bin`, the guest libraries under `native_bridge_lib`.
    /// The host ISA's guest serves identity; a foreign ISA's also needs its engine ([`Runtime::with_qemu_engine`]). Each call adds one ISA's guest.
    /// A foreign ISA's files are read from its own graph dir (native::graph_dir); run-apk checks that graph first (build_state::foreign_out).
    pub fn with_native_bridge_guest(mut self, isa: &str) -> Result<Self> {
        let bin = atl_android_paths::native_bridge_bin(isa);
        let bits = *self
            .guest_bits
            .get(isa)
            .with_context(|| format!("{isa}: no [guest] row in arches.toml"))?;
        let lib = atl_android_paths::native_bridge_lib(isa, bits);
        let graph = crate::native::graph_dir(&self.out, &self.host, isa);
        let module = |m: &str, file: &str| crate::native::nb_module_dir(&graph, isa, m).join(file);
        let (linker, exe) = atl_android_paths::native_bridge_exes(bits);
        let mut files = vec![
            (module(NB_LINKER, linker), format!("{bin}/{linker}")),
            (module(NB_EXE, exe), format!("{bin}/{exe}")),
            (
                crate::runtimes::shared_libcxx(&graph, isa),
                format!("{lib}/libc++.so"),
            ),
        ];
        for m in NB_GUEST_LIBS {
            let file = nb_lib_file(m);
            files.push((module(m, &file), format!("{lib}/{file}")));
        }
        for (f, _) in &files {
            ensure!(
                f.is_file(),
                "missing input {} (xtask native --native-bridge {isa})",
                f.display()
            );
        }
        // the NDK proxies' host sides, where the NativeBridge core loads them (bindings.rs `Host::System`), as the GL fronts are placed; one copy whichever guests are placed
        for m in NDK_PROXIED {
            let host = self.out.join("device").join(m).join(format!("{m}.so"));
            ensure!(
                host.is_file(),
                "missing input {} (build native goal `{m}`)",
                host.display()
            );
            let at = format!("{}/{m}.so", atl_android_paths::SYSTEM_LIB);
            if !self.platform_files.iter().any(|(_, a)| *a == at) {
                self.platform_files.push((host, at));
            }
        }
        self.native_bridge_guest.extend(files);
        Ok(self)
    }

    /// The device's ABIs, preferred first ([`device_abis`]).
    pub fn abis(&self) -> &[SupportedAbi] {
        &self.abis
    }

    /// Also places the QEMU engine of foreign guest ISA `isa` (test images only, [`qemu_engine_files`]): `xtask qemu --pic` of its arch for this run's host (built when stale), the engine `.so` linked from it and the boot executable, kept under `<qemu dir>/app-engine`.
    pub fn with_qemu_engine(mut self, root: &Path, isa: &str) -> Result<Self> {
        let b = crate::qemu::plan_on(root, isa, Some(&self.host), None, true)?;
        crate::qemu::execute(root, &b)?;
        let dir = b.dir.join("app-engine");
        let so = crate::qemu_spike::engine_so_file(root, &b, &dir)?;
        let boot = crate::qemu::boot_exe(root, &b, &dir)?;
        self.platform_files
            .extend(qemu_engine_files(isa, &so, &boot));
        // the guest cpuinfo atld binds over translated apps' /proc/cpuinfo: the probe beside the boot executable, run by atl-engine-cpuinfo
        let probe = crate::qemu::cpuinfo_exe(root, &b, &dir)?;
        self.platform_files
            .push((probe, atl_android_paths::engine_cpuinfo(isa)));
        self.cpuinfo_probe = Some(cargo_bin(root, &self.out, "atl-engine-cpuinfo")?);
        Ok(self)
    }

    /// The cpuinfo break (`test app --cpuinfo-break`): atld gets no `--cpuinfo-probe`, so translated apps' host readers see the host's /proc/cpuinfo.
    pub fn without_cpuinfo(mut self) -> Self {
        self.cpuinfo_probe = None;
        self
    }

    /// The C12 break (`test app --drop-nb-lib`): leaves the NB_LIBS file `file` out of the guest's lib dir, so a guest load that needs it fails naming it. Errors when no placed file has that name.
    pub fn without_nb_lib(mut self, file: &str) -> Result<Self> {
        let at = format!("/{file}");
        let before = self.native_bridge_guest.len();
        self.native_bridge_guest
            .retain(|(_, dst)| !dst.ends_with(at.as_str()));
        ensure!(
            self.native_bridge_guest.len() < before,
            "--drop-nb-lib {file}: no placed native-bridge file of that name"
        );
        Ok(self)
    }

    /// The framework-JNI breaks (`test app --soundpool-break`, `--mediaplayer-break`): unlinks `file` from the staged ART lib dir, so every app process's dlopen of it fails. stage_art restages that dir on every run, so the break lasts one run.
    pub fn without_art_lib(self, file: &str) -> Result<Self> {
        let at = self.art.lib_path.join(file);
        std::fs::remove_file(&at).with_context(|| format!("unlinking {}", at.display()))?;
        Ok(self)
    }

    /// The settings provider break (`test app --settings-provider-break`): the run places no SettingsProvider ([`settings_provider_row`]), so no package declares the `settings` authority.
    pub fn without_settings_provider(mut self) -> Self {
        let at = format!("/{SETTINGS_PROVIDER}");
        self.platform_files.retain(|(_, a)| *a != at);
        self
    }

    /// Adds `name=value` to the /vendor/build.prop the run places (the GL driver's, so call it after [`Runtime::with_gl_driver`]; else one of its own), so every process's area holds it: a gate's test input
    /// for a property no product writer sets yet (the splash stage's `ro.atl.splash`). The file is a copy under `<out>`, named by this process, so concurrent gates do not share it.
    pub fn with_vendor_property(mut self, name: &str, value: &str) -> Result<Self> {
        let at = atl_android_paths::VENDOR_BUILD_PROP;
        let slot = self
            .gl_driver
            .iter_mut()
            .chain(self.fixture_files.iter_mut())
            .find(|(_, a)| a == at);
        let mut text = match &slot {
            Some((host, _)) => std::fs::read_to_string(host)
                .with_context(|| format!("reading {}", host.display()))?,
            None => String::new(),
        };
        text += &format!("{name}={value}\n");
        let file = self
            .out
            .join(format!("vendor-build.{}.prop", std::process::id()));
        std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
        match slot {
            Some((host, _)) => *host = file,
            None => self.fixture_files.push((file, at.to_owned())),
        }
        Ok(self)
    }

    /// Places a gate's fixture file `host` at `at` (a /system or /vendor path) in every app sandbox.
    pub fn with_fixture_file(mut self, host: PathBuf, at: &str) -> Self {
        self.fixture_files.push((host, at.to_owned()));
        self
    }

    /// Feeds atld a headless pointer script (format: atl-composer `input::parse_script`); the app injects it after its first frame. Only the input gate sets it.
    pub fn with_headless_pointer(mut self, script: &str) -> Self {
        self.headless_pointer = Some(script.to_owned());
        self
    }

    /// The display the run answers with (atld `--display`, `WxH@hz,dpi`).
    pub fn with_display(mut self, display: String) -> Self {
        self.display = Some(display);
        self
    }

    /// The app processes' UI mode (atld `--ui`, value `gtk`): every app process presents into a GTK window. Only the `gtk` window stage sets it.
    pub fn with_ui(mut self, mode: &str) -> Self {
        self.ui = Some(mode.to_owned());
        self
    }

    /// `vars` in atld's environment, as a desktop launcher sets them (GPU offload: `__NV_PRIME_RENDER_OFFLOAD`, `DRI_PRIME`); atld's processes inherit them. Test-environment setup: the product reads none of them.
    pub fn with_desktop_env(mut self, vars: &[(String, String)]) -> Self {
        self.desktop_env = vars.to_vec();
        self
    }

    /// `vars` (`KEY=VALUE`) in every app process's environment, atld `--app-env`: test-environment setup (e.g. `LD_DEBUG=bindings`); the product reads none of them.
    pub fn with_app_env(mut self, vars: &[String]) -> Self {
        self.app_env = vars.to_vec();
        self
    }

    /// The app processes' composer breaks `rule` (a deliberate-failure run): the staged ART lib dir's app runtime (the launcher's cdylib, which holds the composer) is replaced by one built with atl-composer feature `break-<rule>` (packages.rs `build_with_breaks`).
    /// stage_art restages that dir on every run (as [`Self::without_art_lib`]), so the break lasts one run; the shipped library is never touched.
    pub fn with_ui_break(self, root: &Path, rule: &str) -> Result<Self> {
        let launcher = self
            .art
            .launcher
            .file_name()
            .and_then(|n| n.to_str())
            .context("the launcher's file name")?;
        let release = packages::build_with_breaks(
            root,
            &self.out,
            launcher,
            &[format!("atl-composer/break-{rule}")],
        )?;
        let at = self.art.lib_path.join(crate::launcher_closure::RUNTIME);
        std::fs::remove_file(&at).with_context(|| format!("unlinking {}", at.display()))?;
        std::fs::copy(release.join(crate::launcher_closure::RUNTIME), &at)
            .with_context(|| format!("placing the faulted {}", at.display()))?;
        Ok(self)
    }

    /// Apps start with `--no-audio-service`: they serve neither AudioFlinger nor AudioPolicyService (audio_app.rs `--break-serve`, test only).
    pub fn without_audio_service(mut self) -> Self {
        self.audio_service = false;
        self
    }

    /// Every device ABI maps to the host's instruction set, so the broker passes the host ISA for a foreign-ABI app (run-apk `--break-isa`, test only): its translated-ISA check must go red.
    pub fn with_broken_instruction_set(mut self) -> Self {
        let host = self.abis[0].isa.clone();
        for a in &mut self.abis {
            a.isa.clone_from(&host);
        }
        self
    }

    /// Apps start with `--break-cblk`: their AudioFlinger allocates each track's region one buffer short, so createTrack refuses it (audio_app.rs `--break-cblk`, test only).
    pub fn with_broken_cblk(mut self) -> Self {
        self.audio_break_cblk = true;
        self
    }

    /// Apps start with `--no-media-player-service`: they serve no media.player (app.rs `--media-player-break`, test only).
    pub fn without_media_player_service(mut self) -> Self {
        self.media_player_service = false;
        self
    }

    /// Apps start with `--break-mix`: their AudioFlinger's mixer plays silence, so no track reaches the sink (audio_app.rs `--break-mix`, test only).
    pub fn with_broken_mix(mut self) -> Self {
        self.audio_break_mix = true;
        self
    }

    /// atld starts with `PIPEWIRE_REMOTE` = `socket`: the desktop's audio server is that socket (audio_app.rs `--unreachable`, test only).
    pub fn with_pipewire_remote(mut self, socket: PathBuf) -> Self {
        self.pipewire_remote = Some(socket);
        self
    }

    /// Atld's WAYLAND_DISPLAY, an absolute socket path, which the app processes inherit: the window gate's proxy.
    pub fn with_wayland(mut self, socket: &Path) -> Self {
        self.wayland = Some(socket.to_owned());
        self
    }

    /// Apps start without the app seccomp filter (atld `--no-app-seccomp`): the deliberate-failure check of the thread-priority rule (seccomp.rs).
    pub fn without_app_seccomp(mut self) -> Self {
        self.app_seccomp = false;
        self
    }

    /// atld `--break-render-path`: each render-path candidate is probed without its nodes, so the run must fall back to the baseline.
    pub fn with_broken_render_path(mut self) -> Self {
        self.break_render_path = true;
        self
    }

    /// The run places none of [`FEATURE_FILES`]: SystemConfig then holds only atld's host features, so the app gate's features row (always-present feature) must fail.
    pub fn without_feature_files(mut self) -> Self {
        self.platform_files.retain(|(host, _)| {
            host.file_name()
                .is_none_or(|n| !FEATURE_FILES.iter().any(|f| n == *f))
        });
        self
    }

    /// atld without the apps' web-process channel (no `--sandbox-join` / `--flatpak-spawn`): the app gate's WebView sandbox break.
    pub fn without_webspawn(mut self) -> Self {
        self.webspawn = false;
        self
    }

    /// atld starts with `PULSE_SERVER` = `server` (libpulse's syntax, `unix:<path>`): the desktop's pulse server, whose socket `share-sound` grants the web process.
    pub fn with_pulse_server(mut self, server: std::ffi::OsString) -> Self {
        self.pulse_server = PulseServer::Named(server);
        self
    }

    /// Every stream the run creates plays into `routing`'s sink, never the session's: atld's own PipeWire streams by target (`PIPEWIRE_PROPS`), the web process's pulse streams through the
    /// run's pulse server when `routing` has one ([`crate::audio_pcm::Routing::with_pulse`]). For runs nobody listens to: gates and driven runs.
    pub fn with_audio_routing(mut self, routing: &crate::audio_pcm::Routing) -> Self {
        self.stream_target = Some(routing.stream_props());
        if let Some(server) = routing.pulse_server() {
            self.pulse_server = PulseServer::Named(server);
        }
        self
    }

    /// atld starts with `PULSE_SERVER` naming a socket that does not exist: the desktop has no pulse server, so `share-sound` grants the web process nothing (the window gate's webview sound break).
    pub fn without_pulse_server(self) -> Self {
        self.with_pulse_server("unix:/nonexistent/atl-test/pulse/native".into())
    }

    /// Removes the web engine module from the run's library dir (staged anew by every [`Runtime::new`]): a host without WebKit, for the app gate's engine break.
    pub fn without_webview_engine(self) -> Result<Self> {
        let module = self
            .out
            .join("ui-module-lib")
            .join(crate::ui_module::WEBVIEW_MODULE_FILE);
        std::fs::remove_file(&module).with_context(|| format!("removing {}", module.display()))?;
        Ok(self)
    }

    /// The first installed package's uid on a fresh run of this runtime: [`FIRST_APP_UID`] plus one per placed [`OWN_APP_ID_ROWS`] package (with the default rows: the WebView provider takes 10001,
    /// so 10002). A row that sees another uid has caught the allocator handing out a taken or wrong id.
    pub fn first_app_uid(&self) -> u32 {
        let own = self.placed(&OWN_APP_ID_ROWS);
        FIRST_APP_UID + u32::try_from(own).expect("a handful of platform rows")
    }

    /// The platform packages this runtime places ([`PACKAGE_ROWS`]), each a system package of the boot scan (PackageManagerService scanSystemDirs): with the default rows the settings provider and the
    /// WebView provider, 2. A gate's own placed fixture APKs come on top.
    pub fn platform_packages(&self) -> usize {
        self.placed(&PACKAGE_ROWS)
    }

    /// How many of `rows` (install paths below a partition) the run places.
    fn placed(&self, rows: &[&str]) -> usize {
        self.platform_files
            .iter()
            .filter(|(_, at)| rows.iter().any(|row| at.ends_with(&format!("/{row}"))))
            .count()
    }

    /// The billing-unavailable desktop adaptation (M62 D1; `run-apk --billing-unavailable`): the run places the responder `com.android.vending` ([`vending_row`]), so a Play Billing client binds it
    /// and reads "billing unavailable". Off unless a run asks: without it the run holds no vending package, exact AOSP without Play.
    pub fn with_billing_unavailable(mut self) -> Result<Self> {
        self.platform_files.push(vending_row(&self.out)?);
        Ok(self)
    }

    /// The WebView provider break (`test app --webview-provider-break`): the run places no provider package ([`webview_provider_row`]), so webviewupdate has no current package.
    pub fn without_webview_provider(mut self) -> Self {
        let at = format!("{}/{WEBVIEW_PROVIDER}", atl_android_paths::PRODUCT);
        self.platform_files.retain(|(_, a)| *a != at);
        self
    }

    /// Input method clients get no death link (atld `--no-ime-death-link`): the ime gate's death sub-gate must then fail.
    pub fn without_ime_death_link(mut self) -> Self {
        self.ime_death_link = false;
        self
    }

    /// The services refuse a start that would reuse an instance (atld `--no-new-intent`): the start-finish stage's new-intent sub-gate must then fail.
    pub fn without_new_intent(mut self) -> Self {
        self.new_intent = false;
        self
    }

    /// Starts owe their callers no result (atld `--no-activity-result`): the app gate's result sub-gate must then fail.
    pub fn without_activity_result(mut self) -> Self {
        self.activity_result = false;
        self
    }

    /// Every bindService answers 0 (atld `--no-bind-service`): the app gate's bound-service and jobscheduler lines must then fail.
    pub fn without_bind_service(mut self) -> Self {
        self.bind_service = false;
        self
    }

    /// The jobscheduler never runs a job (atld `--no-job-runner`): the app gate's jobscheduler line must then fail.
    pub fn without_job_runner(mut self) -> Self {
        self.job_runner = false;
        self
    }

    /// An unhandled Back finishes nothing (atld `--no-back-finish`): the dialog gate's back sub-gate must then fail.
    pub fn without_back_finish(mut self) -> Self {
        self.back_finish = false;
        self
    }

    /// An unhandled root Back closes the root (atld `--root-back close`): the dialog gate's minimize sub-gate must then fail.
    pub fn with_root_back_close(mut self) -> Self {
        self.root_back_close = true;
        self
    }

    /// The services do not register `sensorservice` (atld `--no-sensor-service`): SensorManager's wait must then be detected.
    pub fn without_sensor_service(mut self) -> Self {
        self.sensor_service = false;
        self
    }

    /// The services do not register `name` (atld `--omit-service`): its manager must then be null in the app.
    pub fn without_service(mut self, name: &str) -> Self {
        self.omitted_services.push(name.to_owned());
        self
    }

    /// atld `--mouse-touch true`: the left mouse button reaches the app as one finger (the desktop adaptation atld has on by default; a run without this asserts AOSP's mouse).
    pub fn with_mouse_touch(mut self) -> Self {
        self.mouse_touch = true;
        self
    }

    /// atld `--esc-back true`: the keyboard's map gains the Esc -> Back fallback (atld's default; a run without this asserts AOSP's map).
    pub fn with_esc_back(mut self) -> Self {
        self.esc_back = true;
        self
    }

    /// atld `--exit-with-last-app true`: the run ends once the last app ended (atld's default with `--ui`; a run without this keeps atld until `stop`).
    pub fn with_exit_with_last_app(mut self) -> Self {
        self.exit_with_last_app = true;
        self
    }

    /// atld `--tilt-keys` (the desktop key tilt) with `extra` atld args, e.g. `--tilt-max-deg 30`.
    pub fn with_tilt_keys(mut self, extra: &[String]) -> Self {
        self.tilt_keys = Some(extra.to_vec());
        self
    }

    /// atld (and so every process it starts) sees `sys_bus` as `/sys/bus`, `udev_data` as `/run/udev/data` (the udev database), and the host's `/dev` plus each `(name, host node)` as `/dev/<name>`.
    pub fn with_fake_devices(
        mut self,
        sys_bus: PathBuf,
        udev_data: PathBuf,
        nodes: Vec<(String, PathBuf)>,
    ) -> Self {
        self.fake_devices = Some(FakeDevices {
            sys_bus,
            udev_data,
            nodes,
        });
        self
    }

    /// An offline run (scenario `network offline`): atld and every process it starts see a network namespace with loopback up and nothing else -> no default route, so the product's host network
    /// (atl-platform-host-linux network.rs) reports none and the app sees what AOSP shows a device without connectivity; connects to anything but 127.0.0.1 / ::1 fail. Harness-side setup, no product path.
    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }
}

/// A gate's throwaway data root: `<root>.data` beside the run's root (atld refuses a data root inside the root), removed with the root at each start. Keeps gates hermetic: atld's default is the user's own.
pub fn fresh_data_root(root: &Path) -> PathBuf {
    let mut name = root.file_name().unwrap_or_default().to_os_string();
    name.push(".data");
    root.with_file_name(name)
}

/// `package`'s data dirs under `data_root` as atld lays them out (crates/atld host_layout.rs): `apps/<p>/{data,data_de,external}`. Its code is `apps/<p>/apk`.
pub fn package_data_dirs(data_root: &Path, package: &str) -> [PathBuf; 3] {
    let app = data_root.join("apps").join(package);
    [app.join("data"), app.join("data_de"), app.join("external")]
}

/// `pm clear` of `package` on the persistent `data_root` (run-apk `--clear-data`): its user and device-protected data dirs and its external dirs ([`package_data_dirs`]).
/// Its code and every other package's state stay; the next launch makes the dirs again. Missing dirs are fine.
/// A name that is no package name, or a dir not strictly below `data_root`, is refused before anything is removed.
pub fn clear_package_data(data_root: &Path, package: &str) -> Result<()> {
    ensure!(
        atl_android_paths::valid_package_name(package),
        "--clear-data: {package:?} is no package name: nothing cleared"
    );
    let dirs = package_data_dirs(data_root, package);
    for dir in &dirs {
        ensure!(
            strictly_inside(data_root, dir),
            "--clear-data: {} is not below the data root: nothing cleared",
            dir.display()
        );
    }
    for dir in dirs {
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("clearing {}", dir.display()))?;
        }
    }
    Ok(())
}

/// Whether `dir` lies strictly below `device`, by plain names only: no `..`, no absolute part, not the device itself.
fn strictly_inside(device: &Path, dir: &Path) -> bool {
    dir.strip_prefix(device).is_ok_and(|rel| {
        rel.components().next().is_some()
            && rel.components().all(|c| matches!(c, Component::Normal(_)))
    })
}

/// An APK's native libraries, stored under `lib/<abi>/` as an app's packager lays them out.
pub struct NativeLibs {
    pub abi: String,
    pub libs: Vec<PathBuf>,
    /// The API level they were compiled for (`--target=<triple><api>`); it becomes the manifest's minSdkVersion, as the Android Gradle plugin injects the module's minSdk (aapt2 `--min-sdk-version` only fills a missing attribute, ManifestFixer.cpp, so such a fixture's manifest leaves minSdkVersion out).
    pub min_sdk: u32,
}

impl ApkTools {
    /// Builds the APK of `fixture` (AndroidManifest.xml and java/**.java) into `work`; `class_jars` are on javac's and d8's class path besides android.jar; `native` see [`NativeLibs`]; `assets`: a directory whose files become the APK's assets (aapt2 `-A`), `.bin` stored and the rest deflated (`-0 bin`).
    pub fn build(
        &self,
        fixture: &Path,
        work: &Path,
        class_jars: &[PathBuf],
        native: Option<&NativeLibs>,
        assets: Option<&Path>,
    ) -> Result<PathBuf> {
        if work.exists() {
            std::fs::remove_dir_all(work)?;
        }
        let classes = work.join("classes");
        let dex = work.join("dex");
        for d in [&classes, &dex] {
            std::fs::create_dir_all(d)?;
        }
        let apk = work.join("base.apk");
        let gen_dir = work.join("gen");
        let mut link = Command::new(&self.aapt2);
        link.arg("link")
            .arg("--manifest")
            .arg(fixture.join("AndroidManifest.xml"))
            .arg("-I")
            .arg(&self.android_jar)
            .arg("-o")
            .arg(&apk);
        if let Some(n) = native {
            link.arg("--min-sdk-version").arg(n.min_sdk.to_string());
        }
        if let Some(dir) = assets {
            link.arg("-A").arg(dir).args(["-0", "bin"]);
        }
        // real resources (res/): compiled, linked, and R.java generated for javac
        if fixture.join("res").is_dir() {
            let compiled = work.join("res.zip");
            host_art::exec(
                Command::new(&self.aapt2)
                    .arg("compile")
                    .arg("--dir")
                    .arg(fixture.join("res"))
                    .arg("-o")
                    .arg(&compiled),
                "aapt2 compile",
            )?;
            std::fs::create_dir_all(&gen_dir)?;
            // positional: `-R` would make it an overlay
            link.arg(&compiled).arg("--java").arg(&gen_dir);
        }
        host_art::exec(&mut link, "aapt2 link")?;
        let mut sources = Vec::new();
        if fixture.join("java").is_dir() {
            host_art::collect(&fixture.join("java"), "java", &mut sources)?;
        }
        if gen_dir.is_dir() {
            host_art::collect(&gen_dir, "java", &mut sources)?;
        }
        // no Java at all (a `hasCode="false"` app, e.g. a pure NativeActivity one): no classes.dex, as aapt2 + the NDK's packager leave it
        let manifest = std::fs::read_to_string(fixture.join("AndroidManifest.xml"))?;
        check_code_policy(&manifest, !sources.is_empty())?;
        if !sources.is_empty() {
            self.dex_into(&apk, work, &classes, &dex, class_jars, &sources)?;
        }
        let jdk_bin = self.jdk.join("bin");
        if let Some(n) = native {
            let staged = work.join("native");
            let dir = staged.join("lib").join(&n.abi);
            std::fs::create_dir_all(&dir)?;
            for l in &n.libs {
                let name = l.file_name().context("native library name")?;
                std::fs::copy(l, dir.join(name))
                    .with_context(|| format!("copying {}", l.display()))?;
            }
            // stored, so the APK also loads with extractNativeLibs=false (page-aligned mapping is bridge-1 step 7)
            host_art::exec(
                Command::new(jdk_bin.join("jar"))
                    .args(["--update", "--no-manifest", "--no-compress", "--file"])
                    .arg(&apk)
                    .arg("-C")
                    .arg(&staged)
                    .arg("lib"),
                "jar --update lib",
            )?;
        }
        zipalign(&apk)?;
        self.signer.sign(&apk)?;
        Ok(apk)
    }

    /// Compiles `sources` (javac -> jar -> d8) and stores the dex in `apk` as `classes.dex`.
    fn dex_into(
        &self,
        apk: &Path,
        work: &Path,
        classes: &Path,
        dex: &Path,
        class_jars: &[PathBuf],
        sources: &[PathBuf],
    ) -> Result<()> {
        let classpath = std::env::join_paths(std::iter::once(&self.android_jar).chain(class_jars))?;
        let jdk_bin = self.jdk.join("bin");
        host_art::exec(
            Command::new(jdk_bin.join("javac"))
                .args([
                    "-source",
                    "8",
                    "-target",
                    "8",
                    "-Xlint:-options",
                    "-bootclasspath",
                ])
                .arg(&classpath)
                .arg("-d")
                .arg(classes)
                .args(sources),
            "javac",
        )?;
        let jar = work.join("classes.jar");
        host_art::exec(
            Command::new(jdk_bin.join("jar"))
                .arg("cf")
                .arg(&jar)
                .arg("-C")
                .arg(classes)
                .arg("."),
            "jar",
        )?;
        let path = std::env::join_paths(std::iter::once(jdk_bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))?;
        let mut d8 = Command::new(&self.d8);
        d8.env("PATH", path).arg("--lib").arg(&self.android_jar);
        for lib in class_jars {
            d8.arg("--lib").arg(lib);
        }
        host_art::exec(d8.arg("--output").arg(dex).arg(&jar), "d8")?;
        // the dex stored, as the platform's packagers store it (ART maps an uncompressed dex in place)
        host_art::exec(
            Command::new(jdk_bin.join("jar"))
                .args(["--update", "--no-manifest", "--no-compress", "--file"])
                .arg(apk)
                .arg("-C")
                .arg(dex)
                .arg("classes.dex"),
            "jar --update",
        )?;
        Ok(())
    }
}

/// Stored entries' data aligned as `zipalign -p 4` leaves them: 4 bytes, a native library to the page (4096). `jar --update` drops aapt2's alignment, and the installer's parse refuses a
/// targetSdk >= 30 APK whose resources.arsc is not stored 4-aligned (ParsingPackageUtils, error -124). Entries keep their compression method.
fn zipalign(apk: &Path) -> Result<()> {
    use std::io::{Read, Write};
    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;
    /// zipalign's default alignment, and `-p`'s for `.so` entries
    const ALIGN: u16 = 4;
    const PAGE: u16 = 4096;
    let mut input = zip::ZipArchive::new(std::fs::File::open(apk)?)
        .with_context(|| format!("reading {}", apk.display()))?;
    let tmp = apk.with_extension("aligned");
    let mut out = zip::ZipWriter::new(std::fs::File::create(&tmp)?);
    for i in 0..input.len() {
        let mut entry = input.by_index(i)?;
        let name = entry.name()?.into_owned();
        let stored = entry.compression() == CompressionMethod::Stored;
        let options = if stored {
            SimpleFileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .with_alignment(if name.ends_with(".so") { PAGE } else { ALIGN })
        } else {
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
        };
        if entry.is_dir() {
            out.add_directory(name, options)?;
        } else {
            let mut data = Vec::with_capacity(usize::try_from(entry.size())?);
            entry.read_to_end(&mut data)?;
            out.start_file(name, options)?;
            out.write_all(&data)?;
        }
    }
    out.finish()?;
    std::fs::rename(&tmp, apk)?;
    Ok(())
}

impl Runtime {
    /// Every host path the runtime reads: the staged daemons and their libraries, ART, fonts, keyboards, the media stack's files, the GL driver, the gralloc HAL and the native bridge.
    fn runtime_inputs(&self) -> Result<remote::Inputs> {
        let mut inputs = art_inputs(&self.art);
        inputs.files.extend([
            self.servicemanager.clone(),
            self.services.clone(),
            self.am.clone(),
            self.screencap.clone(),
            self.binderd.clone(),
            self.sandbox_stub.clone(),
            self.sandbox_join.clone(),
            self.flatpak_spawn.clone(),
            self.atld.clone(),
            self.installer.clone(),
            self.api_impl.clone(),
            self.framework_res.clone(),
        ]);
        // a directory (atld's `--font-data`): the record takes its tree whole
        inputs.dirs.push(self.fonts.font_data.clone());
        if let Some((conf, files)) = self.pinned_fonts() {
            inputs.files.push(conf.to_path_buf());
            inputs.files.extend(files.iter().cloned());
        }
        inputs.files.extend(
            tree::KEYBOARD_FILES
                .iter()
                .map(|(f, _)| self.keychars.join(f)),
        );
        if let Some((allocator, mapper_dir)) = &self.gralloc {
            inputs.files.push(allocator.clone());
            inputs.files.extend(remote::lib_files(mapper_dir)?);
        }
        inputs
            .files
            .extend([self.health.0.clone(), self.health.1.clone()]);
        if let Some(k) = &self.keystore {
            inputs
                .files
                .extend([k.keystore2.clone(), k.keymint.clone()]);
            inputs
                .files
                .extend(k.fragments.iter().map(|(_, f)| f.clone()));
        }
        inputs.files.extend(self.gpu_probe.iter().cloned());
        inputs.files.extend(self.cpuinfo_probe.iter().cloned());
        inputs
            .files
            .extend(self.gl_driver.iter().map(|(f, _)| f.clone()));
        inputs
            .files
            .extend(self.fixture_files.iter().map(|(f, _)| f.clone()));
        inputs
            .files
            .extend(self.native_bridge_guest.iter().map(|(f, _)| f.clone()));
        inputs
            .files
            .extend(self.platform_files.iter().map(|(f, _)| f.clone()));
        inputs.files.extend(remote::lib_files(&self.native_bridge)?);
        inputs
            .files
            .extend(remote::lib_files(&self.platform_module)?);
        // the window gate's live half runs on a remote target as this binary (window.rs `agent`); a build-dir file, so the bundle maps it under `out/`
        if self.gate == "window" {
            inputs.files.push(self.harness_exe()?);
        }
        Ok(inputs)
    }

    /// This harness binary under the build dir's canonical path (`out/build`, as the record names every build-dir file): the process sees its real path (`out/pool/ws/<id>/...`, xtask out.rs `real`).
    pub fn harness_exe(&self) -> Result<PathBuf> {
        let exe = std::env::current_exe().context("the harness binary's path")?;
        Ok(match exe.strip_prefix(xtask::out::real(&self.out)) {
            Ok(rest) => self.out.join(rest),
            Err(_) => exe,
        })
    }

    /// The host roots a remote run relocates (remote.rs `Roots`).
    pub fn roots(&self) -> remote::Roots {
        remote::Roots::of(&self.out, &self.java_out, &self.tree, &self.llvm_lib)
    }

    /// Records what `run` reads for `xtask remote push` (remote.rs `write_record`): the runtime's inputs, the run's packages and boot jars. Written before atld starts, so a run that fails still leaves its record.
    pub fn record_inputs(&self, run: &Run) -> Result<()> {
        let mut inputs = self.runtime_inputs()?;
        let files = run.packages.iter().flat_map(|p| &p.files);
        inputs.files.extend(files.map(|f| f.path.clone()));
        if let Some(shell) = &run.shell {
            inputs.files.extend(shell.boot_jars.iter().cloned());
        }
        remote::write_record(
            &self.out,
            &self.gate,
            &self.java_out,
            &self.tree,
            &self.llvm_lib,
            &inputs,
        )
    }

    /// Starts atld for `run`, its stdout and stderr in `run.atld_log()`. The caller waits for it and ends it.
    /// On a remote [`remote::Site`] the same command runs on the target (remote.rs `RemoteRun::spawn`): the child is the transfer, and it ends with the run.
    pub fn start_atld(&self, run: &Run) -> Result<Child> {
        self.record_inputs(run)?;
        std::fs::create_dir_all(&run.scratch)?;
        // atld makes the root again itself, but only once it runs: a poll must not see the previous run's logs
        let fresh = self.data_root.is_none().then(|| fresh_data_root(&run.root));
        for dir in std::iter::once(&run.root).chain(fresh.as_ref()) {
            if dir.exists() {
                std::fs::remove_dir_all(dir)
                    .with_context(|| format!("removing {}", dir.display()))?;
            }
        }
        let (mut cmd, mut written) = self.atld_command(run)?;
        if let remote::Site::Remote(target) = &self.site {
            // a UI window needs the target's display (remote.rs Display: its session's socket or a nested labwc); this host's socket has no target
            ensure!(
                self.fake_devices.is_none()
                    && self.wayland.is_none()
                    && run.libraries.is_empty()
                    && self.data_root.is_none()
                    && (self.ui.is_none() || target.has_display()),
                "remote run {}: not remotable yet (sensor injection, a gate's own Wayland socket, shared libraries, a persistent data root, and a UI without a target display need a target runner)",
                run.root.display()
            );
            // fontconfig's `<dir>` and `<cachedir>` name this host's paths: the run reads a copy that the script relocates
            if let Some((pinned, _)) = self.pinned_fonts() {
                let conf = run.scratch.join("fonts.conf");
                std::fs::copy(pinned, &conf)
                    .with_context(|| format!("copying {}", pinned.display()))?;
                cmd.env("FONTCONFIG_FILE", &conf);
                written.push(conf);
            }
            let roots = self.roots();
            return target.spawn(
                &roots,
                &cmd,
                &run.root,
                &run.scratch,
                &written,
                self.pulse_server.remote_session_env(),
            );
        }
        let log_file = std::fs::File::create(run.atld_log())?;
        cmd.stdin(Stdio::null())
            .stdout(log_file.try_clone()?)
            .stderr(log_file);
        cmd.spawn().context("starting atld")
    }

    /// How a run's child ended: an error naming the transcript's last line when a remote transfer failed by itself (an exit code). A transfer the caller stopped at its limit has no code: its logs are the judge's to read, as on this host.
    /// Also an error naming each system daemon atld saw die mid-run (its `verdict: system process` lines): a daemon crash right after registering otherwise leaves no trace in the checks.
    pub fn check_end(&self, end: ExitStatus, run: &Run) -> Result<()> {
        if let remote::Site::Remote(target) = &self.site
            && end.code().is_some_and(|c| c != 0)
        {
            return Err(target.failed(&run.root, end));
        }
        let log = std::fs::read_to_string(run.atld_log()).unwrap_or_default();
        let died = system_deaths(&log);
        if !died.is_empty() {
            bail!("{}", died.join("; "));
        }
        Ok(())
    }

    /// atld's command for `run`, and the files it wrote under `run.scratch` (the package and permission files it reads).
    fn atld_command(&self, run: &Run) -> Result<(Command, Vec<PathBuf>)> {
        let mut written = Vec::new();
        let keychars: Vec<PathBuf> = tree::KEYBOARD_FILES
            .iter()
            .map(|(file, _)| self.keychars.join(file))
            .collect();
        let mut staged: Vec<&Path> = vec![
            &self.art.lib_path,
            &self.native_bridge,
            &self.platform_module,
            &self.framework_res,
        ];
        staged.extend(
            self.pinned_fonts()
                .into_iter()
                .flat_map(|(_, files)| files.iter().map(PathBuf::as_path)),
        );
        staged.extend(keychars.iter().map(PathBuf::as_path));
        staged.extend(self.gralloc.iter().map(|(_, dir)| dir.as_path()));
        staged.extend(self.gl_driver.iter().map(|(f, _)| f.as_path()));
        staged.extend(self.native_bridge_guest.iter().map(|(f, _)| f.as_path()));
        crate::licenses::refuse_test_only(&staged, &self.test_only)?;
        // the staged ART's library dir, the NativeBridge core's, the platform module's, then mapper.atl.so's (a PathBuf would take the absolute second as a replacement)
        let mut lib_path = std::ffi::OsString::from(self.art.lib_path.as_os_str());
        lib_path.push(":");
        lib_path.push(&self.native_bridge);
        lib_path.push(":");
        lib_path.push(&self.platform_module);
        if let Some((_, dir)) = &self.gralloc {
            lib_path.push(":");
            lib_path.push(dir);
        }
        // a remote target has no libc++ of its own: the bundle's `lib/` (relocated from `llvm_lib`) joins the runtime's library path there
        if matches!(self.site, remote::Site::Remote(_)) {
            lib_path.push(":");
            lib_path.push(&self.llvm_lib);
        }
        // unshare and sh both exec (no --fork): the child's pid stays atld's, so stop() still reaches it
        let mut cmd = match &self.fake_devices {
            None => Command::new(&self.atld),
            Some(fake) => {
                let saved = run.scratch.join("host-dev");
                std::fs::create_dir_all(&saved)
                    .with_context(|| format!("creating {}", saved.display()))?;
                let mut c = Command::new("unshare");
                c.args(["-Urm", "--", "sh", "-c", FAKE_KERNEL])
                    .arg(&fake.sys_bus)
                    .arg(&fake.udev_data)
                    .arg(&saved);
                for (name, host) in &fake.nodes {
                    c.arg(name).arg(host);
                }
                c.arg("--").arg(&self.atld);
                c
            }
        };
        if self.offline {
            // outermost: the fake-kernel namespace (if any) nests inside it. unshare(1) leaves lo down in a new namespace; ns-root raises it (needs iproute2's `ip`), then execs on
            let inner = cmd;
            cmd = Command::new("unshare");
            cmd.args(["-Urn", "--", "sh", "-c", OFFLINE_NET, "sh"])
                .arg(inner.get_program())
                .args(inner.get_args());
        }
        if let Some(extra) = &self.tilt_keys {
            cmd.arg("--tilt-keys").args(extra);
        }
        cmd.args([
            "--mouse-touch",
            if self.mouse_touch { "true" } else { "false" },
            "--esc-back",
            if self.esc_back { "true" } else { "false" },
            "--exit-with-last-app",
            if self.exit_with_last_app {
                "true"
            } else {
                "false"
            },
        ]);
        if !self.sensor_service {
            cmd.arg("--no-sensor-service");
        }
        for s in &self.omitted_services {
            cmd.args(["--omit-service", s]);
        }
        let placed = self
            .gl_driver
            .iter()
            .chain(&self.native_bridge_guest)
            .chain(&self.fixture_files)
            .chain(&self.platform_files);
        for (host, at) in placed {
            cmd.arg("--file").arg(format!("{}={at}", host.display()));
        }
        if let Some(probe) = &self.gpu_probe {
            cmd.arg("--gpu-probe").arg(probe);
        }
        if let Some(probe) = &self.cpuinfo_probe {
            cmd.arg("--cpuinfo-probe").arg(probe);
        }
        if let Some((allocator, _)) = &self.gralloc {
            cmd.arg("--gralloc").arg(allocator);
        }
        cmd.arg("--health")
            .arg(&self.health.0)
            .arg("--vintf-fragment")
            .arg(format!("vendor={}", self.health.1.display()));
        if let Some(k) = &self.keystore {
            cmd.arg("--keystore2")
                .arg(&k.keystore2)
                .arg("--keymint")
                .arg(&k.keymint);
            for (partition, file) in &k.fragments {
                cmd.arg("--vintf-fragment")
                    .arg(format!("{partition}={}", file.display()));
            }
        }
        cmd.arg("--root")
            .arg(&run.root)
            .arg("--binderd")
            .arg(&self.binderd)
            .arg("--sandbox-stub")
            .arg(&self.sandbox_stub);
        let data_root = self
            .data_root
            .clone()
            .unwrap_or_else(|| fresh_data_root(&run.root));
        cmd.arg("--data-root").arg(data_root);
        if self.webspawn {
            cmd.arg("--sandbox-join")
                .arg(&self.sandbox_join)
                .arg("--flatpak-spawn")
                .arg(&self.flatpak_spawn);
        }
        cmd.arg("--servicemanager")
            .arg(&self.servicemanager)
            .arg("--services")
            .arg(&self.services)
            .arg("--am")
            .arg(&self.am)
            .arg("--screencap")
            .arg(&self.screencap)
            .arg("--launcher")
            .arg(&self.art.launcher)
            .arg("--installer")
            .arg(&self.installer)
            .arg("--installer-class-path")
            .arg(&self.api_impl)
            .arg("--lib-path")
            .arg(&lib_path)
            .arg("--art-root")
            .arg(&self.art.art)
            .arg("--build-prop")
            .arg(&self.art.build_prop)
            .arg("--framework-res")
            .arg(&self.framework_res)
            .arg("--font-data")
            .arg(&self.fonts.font_data)
            // atld asks fontconfig for the desktop's fonts: the pinned set (test_fonts.rs), or for_user_launch the desktop's own config
            .envs(self.pinned_fonts().map(|(conf, _)| ("FONTCONFIG_FILE", conf)))
            .args(tree::KEYBOARD_FILES.iter().flat_map(|(file, dir)| {
                [
                    "--file".to_owned(),
                    format!(
                        "{}=/system/{dir}/{file}",
                        self.keychars.join(file).display()
                    ),
                ]
            }))
            .args(["--log-tags", &run.log_tags]);
        if let Some(script) = &self.headless_pointer {
            cmd.args(["--headless-pointer", script]);
        }
        if let Some(display) = &self.display {
            cmd.args(["--display", display]);
        }
        if let Some(mode) = &self.ui {
            // the compositor logs its `present` lines (first and power-of-two counts) at debug (atl-composer ui_window.rs): the UI runs that read them enable it; read in the app processes, so
            // it goes as --app-env (atld forwards nothing else of its own). GTK's dmabuf lines are the window gate's own (window.rs).
            cmd.args(["--ui", mode])
                .arg("--app-env")
                .arg("RUST_LOG=info,atl_composer=debug");
        }
        for var in &self.app_env {
            cmd.arg("--app-env").arg(var);
        }
        // never the user's session bus: a gate's own private bus (with_desktop_env, after this), else an address with no socket (atld's notification tier none)
        cmd.env(
            "DBUS_SESSION_BUS_ADDRESS",
            crate::session_bus::no_bus(&run.scratch),
        );
        cmd.envs(self.desktop_env.iter().map(|(k, v)| (k, v)));
        if !self.audio_service {
            cmd.arg("--no-audio-service");
        }
        if self.audio_break_cblk {
            cmd.arg("--break-cblk");
        }
        if !self.media_player_service {
            cmd.arg("--no-media-player-service");
        }
        if self.audio_break_mix {
            cmd.arg("--break-mix");
        }
        if let Some(socket) = &self.pipewire_remote {
            cmd.env("PIPEWIRE_REMOTE", socket);
        }
        self.pulse_server.apply(&mut cmd);
        if let Some(props) = &self.stream_target {
            cmd.env("PIPEWIRE_PROPS", props);
        }
        if let Some(socket) = &self.wayland {
            cmd.env("WAYLAND_DISPLAY", socket);
        }
        if self.break_render_path {
            cmd.arg("--break-render-path");
        }
        if !self.app_seccomp {
            cmd.arg("--no-app-seccomp");
        }
        if !self.ime_death_link {
            cmd.arg("--no-ime-death-link");
        }
        if !self.new_intent {
            cmd.arg("--no-new-intent");
        }
        if !self.activity_result {
            cmd.arg("--no-activity-result");
        }
        if !self.bind_service {
            cmd.arg("--no-bind-service");
        }
        if !self.job_runner {
            cmd.arg("--no-job-runner");
        }
        if !self.back_finish {
            cmd.arg("--no-back-finish");
        }
        if self.root_back_close {
            cmd.args(["--root-back", "close"]);
        }
        for jar in &self.art.boot {
            cmd.arg("--boot-jar").arg(jar);
        }
        for a in &self.abis {
            cmd.arg("--abi").arg(format!("{}={}", a.abi, a.isa));
        }
        for (i, package) in run.packages.iter().enumerate() {
            let file = run.scratch.join(format!("package-{i}.json"));
            std::fs::write(
                &file,
                serde_json::to_vec_pretty(&PackageSource {
                    files: &package.files,
                })?,
            )?;
            written.push(file.clone());
            cmd.arg("--package").arg(file);
        }
        // framework-res's manifest, read as an app's is (aapt2): the install grants from its permission definitions, the broadcast sender check from its protected broadcasts
        let platform = run.scratch.join("platform-manifest.json");
        std::fs::write(
            &platform,
            serde_json::to_vec_pretty(&manifest::read_platform(
                &self.apk.aapt2,
                &self.framework_res,
            )?)?,
        )?;
        written.push(platform.clone());
        cmd.arg("--platform-manifest").arg(platform);
        for (name, lib) in &run.libraries {
            cmd.arg("--shared-library").arg(lib.to_arg(name, &lib.file));
        }
        if let Some(p) = &run.launch {
            cmd.args(["--launch", p]);
        }
        if let Some(s) = &run.shell {
            cmd.args(["--run-class", &s.class])
                .args(["--run-uid", &s.uid.to_string()])
                .args(["--run-name", &s.name]);
            for a in &s.args {
                cmd.arg(format!("--run-arg={a}"));
            }
            for j in &s.boot_jars {
                cmd.arg("--run-boot-jar").arg(j);
            }
        }
        Ok((cmd, written))
    }
}

// The `diag:` counter lines' one parser (the services, the app processes and atld read the same lines).
pub use atl_diag::summary::{
    Counter, Event, MS_PER_S, counter_lines, counters, events, parse_stamp, show, totals,
};

/// A counter line without its ` at=<stamp>` (and any cause after it): the text the exact-match checks compare.
pub fn without_stamp(line: &str) -> &str {
    line.rsplit_once(" at=").map_or(line, |(head, _)| head)
}

/// The line activity_task.rs writes when an app reports its resume (`activityResumed`, sent after onResume returns): `services: activity resumed <class> at=<stamp>`.
const RESUMED_PREFIX: &str = "services: activity resumed";
/// The lines activity_task.rs writes on each change of the settled state (activity_stack::Machine::settled): nothing pending, or a start or lifecycle step still unacknowledged.
const SETTLED_PREFIX: &str = "services: activity settled at=";
const UNSETTLED_PREFIX: &str = "services: activity unsettled at=";

/// The activity an app reported resumed last: its class name and onResume's time.
pub struct Resumed {
    pub class: String,
    pub ms: u64,
}

/// The last resume line of the services' log, i.e. the activity the app is on now. None: no resume line, or its stamp does not parse.
pub fn last_resumed(services_log: &str) -> Option<Resumed> {
    let line = services_log
        .lines()
        .rev()
        .find(|l| l.starts_with(RESUMED_PREFIX))?;
    let (class, stamp) = line[RESUMED_PREFIX.len()..].rsplit_once(" at=")?;
    Some(Resumed {
        class: class.trim().to_owned(),
        ms: parse_stamp(stamp)?,
    })
}

/// Whether the services are settled now: the last settled-state line says so. No such line (no launch yet) is not settled.
pub fn settled(services_log: &str) -> bool {
    services_log
        .lines()
        .rev()
        .find(|l| l.starts_with(SETTLED_PREFIX) || l.starts_with(UNSETTLED_PREFIX))
        .is_some_and(|l| l.starts_with(SETTLED_PREFIX))
}

/// AOSP's install-time code policy (ScanPackageUtils.assertCodePolicy): `<application android:hasCode>` (default true) needs a dex in the APK, else INSTALL_FAILED_INVALID_APK "code is missing". A fixture without Java must say hasCode="false", so a packaged fixture is one a real device installs.
fn check_code_policy(manifest_xml: &str, has_dex: bool) -> Result<()> {
    const ANDROID: &str = "http://schemas.android.com/apk/res/android";
    let doc = roxmltree::Document::parse(manifest_xml)?;
    let declared = doc
        .descendants()
        .find(|n| n.has_tag_name("application"))
        .and_then(|a| a.attribute((ANDROID, "hasCode")))
        .is_none_or(|v| v != "false");
    anyhow::ensure!(
        has_dex || !declared,
        "the fixture has no Java but declares hasCode (default true): AOSP refuses the APK (ScanPackageUtils.assertCodePolicy: code is missing)"
    );
    Ok(())
}

/// Where atld's `PULSE_SERVER` comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PulseServer {
    /// Removed = libpulse's default socket. The gates' desktop is one audio server, PipeWire: a session PULSE_SERVER (WSLg sets unix:/mnt/wslg/PulseServer, its own PulseAudio, downstream of
    /// PipeWire) would send the web process's audio past the graph the app's own audio plays into, and leak sound to the user's server.
    GateDefault,
    /// Set to this value (libpulse's syntax): a gate that judges the sound names its own server.
    Named(std::ffi::OsString),
    /// The session's value, untouched (run-apk: the user's launch).
    Session,
}

impl PulseServer {
    fn apply(&self, cmd: &mut Command) {
        match self {
            PulseServer::GateDefault => {
                cmd.env_remove("PULSE_SERVER");
            }
            PulseServer::Named(server) => {
                cmd.env("PULSE_SERVER", server);
            }
            PulseServer::Session => {}
        }
    }

    /// The variables a remote run takes from the target's desktop session (remote.rs `session_env_script`): Session's PULSE_SERVER, as a desktop launch there inherits it. GateDefault needs no
    /// entry (the target's `env -i` leaves a removed variable absent), Named is an explicit value.
    fn remote_session_env(&self) -> &'static [&'static str] {
        match self {
            PulseServer::Session => &["PULSE_SERVER"],
            PulseServer::GateDefault | PulseServer::Named(_) => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only atld's end lines for dead daemons count; the mid-run line and other verdict-like text do not (lines as the red control's atld.log wrote them).
    #[test]
    fn system_deaths_from_atld_log() {
        let log = "atld: system process keystore2 died: exit status: 137\n\
                   diag: first system:died keystore2 1 at=1.0 cause=exit status: 137\n\
                   atld: verdict: system process keystore2 died during the run: exit status: 137\n\
                   atld: stopped keymint\n";
        assert_eq!(
            system_deaths(log),
            ["system process keystore2 died during the run: exit status: 137"]
        );
        assert!(system_deaths("atld: stopped keystore2\n").is_empty());
    }

    /// The command's PULSE_SERVER per state: Command::get_envs lists an explicit set as Some(value), an explicit removal as None, and an untouched (inherited) variable not at all.
    #[test]
    fn pulse_server_states_set_remove_or_inherit() {
        let of = |state: PulseServer| {
            let mut cmd = Command::new("true");
            state.apply(&mut cmd);
            cmd.get_envs()
                .find(|(k, _)| *k == "PULSE_SERVER")
                .map(|(_, v)| v.map(|v| v.to_owned()))
        };
        let named = std::ffi::OsString::from("unix:/run/user/1000/atl-pipewire-pulse");
        assert_eq!(of(PulseServer::Named(named.clone())), Some(Some(named)));
        assert_eq!(of(PulseServer::GateDefault), Some(None));
        assert_eq!(of(PulseServer::Session), None);
    }

    /// `pm clear` removes the package's three dirs and leaves its code and another package's dirs.
    #[test]
    fn clear_package_data_removes_only_that_package() {
        let device = std::env::temp_dir().join(format!("atl-clear-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&device);
        let made = [
            "apps/a/data/files/save.txt",
            "apps/a/data_de/cache/x",
            "apps/a/external/data/y",
            "apps/a/apk/base.apk",
            "apps/b/data/files/keep.txt",
        ];
        for f in made {
            let p = device.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        clear_package_data(&device, "a").unwrap();
        for gone in ["apps/a/data", "apps/a/data_de", "apps/a/external"] {
            assert!(!device.join(gone).exists(), "{gone} should be cleared");
        }
        for kept in ["apps/a/apk/base.apk", "apps/b/data/files/keep.txt"] {
            assert!(device.join(kept).is_file(), "{kept} should stay");
        }
        std::fs::remove_dir_all(&device).unwrap();
    }

    /// A name that is no package name is refused before anything is deleted: `..` would clear every app's external dir, an absolute name a path outside the device.
    #[test]
    fn clear_package_data_refuses_a_name_that_is_no_package() {
        let base = std::env::temp_dir().join(format!("atl-clear-refused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let device = base.join("device");
        let victim = base.join("outside").join("victim");
        let keep = device.join("apps/a/data/keep.txt");
        for f in [
            keep.clone(),
            device.join("apps/a/external/keep.txt"),
            victim.join("f"),
        ] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, b"x").unwrap();
        }
        assert!(
            clear_package_data(&device, "..").is_err(),
            "'..' was accepted"
        );
        assert!(keep.is_file(), "'..' deleted the device's data");
        let absolute = victim.to_str().unwrap();
        assert!(
            clear_package_data(&device, absolute).is_err(),
            "an absolute name was accepted"
        );
        assert!(
            victim.join("f").is_file(),
            "an absolute name deleted a path outside the device"
        );
        assert!(keep.is_file(), "an absolute name deleted the device's data");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// proc(5) stat lines: comm may hold spaces and parens (the last `)` ends it); the 4th field is ppid. And the live check: this test process's own child, `sleep`, is outside the broker.
    #[test]
    fn children_by_ppid() {
        assert_eq!(stat_comm_ppid("42 (bwrap) S 7 42 42 0"), Some(("bwrap", 7)));
        assert_eq!(stat_comm_ppid("9 (a) b) R 1 9"), Some(("a) b", 1)));
        assert_eq!(stat_comm_ppid("garbage"), None);
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        // the comm the kernel showed for the join stage in a real run ("763341 atl-sandbox-joi", webview-d2 app gate), and bwrap's whole name
        assert_eq!(comm_of("atl-sandbox-join"), "atl-sandbox-joi");
        assert_eq!(comm_of("bwrap"), "bwrap");
        let outside = children_outside_broker(std::process::id()).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        // by pid, not comm: until exec completes a child shows its forker's comm (here the test thread's name); other tests' children are listed too
        let pid = child.id().to_string();
        assert!(
            outside
                .iter()
                .any(|l| l.split(' ').next() == Some(pid.as_str())),
            "{outside:?}"
        );
    }

    /// hasCode="false" packages without a dex; the default (absent) and "true" need one; with a dex every declaration passes (AOSP checks one direction only).
    #[test]
    fn code_policy_follows_has_code() {
        let m = |attr: &str| {
            format!(
                "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" package=\"p\"><application {attr}/></manifest>"
            )
        };
        assert!(check_code_policy(&m("android:hasCode=\"false\""), false).is_ok());
        assert!(check_code_policy(&m(""), false).is_err());
        assert!(check_code_policy(&m("android:hasCode=\"true\""), false).is_err());
        for attr in ["", "android:hasCode=\"false\"", "android:hasCode=\"true\""] {
            assert!(check_code_policy(&m(attr), true).is_ok());
        }
    }

    /// The re-exported parser reads the writer's lines (atl-diag's own tests cover the policy); other lines are ignored.
    #[test]
    fn counter_lines_read_diag_lines() {
        let log = "services: ready\n\
                   diag: first unimplemented IActivityManager.getProcessLimit 1 at=100.000\n\
                   diag: count unimplemented IActivityManager.getProcessLimit 2 at=100.002\n";
        assert_eq!(
            counter_lines(log),
            ["unimplemented IActivityManager.getProcessLimit x>=2"]
        );
        assert!(counter_lines("nothing\n").is_empty());
    }

    /// Only the first started app's end counts (atld main.rs start_app / reap lines); another pid's end, or none, is not it.
    #[test]
    fn first_app_ended_follows_the_first_started_pid() {
        let started = "atld: services ready\natld: started org.x (package org.x): uid 10123, host pid 41\natld: started org.x (package org.x): uid 10123, host pid 57\n";
        assert!(!first_app_ended(started));
        assert!(!first_app_ended(&format!(
            "{started}atld: process 57 ended: signal: 9\n"
        )));
        assert!(first_app_ended(&format!(
            "{started}atld: process 41 ended: exit status: 0\n"
        )));
        assert!(!first_app_ended("atld: process 41 ended: exit status: 0\n"));
    }

    /// The last resume line gives the class and its stamp; a last line without a parsable stamp, a line of another kind, and no line at all give None.
    #[test]
    fn last_resumed_reads_the_last_resume_line() {
        let log = "services: ready\nservices: activity resumed org.x.Splash at=100.250\nservices: activity unsettled at=100.300\nservices: activity resumed org.x.Main at=101.000\nservices: activity settled at=101.001\n";
        let last = last_resumed(log).expect("two resume lines");
        assert_eq!((last.class.as_str(), last.ms), ("org.x.Main", 101_000));
        assert!(last_resumed("services: activity resumed org.x.Main\n").is_none());
        assert!(last_resumed("services: ready at=100.250\n").is_none());
        assert!(last_resumed("nothing\n").is_none());
    }

    /// The last settle line decides; a resume line alone is not settled.
    #[test]
    fn settled_is_the_last_settle_line() {
        assert!(settled(
            "services: activity unsettled at=1.0\nservices: activity settled at=2.0\n"
        ));
        assert!(!settled(
            "services: activity settled at=1.0\nservices: activity unsettled at=2.0\n"
        ));
        assert!(!settled("services: activity resumed org.x.Main at=1.0\n"));
    }
}
