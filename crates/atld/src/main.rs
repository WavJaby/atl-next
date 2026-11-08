// SPDX-License-Identifier: MIT
//! atld, M2 wiring: starts the system the Android way, each process through the broker, then serves the services' process control until the run ends (step 7).
//! 1. the runtime root `--root` (made fresh): `properties` (the default property area, atl-property-service; `properties.d/` one area per app process: the defaults plus the properties only that process has, [`Host::process_properties`]), `system/` (build.prop, `framework/` = the boot jars and shared libraries),
//!    `data/` (`app/<package>/base.apk`, `user/0/<package>/`, `dalvik-cache/`), `packages.json` (the installed packages), `log/<process>.log` (each process's stdout + stderr; a System.out line is a liblog record there).
//! 2. the install: each `--package` (an APK and the manifest xtask read from it) gets the next app uid, its data directories and `/data/app/<package>/base.apk`; the records go to `packages.json`.
//! 3. atl-binderd, its admin connection on stdin; the broker holds the other end.
//! 4. AOSP servicemanager as uid 1000 (AID_SYSTEM), the one process that may become context manager.
//! 5. atl-system-services as uid 1000 with the packages and, as its stdin, the process-control connection (atl_broker::control): it starts app processes (ActivityManager's ProcessList) and kills them, here, through
//!    the broker's sandbox. atld waits for its `services: ready`.
//! 6. `--run-class`: a command as uid `--run-uid` (the shell), `am instrument` for example; its exit ends the run.
//! 7. the control socket `<root>/control.sock` (serve_run_control): `stop` ends the run, `start <package>/<class>` is `am start`. An app process ending does not end the run: the services see its death.
//!
//! Exit status: the command's (bwrap passes it on); 0 after `stop`; failure when the services exited. Plan: binderd, servicemanager and the services move into this process later; here each is a process, as `xtask test binder` runs them.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode, ExitStatus};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_android_paths::{
    DATA, SDCARD, SHARED_STORAGE, SYSTEM, SYSTEM_FONTS, SYSTEM_FRAMEWORK, VENDOR,
};
use atl_broker::control::{self, Request, Response, StartRequest};
use atl_broker::{Broker, Link, Mount, Process, Spawn, kill_sandbox, to_log};
use atl_gralloc::vintf;
use atl_property_service::{LinearTier, Property};
use atl_services::compositor_registry;
use atl_services::display_model::DisplayModel;
use atl_services::native_libs::{self, SupportedAbi};
use atl_services::packages::{Library, Manifest, PackageRecord, PermissionDecl};
use clap::Parser;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

mod cacerts;
use serde::Deserialize;

#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// Runtime root (removed and made again).
    #[arg(long)]
    root: PathBuf,
    /// atl-binderd executable.
    #[arg(long)]
    binderd: PathBuf,
    /// atl-sandbox-stub executable: the in-child stage that applies Landlock before binderd runs.
    #[arg(long)]
    sandbox_stub: PathBuf,
    /// AOSP servicemanager executable.
    #[arg(long)]
    servicemanager: PathBuf,
    /// atl-system-services executable.
    #[arg(long)]
    services: PathBuf,
    /// atl-gralloc-allocator executable: the graphics allocator HAL (`android.hardware.graphics.allocator.IAllocator/default`), started next to the services with `/dev/dri` and declared in the VINTF manifest servicemanager reads.
    /// Omitted: no graphics buffers (headless runs on a host without a DRM device). The mapper library `mapper.atl.so` it names to libui must be in `--lib-path`.
    #[arg(long)]
    gralloc: Option<PathBuf>,
    /// atl-app-launcher executable.
    #[arg(long)]
    launcher: PathBuf,
    /// LD_LIBRARY_PATH of every process: the runtime's libraries.
    #[arg(long)]
    lib_path: OsString,
    /// The ART root (ANDROID_ROOT; com.android.art, com.android.i18n, com.android.tzdata below it).
    #[arg(long)]
    art_root: PathBuf,
    /// A boot classpath jar, in order (repeat); mounted as /system/framework/<file name>.
    #[arg(long = "boot-jar", required = true)]
    boot_jars: Vec<PathBuf>,
    /// The framework's resources, /system/framework/framework-res.apk (AssetManager.FRAMEWORK_APK_PATH: every process's system AssetManager).
    #[arg(long)]
    framework_res: PathBuf,
    /// The converter's build.prop: the property area's source, /system/build.prop.
    #[arg(long)]
    build_prop: PathBuf,
    /// AOSP's font data dir (frameworks/base/data/fonts): its alias.json and fallback_order.json, read as-is; the fonts themselves are the desktop's.
    #[arg(long)]
    font_data: PathBuf,
    /// A file of /system or /vendor besides the framework's (repeat): `<host path>=<absolute Android path>`, linked there (the GL driver: /vendor/lib64/egl/<name>).
    #[arg(long = "file", value_parser = partition_file)]
    files: Vec<(PathBuf, PathBuf)>,
    /// A shared library packages may `<uses-library>` (repeat): `<name>=<host jar>[,<dependency name>...]`, linked as /system/framework/<file name> and registered with the package manager
    /// (SystemConfig's `<library name file dependency>`).
    #[arg(long = "shared-library", value_parser = Library::parse_arg)]
    shared_libraries: Vec<(String, Library)>,
    /// A package to install (repeat): a JSON file `{"apk": <host path>, "manifest": {...}}` (atl_services::packages::Manifest, which xtask reads from the APK with aapt2).
    #[arg(long = "package", required = true)]
    packages: Vec<PathBuf>,
    /// The platform's permission definitions (framework-res's `<permission>` elements): a JSON array of atl_services::packages::PermissionDecl, the install-time grant rule's platform half.
    /// Omitted: only the packages' own definitions count, so no platform permission is granted.
    #[arg(long)]
    platform_permissions: Option<PathBuf>,
    /// The first app uid (AID_APP_START 10000 and above); the i-th package gets uid + i.
    #[arg(long)]
    first_uid: u32,
    /// liblog's ANDROID_LOG_TAGS for every app process.
    #[arg(long, default_value = "*:i")]
    log_tags: OsString,
    /// A package whose launcher activity the services start once they are up.
    #[arg(long)]
    launch: Option<String>,
    /// The display the services answer with (`WxH@hz,dpi`, atl_services::display_model): forwarded to atl-system-services; omitted, the desktop's first monitor (atl-platform-host, DisplayModel::from_monitor).
    #[arg(long, value_parser = DisplayModel::parse_arg)]
    display: Option<DisplayModel>,
    /// The headless pointer script, `down:x,y move:x,y up:x,y` (atl-composer `input::parse_script`): every app process gets it as `ro.atl.headless.pointer`, and the app injects it after its first frame. Omitted: no input.
    #[arg(long)]
    headless_pointer: Option<String>,
    /// The UI mode of every app process: `gtk` gives each one a GTK window on its main thread (delivered as `ro.atl.ui`, compositor_registry::UI_PROPERTY). Omitted: headless.
    #[arg(long, value_parser = ["gtk"])]
    ui: Option<String>,
    /// atl-am: run as the shell uid for the control command `start` (a startActivity with no caller app). Omitted: `start` is refused.
    #[arg(long)]
    am: Option<PathBuf>,
    /// A command to run once the services are up, as ActivityThread's sibling `main` class (zygoteInit runs it): the class, then `--run-arg`s. Its exit ends atld.
    #[arg(long, requires_all = ["run_uid", "run_name"])]
    run_class: Option<String>,
    #[arg(long)]
    run_uid: Option<u32>,
    /// The command's log name (`log/<name>.log`).
    #[arg(long)]
    run_name: Option<String>,
    #[arg(long = "run-arg", allow_hyphen_values = true)]
    run_args: Vec<String>,
    /// The dex jar(s) that hold --run-class (repeat): mounted as /system/framework/<file name> and appended to that command's boot classpath (zygoteInit loads the class with the boot class loader).
    #[arg(long = "run-boot-jar")]
    run_boot_jars: Vec<PathBuf>,
    /// Test only: apps start without the app seccomp filter (atl-broker seccomp.rs), the deliberate-failure check of its thread-priority rule: without the filter queued-work-looper dies with SecurityException.
    #[arg(long)]
    no_app_seccomp: bool,
    /// Test only: passed to the services (`--no-ime-death-link`), the deliberate break of the ime gate's death sub-gate.
    #[arg(long)]
    no_ime_death_link: bool,
    /// Test only: passed to the services (`--no-new-intent`), the deliberate break of the start-finish stage's new-intent sub-gate.
    #[arg(long)]
    no_new_intent: bool,
    /// Test only: passed to the services (`--no-activity-result`), the deliberate break of the app gate's result sub-gate: starts owe no result.
    #[arg(long)]
    no_activity_result: bool,
    /// Test only: passed to the services (`--no-root-back`), the deliberate break of the dialog gate's back-close sub-gate.
    #[arg(long)]
    no_root_back: bool,
    /// Test only: passed to the services (`--no-sensor-service`), the deliberate break of the sensors gate's empty stage.
    #[arg(long)]
    no_sensor_service: bool,
    /// Test only: passed to the services (`--omit-service <name>`, repeat), the deliberate break of the app gate's line for that service.
    #[arg(long)]
    omit_service: Vec<String>,
    /// Desktop tilt for accelerometer games: while an app window has focus, held Left / Right (or A / D) tilt the device, injected as accelerometer data (sensorservice HAL bypass injection); the keys still reach the app. Off: no emulation. No effect on a machine with a real accelerometer (the services probe it).
    #[arg(long)]
    tilt_keys: bool,
    /// With `--tilt-keys`: the angle a held key reaches, degrees (default: atl-services `TiltConfig::default`).
    #[arg(long, requires = "tilt_keys")]
    tilt_max_deg: Option<f32>,
    /// With `--tilt-keys`: ms from level to the full angle and back.
    #[arg(long, requires = "tilt_keys")]
    tilt_ramp_ms: Option<u32>,
    /// With `--tilt-keys`: the injected events' period, ms.
    #[arg(long, requires = "tilt_keys")]
    tilt_period_ms: Option<u32>,
    /// Test only, with `--tilt-keys`: each app's tilt injects a 90 deg probe reading until its first focus (atl-services `TiltConfig::probe_unfocused`), the misbehaving injector of the sensors gate's focus sub-gate.
    #[arg(long, requires = "tilt_keys")]
    tilt_probe_unfocused: bool,
    /// Test only: passed to the services (`--no-sensor-focus-check`), the deliberate break of the sensors gate's focus sub-gate.
    #[arg(long)]
    no_sensor_focus_check: bool,
    /// Test only: apps start with `--no-audio-service` (their launcher serves neither AudioFlinger nor AudioPolicyService), the deliberate break of the audio gate.
    #[arg(long)]
    no_audio_service: bool,
    /// Test only: apps start with `--break-cblk` (their AudioFlinger's tracks get a control block one buffer short), the audio gate's second break.
    #[arg(long)]
    break_cblk: bool,
    /// Test only: apps start with `--no-media-player-service` (their launcher serves no media.player), the deliberate break of the media player gate.
    #[arg(long)]
    no_media_player_service: bool,
    /// Test only: apps start with `--break-mix` (their AudioFlinger's mixer plays silence), the audio gate's third break.
    #[arg(long)]
    break_mix: bool,
}

/// Android uids (system/core/libcutils/include/private/android_filesystem_config.h): AID_SYSTEM, AID_APP_START.
const AID_SYSTEM: u32 = 1000;
const AID_SHELL: u32 = 2000;
const AID_APP_START: u32 = 10_000;
/// The run's control socket, below the root (serve_run_control).
const CONTROL_SOCKET: &str = "control.sock";
/// How often the run checks that the services still run while it waits for its end: a dead services process is noticed within this.
const SERVICES_POLL: Duration = Duration::from_millis(200);
/// How long the services may take to register: servicemanager's start, then two addService round trips (well under a second measured by `xtask test binder`; libbinder waits 1 s
/// per retry while the context manager is missing); 20 s for a loaded machine.
const READY: Duration = Duration::from_secs(20);
/// What atl-system-services prints once registered.
const READY_LINE: &str = "services: ready";
/// What atl-gralloc-allocator prints once registered.
const GRALLOC_READY_LINE: &str = "gralloc: ready";
/// The DRM device directory the allocator opens (atl_gralloc_service::backend::DRM_DIR); the sandbox's `/dev` is otherwise minimal.
const DRM_DIR: &str = "/dev/dri";
/// framework-res's installed name in /system/framework (AssetManager.FRAMEWORK_APK_PATH_DEVICE).
const FRAMEWORK_RES: &str = "framework-res.apk";
/// The target SDK of a process that is not an app (the shell's `am`): 0 = the platform's level, bionic's own meaning (atl-app-launcher args).
const PLATFORM_SDK: i32 = 0;

fn log(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "atld: {msg}");
}

/// The display without `--display`: the desktop's first monitor (atl-platform-host) as [`DisplayModel::from_monitor`]. No desktop or no monitor: the default, and atld says why.
/// A start value only: the window's own monitor (size, scale, rotation) replaces it at the window's first entry (registry SET_DISPLAY).
fn host_display() -> DisplayModel {
    match atl_platform_host::first_monitor() {
        Ok(Some(m)) => {
            let model = DisplayModel::from_monitor(
                m.size.width,
                m.size.height,
                m.refresh_hz,
                m.scale.get(),
                m.rotation.quarter_turns(),
            );
            log(format_args!("display: monitor {m:?} -> {}", model.to_arg()));
            model
        }
        Ok(None) => {
            log(format_args!(
                "display: the desktop lists no monitor, the default display"
            ));
            DisplayModel::default()
        }
        Err(e) => {
            log(format_args!("display: {e}, the default display"));
            DisplayModel::default()
        }
    }
}

/// A package to install, as xtask writes it.
#[derive(Deserialize)]
struct PackageSource {
    apk: PathBuf,
    manifest: Manifest,
    /// The device's ABIs, preferred first, for the native-library extraction.
    abis: Vec<SupportedAbi>,
}

/// The runtime root's layout.
struct Root {
    root: PathBuf,
}

impl Root {
    fn system(&self) -> PathBuf {
        self.root.join("system")
    }
    fn vendor(&self) -> PathBuf {
        self.root.join("vendor")
    }
    fn data(&self) -> PathBuf {
        self.root.join("data")
    }
    fn properties(&self) -> PathBuf {
        self.root.join("properties")
    }
    /// The area of the app process `name` of `uid`; a name's `:` and `/` cannot sit in a file name.
    fn process_properties(&self, uid: u32, name: &str) -> PathBuf {
        self.root
            .join("properties.d")
            .join(format!("{uid}-{}", name.replace([':', '/'], "_")))
    }
    fn log(&self, name: &str) -> PathBuf {
        self.root.join("log").join(format!("{name}.log"))
    }
    /// The app's external root (`apps/<package>/external`): `data` is its Android/data/<package>, `obb` its Android/obb/<package> (the broker mounts both, app_spawn).
    fn external(&self, package: &str) -> PathBuf {
        self.root.join("apps").join(package).join("external")
    }
}

/// `/system/framework/<file name of host>`: a link to `host` (the host's tree is visible in the sandbox), the sandbox path of the file.
fn link_into_framework(r: &Root, host: &Path) -> Result<String> {
    let name = host
        .file_name()
        .with_context(|| format!("{}: no file name", host.display()))?;
    let at = r.system().join("framework").join(name);
    ensure!(
        !at.exists(),
        "two framework files named {}",
        name.to_string_lossy()
    );
    std::os::unix::fs::symlink(std::path::absolute(host)?, &at)?;
    Ok(format!("{SYSTEM_FRAMEWORK}/{}", name.to_string_lossy()))
}

/// The system CA store below /system (cacerts.rs); the staged ART root's `etc/security/cacerts` links to it.
const CACERTS: &str = "etc/security/cacerts";

/// The system font list from the desktop's (atl-fonts): `/system/etc/font_fallback.xml` (SystemFonts.FONTS_XML) and the legacy `/system/etc/fonts.xml`, each face named `<k>/<file>` below /system/fonts.
/// Returns the host dirs of the faces: dir k is bound read-only at /system/fonts/<k> into every app (empty mount points made here, since /system is read-only inside).
/// No sans-serif on the desktop fails the start: Typeface.setSystemFontMap needs DEFAULT_FAMILY.
fn font_layout(a: &Args, r: &Root) -> Result<Vec<PathBuf>> {
    let data = |f: &str| {
        let p = a.font_data.join(f);
        std::fs::read_to_string(&p).with_context(|| p.display().to_string())
    };
    let table = atl_fonts::Table::parse(atl_fonts::FONTS_TOML)?;
    let src = atl_platform_host::PlatformFonts::new()?;
    let model = atl_fonts::build(
        &src,
        &atl_fonts::FileReader,
        &table,
        atl_fonts::parse_aliases(&data("alias.json")?)?,
        &atl_fonts::parse_order(&data("fallback_order.json")?)?,
    )?;
    let etc = r.system().join("etc");
    std::fs::create_dir_all(&etc)?;
    std::fs::write(etc.join("font_fallback.xml"), model.font_fallback_xml())?;
    std::fs::write(etc.join("fonts.xml"), model.fonts_xml())?;
    for k in 0..model.dirs.len() {
        std::fs::create_dir_all(r.system().join("fonts").join(k.to_string()))?;
    }
    Ok(model.dirs)
}

/// The desktop paths a UI app reads (atl_platform_host::app_access), read-only at their own paths. No compositor socket is an error: a UI run without one cannot show a window.
fn desktop_mounts() -> Result<Vec<Mount>> {
    use atl_platform_host::app_access;
    let socket = app_access::wayland_socket()
        .context("--ui: no compositor socket (WAYLAND_DISPLAY / XDG_RUNTIME_DIR)")?;
    Ok(std::iter::once(socket)
        .chain(app_access::fontconfig_paths()?)
        .map(|p| ro_at_own_path(&p))
        .collect())
}

/// /system/framework/<name> of each boot jar, the boot classpath as the sandbox sees it; /system's CA store from the distro bundle.
fn system_layout(a: &Args, r: &Root) -> Result<Vec<String>> {
    let framework = r.system().join("framework");
    std::fs::create_dir_all(&framework)?;
    std::fs::copy(&a.build_prop, r.system().join("build.prop"))?;
    std::os::unix::fs::symlink(
        std::path::absolute(&a.framework_res)?,
        framework.join(FRAMEWORK_RES),
    )?;
    let boot = a
        .boot_jars
        .iter()
        .map(|jar| link_into_framework(r, jar))
        .collect::<Result<Vec<_>>>()?;
    cacerts::write_store(&cacerts::distro_bundle()?, &r.system().join(CACERTS))?;
    for (host, at) in &a.files {
        let at = r.root.join(at);
        ensure!(!at.exists(), "two files at {}", at.display());
        std::fs::create_dir_all(at.parent().context("a path below a partition")?)?;
        std::os::unix::fs::symlink(std::path::absolute(host)?, &at)?;
    }
    Ok(boot)
}

/// The VINTF manifests servicemanager reads (libvintf's default paths): `/system/etc/vintf/manifest.xml` (framework) and `/vendor/etc/vintf/manifest.xml` (device), which declares the graphics HALs when `--gralloc` is given.
/// servicemanager refuses to register an undeclared @VintfStability service and answers `isDeclared` from these (libui checks it before it loads the mapper).
fn vintf_layout(a: &Args, r: &Root) -> Result<()> {
    for (dir, kind, hals) in [
        (r.system(), "framework", String::new()),
        (
            r.vendor(),
            "device",
            if a.gralloc.is_some() {
                vintf::device_hals()
            } else {
                String::new()
            },
        ),
    ] {
        let d = dir.join("etc/vintf");
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("manifest.xml"), vintf::manifest(kind, &hals))?;
    }
    Ok(())
}

/// Partitions `--file` may place files in: the root's `system` and `vendor` (Root::system, Root::vendor).
const FILE_PARTITIONS: &[&str] = &["system", "vendor"];

/// `--file`'s `<host path>=<absolute path below a FILE_PARTITIONS partition>` -> (host, path relative to the root).
fn partition_file(s: &str) -> Result<(PathBuf, PathBuf), String> {
    let (host, at) = s
        .split_once('=')
        .ok_or_else(|| format!("{s}: not <host path>=</system or /vendor path>"))?;
    let rel = Path::new(at)
        .strip_prefix("/")
        .map_err(|_| format!("{at}: not an absolute path"))?;
    let below = rel.components().count() > 1
        && rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && rel
            .components()
            .next()
            .is_some_and(|p| FILE_PARTITIONS.iter().any(|f| p.as_os_str() == *f));
    if !below {
        return Err(format!("{at}: not a file path below /system or /vendor"));
    }
    Ok((PathBuf::from(host), rel.to_path_buf()))
}

/// The install step, until the package manager installs from a real APK (the installer process: AOSP ParsingPackageUtils on ART, plan 4b): for each package its uid, `/data/app/<package>/base.apk` (the APK copied), its
/// native libraries (`lib/<isa>/`, [`native_libs::extract`]), credential- and device-protected data directories, and ART's dalvik-cache; the records the services read.
fn install(a: &Args, r: &Root) -> Result<Vec<PackageRecord>> {
    ensure!(
        a.first_uid >= AID_APP_START,
        "--first-uid {}: not an app uid",
        a.first_uid
    );
    std::fs::create_dir_all(r.data().join("dalvik-cache"))?;
    let mut records = Vec::new();
    for (i, file) in a.packages.iter().enumerate() {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let source: PackageSource =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        let name = &source.manifest.name;
        ensure!(
            !records.iter().any(|p: &PackageRecord| &p.name == name),
            "two packages named {name}"
        );
        let (app, user, user_de) = (
            format!("app/{name}"),
            format!("user/0/{name}"),
            format!("user_de/0/{name}"),
        );
        for d in [&app, &user, &user_de] {
            std::fs::create_dir_all(r.data().join(d))?;
        }
        for d in ["data", "obb"] {
            std::fs::create_dir_all(r.external(name).join(d))?;
        }
        std::fs::copy(&source.apk, r.data().join(&app).join("base.apk"))
            .with_context(|| format!("copying {}", source.apk.display()))?;
        let libs = native_libs::extract(
            &source.apk,
            &r.data().join(&app),
            &format!("{DATA}/{app}"),
            &source.abis,
        )?;
        records.push(PackageRecord {
            uid: a.first_uid + u32::try_from(i)?,
            source_dir: format!("{DATA}/{app}/base.apk"),
            primary_cpu_abi: libs.primary_cpu_abi,
            native_library_dir: libs.native_library_dir,
            data_dir: format!("{DATA}/{user}"),
            device_protected_data_dir: format!("{DATA}/{user_de}"),
            manifest: source.manifest,
            granted_permissions: vec![],
        });
    }
    // install-time grants over every definition: the platform's first, then the packages' (permission::install_grants)
    let platform: Vec<PermissionDecl> = match &a.platform_permissions {
        Some(file) => serde_json::from_str(
            &std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?,
        )
        .with_context(|| format!("parsing {}", file.display()))?,
        None => Vec::new(),
    };
    let grants: Vec<Vec<String>> = records
        .iter()
        .map(|p| {
            let defs = platform.iter().map(|d| ("android", d)).chain(
                records
                    .iter()
                    .flat_map(|o| o.permissions.iter().map(move |d| (o.name.as_str(), d))),
            );
            atl_services::permission::install_grants(&p.name, &p.requested_permissions, defs)
        })
        .collect();
    for (record, granted) in records.iter_mut().zip(grants) {
        record.granted_permissions = granted;
    }
    std::fs::write(
        r.root.join("packages.json"),
        serde_json::to_vec_pretty(&records)?,
    )?;
    Ok(records)
}

/// Writes the default property area (system processes map it) and returns the properties it holds, to which each app process's area adds its own.
/// Sources as init's property service loads them: /system/build.prop, then /vendor/build.prop when `--file` places one (the device's: `ro.hardware.egl`).
/// `tier`: the Codec2 linear allocator tier the host supports (atl_property_service::LinearTier::probe); its properties go into the set.
fn property_area(a: &Args, r: &Root, tier: LinearTier) -> Result<Vec<Property>> {
    let mut text = std::fs::read_to_string(&a.build_prop)
        .with_context(|| format!("reading {}", a.build_prop.display()))?;
    let vendor = Path::new("vendor/build.prop");
    if let Some((host, _)) = a.files.iter().find(|(_, at)| at == vendor) {
        text.push('\n');
        text += &std::fs::read_to_string(host)
            .with_context(|| format!("reading {}", host.display()))?;
    }
    let props = atl_property_service::defaults(&text, tier)?;
    std::fs::write(r.properties(), atl_property_service::area(&props)?)?;
    std::fs::create_dir(r.root.join("properties.d"))?;
    Ok(props)
}

/// What spawning a process takes beyond the process itself: everything the apps share, and the table of the processes the services started.
struct Host {
    broker: Broker,
    root: Root,
    launcher: PathBuf,
    lib: (OsString, OsString),
    log_tags: OsString,
    art_root: PathBuf,
    /// the boot classpath as the sandbox sees it
    boot: Vec<String>,
    /// the desktop's font dirs, dir k bound at /system/fonts/<k> (font_layout)
    fonts: Vec<PathBuf>,
    /// the default property set and the display model: what [`Host::process_properties`] builds an app process's area from
    defaults: Vec<Property>,
    display: DisplayModel,
    /// the headless pointer script; pushed to each app process as `ro.atl.headless.pointer`
    headless_pointer: Option<String>,
    /// the UI mode; pushed to each app process as `ro.atl.ui`
    ui: Option<String>,
    /// the key tilt (`--tilt-keys`); pushed to each app process as `ro.atl.tilt`
    tilt: Option<compositor_registry::TiltConfig>,
    /// test only: each app's launcher gets `--no-audio-service` (the audio gate's break)
    no_audio_service: bool,
    /// test only: each app's launcher gets `--break-cblk` (the audio gate's second break)
    break_cblk: bool,
    /// test only: each app's launcher gets `--no-media-player-service` (the media player gate's break)
    no_media_player_service: bool,
    /// test only: each app's launcher gets `--break-mix` (the audio gate's third break)
    break_mix: bool,
    /// the processes the services started and may kill: the host pids binderd tags them with
    controlled: Mutex<BTreeSet<i32>>,
    /// why the run ends: the `--run-class` command's exit or an explicit stop
    done: Sender<End>,
    /// atl-am (`--am`): what the control command `start` runs
    am: Option<PathBuf>,
    /// The host paths every process of the runtime reads, at the same paths (ro_at_own_path): the LD_LIBRARY_PATH dirs, the ART root, the host files /system and /vendor link to.
    shared_ro: Vec<Mount>,
    /// With `--ui`: what each app's GTK side reads from the desktop, read-only at its own path (atl_platform_host::app_access): the compositor socket, fontconfig's file and font dirs. Headless: none.
    desktop_ro: Vec<Mount>,
    /// The desktop's PipeWire socket (atl_platform_host::app_access::pipewire_socket): every app process gets it read-only at its own path and names it to its launcher (`--pipewire-socket`). None: no server, and the apps' tracks fail.
    pipewire: Option<PathBuf>,
    /// The device nodes each app process may open (set where the tier is probed, `run`).
    app_devices: Vec<PathBuf>,
    /// The installed packages: an app process mounts its own directories (package_mounts).
    packages: Vec<PackageRecord>,
}

/// A host path read-only at the same path in the sandbox: the runtime's shared host paths name themselves in both trees.
fn ro_at_own_path(p: &Path) -> Mount {
    Mount {
        host: p.to_path_buf(),
        at: p.to_path_buf(),
        writable: false,
    }
}

/// Why a run ends. An app process ending is not one: the services observe it (its binder death) and the run goes on, as system_server outlives its apps.
enum End {
    /// the `--run-class` command exited
    Command(ExitStatus),
    /// the control command `stop`
    Stop,
    /// the services exited: nothing can run any more
    Services(ExitStatus),
}

impl Host {
    /// ANDROID_ROOT & co. and the library path: the environment of an ART process (the converter's staged ART root).
    fn art_env(&self) -> Vec<(OsString, OsString)> {
        let art = &self.art_root;
        [
            ("ANDROID_ROOT", art.clone()),
            ("ANDROID_ART_ROOT", art.join("com.android.art")),
            ("ANDROID_I18N_ROOT", art.join("com.android.i18n")),
            ("ANDROID_TZDATA_ROOT", art.join("com.android.tzdata")),
            ("ANDROID_DATA", PathBuf::from(DATA)),
            ("LD_BIND_NOW", PathBuf::from("1")),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v.into_os_string()))
        .chain([
            self.lib.clone(),
            ("ANDROID_LOG_TAGS".into(), self.log_tags.clone()),
        ])
        .collect()
    }

    /// The property area of the app process `name` of `uid`: the defaults plus what only this process has. `ro.atl.display` is the display the process's compositor answers with
    /// (compositor_registry.rs), the model the services answer with too; `ro.atl.headless.pointer` is the input script, present only with `--headless-pointer`.
    fn process_properties(&self, uid: u32, name: &str) -> Result<PathBuf> {
        let mut props = self.defaults.clone();
        props.push((
            compositor_registry::DISPLAY_PROPERTY.to_owned(),
            self.display.to_arg(),
        ));
        if let Some(script) = &self.headless_pointer {
            props.push((
                compositor_registry::POINTER_PROPERTY.to_owned(),
                script.clone(),
            ));
        }
        if let Some(mode) = &self.ui {
            props.push((compositor_registry::UI_PROPERTY.to_owned(), mode.clone()));
        }
        if let Some(tilt) = &self.tilt {
            props.push((compositor_registry::TILT_PROPERTY.to_owned(), tilt.to_arg()));
        }
        let path = self.root.process_properties(uid, name);
        std::fs::write(&path, atl_property_service::area(&props)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// The launcher running `class` as zygote's child runs it (`--app`), as `uid`, in the sandbox with /system, /data and its property area; its log is `log/<name>.log` (appended to: a process
    /// that starts again keeps the earlier run's lines).
    fn app_spawn(
        &self,
        uid: u32,
        name: &str,
        target_sdk: i32,
        boot: &[String],
        class: &str,
        args: &[String],
    ) -> Result<Spawn> {
        let boot = boot.join(":");
        let launcher_args: Vec<OsString> =
            [format!("--target-sdk={target_sdk}"), "--app".to_owned()]
                .into_iter()
                .chain(
                    self.pipewire
                        .iter()
                        .map(|p| format!("--pipewire-socket={}", p.display())),
                )
                .chain(
                    self.no_audio_service
                        .then(|| "--no-audio-service".to_owned()),
                )
                .chain(self.break_cblk.then(|| "--break-cblk".to_owned()))
                .chain(
                    self.no_media_player_service
                        .then(|| "--no-media-player-service".to_owned()),
                )
                .chain(self.break_mix.then(|| "--break-mix".to_owned()))
                .chain([
                    format!("-Xbootclasspath:{boot}"),
                    format!("-Xbootclasspath-locations:{boot}"),
                    "-Xnoimage-dex2oat".to_owned(),
                    class.to_owned(),
                ])
                .chain(args.iter().cloned())
                .map(OsString::from)
                .collect();
        let (stdout, stderr) = to_log(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.log(name))?,
        )?;
        Ok(Spawn {
            uid,
            context_manager: false,
            mounts: [Mount {
                host: self.root.system(),
                at: SYSTEM.into(),
                writable: false,
            }]
            .into_iter()
            // the desktop's font dirs inside /system, after it (font_layout)
            .chain(self.fonts.iter().enumerate().map(|(k, host)| Mount {
                host: host.clone(),
                at: Path::new(SYSTEM_FONTS).join(k.to_string()),
                writable: false,
            }))
            .chain([
                // as on Android, apps read /vendor: libEGL loads the GL driver from /vendor/lib64/egl into the app
                Mount {
                    host: self.root.vendor(),
                    at: VENDOR.into(),
                    writable: false,
                },
            ])
            // the runtime's shared host paths; the package's own directories come with start_app (package_mounts)
            .chain(self.shared_ro.iter().cloned())
            .chain(self.desktop_ro.iter().cloned())
            .chain(self.pipewire.iter().map(|p| ro_at_own_path(p)))
            .collect(),
            links: vec![],
            devices: self.app_devices.clone(),
            property_area: Some(self.process_properties(uid, name)?),
            rights: vec![],
            program: self.launcher.clone(),
            args: launcher_args,
            env: self.art_env(),
            cwd: None,
            stdin: None,
            stdout,
            stderr,
        })
    }

    /// A package's own directories (the install's records name them): its APK and native libraries read-only at /data/app/<package>, its data and device-protected data read-write. Never the whole /data.
    fn package_mounts(&self, p: &PackageRecord) -> Result<Vec<Mount>> {
        let app = Path::new(&p.source_dir)
            .parent()
            .with_context(|| format!("{}: no app directory", p.source_dir))?;
        [
            (app, false),
            (Path::new(&p.data_dir), true),
            (Path::new(&p.device_protected_data_dir), true),
        ]
        .into_iter()
        .map(|(at, writable)| {
            let rel = at
                .strip_prefix(DATA)
                .with_context(|| format!("{}: not below {DATA}", at.display()))?;
            Ok(Mount {
                host: self.root.data().join(rel),
                at: at.to_path_buf(),
                writable,
            })
        })
        .collect()
    }

    /// A package's own external dirs, bound at Android/{data,obb}/<package> under the shared storage root, with /sdcard linked to that root: no permission needed, never the root itself (plan Storage/SAF).
    fn external_storage(&self, spawn: &mut Spawn, package: &str) {
        let external = self.root.external(package);
        let shared = PathBuf::from(SHARED_STORAGE);
        spawn.mounts.push(Mount {
            host: external.join("data"),
            at: shared.join("Android/data").join(package),
            writable: true,
        });
        spawn.mounts.push(Mount {
            host: external.join("obb"),
            at: shared.join("Android/obb").join(package),
            writable: true,
        });
        spawn.links.push(Link {
            target: shared,
            at: SDCARD.into(),
        });
    }

    /// Starts `spawn`; a thread reaps it. `main`: the `--run-class` command, whose end is the run's (`done`).
    fn start(self: &Arc<Self>, spawn: Spawn, main: bool, controlled: bool) -> Result<i32> {
        let Process {
            mut child,
            host_pid,
        } = self.broker.spawn(spawn)?;
        if controlled {
            self.controlled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(host_pid);
        }
        let host = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("reap-{host_pid}"))
            .spawn(move || {
                let status = child.wait();
                host.controlled
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&host_pid);
                match status {
                    Ok(status) => {
                        // bwrap, the sandbox's pid 1, reports a child killed by signal N as exit 128 + N
                        match status.code().filter(|c| *c > 128) {
                            Some(c) => log(format_args!(
                                "process {host_pid} ended: {status} (signal {})",
                                c - 128
                            )),
                            None => log(format_args!("process {host_pid} ended: {status}")),
                        }
                        if main {
                            let _ = host.done.send(End::Command(status));
                        }
                    }
                    Err(e) => log(format_args!("waiting for process {host_pid}: {e}")),
                }
            })?;
        Ok(host_pid)
    }

    /// The services' start request: ProcessList.startProcessLocked's Process.start, as an app process in the sandbox.
    fn start_app(self: &Arc<Self>, req: &StartRequest) -> Result<i32> {
        ensure!(req.uid >= AID_APP_START, "{}: not an app uid", req.uid);
        let mut spawn = self.app_spawn(
            req.uid,
            &req.process_name,
            req.target_sdk,
            &self.boot,
            &req.entry_point,
            &req.args,
        )?;
        self.external_storage(&mut spawn, &req.package);
        let record = self
            .packages
            .iter()
            .find(|p| p.name == req.package)
            .with_context(|| format!("{}: not an installed package", req.package))?;
        spawn.mounts.extend(self.package_mounts(record)?);
        spawn.rights = atl_broker::rights_of(record.granted_permissions.as_slice());
        let pid = self.start(spawn, false, true)?;
        log(format_args!(
            "started {} (package {}): uid {}, host pid {pid}",
            req.process_name, req.package, req.uid
        ));
        Ok(pid)
    }

    /// The services' kill request: only for a process they started.
    fn kill(&self, pid: i32) -> Result<()> {
        ensure!(
            self.controlled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&pid),
            "pid {pid} is not a process the services started"
        );
        kill_sandbox(pid)?;
        log(format_args!("killed process {pid}"));
        // dying: bwrap reports it a moment later, and what is killed is not "left running" at the end of the run
        self.controlled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pid);
        Ok(())
    }
}

impl Host {
    /// The control command `start <package>/<class>`: atl-am as the shell uid (AID_SHELL), a startActivity with no caller app; waits for it. Its reply line, `ok <am line>` or `error <why>`.
    fn am_start(&self, component: &str) -> String {
        let Some(am) = &self.am else {
            return "error atld has no --am".to_owned();
        };
        let log = self.root.log("am");
        let spawn = || -> Result<Spawn> {
            let (stdout, stderr) =
                to_log(OpenOptions::new().create(true).append(true).open(&log)?)?;
            Ok(Spawn {
                uid: AID_SHELL,
                context_manager: false,
                mounts: vec![],
                links: vec![],
                devices: vec![],
                property_area: Some(self.root.properties()),
                rights: vec![],
                program: am.clone(),
                args: vec!["start".into(), component.into()],
                env: vec![self.lib.clone()],
                cwd: None,
                stdin: None,
                stdout,
                stderr,
            })
        };
        let outcome = spawn()
            .and_then(|s| Ok(self.broker.spawn(s)?))
            .and_then(|mut p| Ok(p.child.wait()?));
        let last = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .rev()
            .find(|l| l.starts_with("am: "))
            .unwrap_or("")
            .to_owned();
        match outcome {
            Ok(s) if s.success() => format!("ok {last}"),
            Ok(s) => format!("error atl-am {s}: {last}"),
            Err(e) => format!("error {e:#}"),
        }
    }
}

/// The run's control socket, `<root>/control.sock` (owner decision 2026-11-06: the run ends on an explicit stop, not with an app): one command line per connection, one reply line.
/// `stop` ends the run (reply `ok`); `start <package>/<class>` starts an activity as `am start` does ([`Host::am_start`]). Anything else: `error unknown command`.
fn serve_run_control(host: Arc<Host>, listener: UnixListener) {
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let mut line = String::new();
        if BufReader::new(&conn).read_line(&mut line).is_err() {
            continue;
        }
        let line = line.trim();
        let reply = match line.split_once(' ').unwrap_or((line, "")) {
            ("stop", "") => {
                log(format_args!("control: stop"));
                let _ = host.done.send(End::Stop);
                "ok".to_owned()
            }
            ("start", component) if !component.is_empty() => {
                let reply = host.am_start(component);
                log(format_args!("control: start {component}: {reply}"));
                reply
            }
            _ => "error unknown command".to_owned(),
        };
        let _ = writeln!(conn, "{reply}");
    }
}

/// The control connection's server: answers the services' requests until they end it.
fn serve_control(host: Arc<Host>, fd: std::os::fd::OwnedFd) {
    let result = control::serve(&fd, |request| {
        let outcome = match request {
            Request::Start(req) => host.start_app(&req).map(|pid| Response::Started { pid }),
            Request::Kill { pid } => host.kill(pid).map(|()| Response::Killed),
        };
        outcome.unwrap_or_else(|e| {
            log(format_args!("control request failed: {e:#}"));
            Response::Error {
                message: format!("{e:#}"),
            }
        })
    });
    if let Err(e) = result {
        log(format_args!("process control ended: {e}"));
    }
}

/// Waits until `child`'s `log` holds `line`; fails when it exits first (naming its log, which says why) or after [`READY`].
fn wait_ready(child: &mut Child, log: &Path, line: &str, what: &str) -> Result<()> {
    let start = Instant::now();
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains(line) {
            return Ok(());
        }
        if let Some(s) = child.try_wait()? {
            bail!(
                "{what} exited {s}:\n{}\n(log {})",
                text.trim(),
                log.display()
            );
        }
        if start.elapsed() > READY {
            bail!("{what} not ready after {READY:?} (log {})", log.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run(a: &Args) -> Result<ExitCode> {
    if a.root.exists() {
        std::fs::remove_dir_all(&a.root)?;
    }
    let r = Root {
        root: std::path::absolute(&a.root)?,
    };
    std::fs::create_dir_all(r.root.join("log"))?;
    let tier = LinearTier::probe();
    log(format_args!("linear allocator tier {}", tier.name()));
    let defaults = property_area(a, &r, tier)?;
    // the GPU's DRM nodes, as AOSP grants every non-isolated app gpu_device (system/sepolicy private/app.te:393): Mesa renders into, and imports, the gralloc dmabufs through them; without them it falls back to pure swrast, which cannot import a window's buffers.
    // + the DMABUF system heap on the heap tier only (narrowest grant): Codec2 allocates linear blocks from it in the app process
    let app_devices = std::iter::once(PathBuf::from(DRM_DIR))
        .chain(
            (tier == LinearTier::DmaBufHeap)
                .then(|| PathBuf::from(atl_property_service::DMA_HEAP_SYSTEM)),
        )
        .collect();
    let boot = system_layout(a, &r)?;
    let fonts = font_layout(a, &r)?;
    vintf_layout(a, &r)?;
    let mut libraries = Vec::new();
    for (name, lib) in &a.shared_libraries {
        let installed = link_into_framework(&r, Path::new(&lib.file))?;
        libraries.push(lib.to_arg(name, &installed));
    }
    let run_boot = a
        .run_boot_jars
        .iter()
        .map(|jar| link_into_framework(&r, jar))
        .collect::<Result<Vec<_>>>()?;
    let records = install(a, &r)?;
    if let Some(p) = &a.launch
        && !records.iter().any(|rec| &rec.name == p)
    {
        bail!("--launch {p}: not an installed package");
    }
    let (broker, mut binderd) = Broker::start(
        &a.binderd,
        &a.sandbox_stub,
        std::fs::File::create(r.log("binderd"))?,
    )?;
    let broker = if a.no_app_seccomp {
        broker.without_app_seccomp()
    } else {
        broker
    };
    let lib = (OsString::from("LD_LIBRARY_PATH"), a.lib_path.clone());
    // what the runtime's environment and partitions name, read-only at the same paths in every process: the LD_LIBRARY_PATH dirs and the ART root, and the host files
    // /system and /vendor link to (boot jars, framework-res, shared libraries, --file sources)
    let mut shared_ro: Vec<Mount> = atl_broker::library_path_mounts(&a.lib_path)?;
    shared_ro.extend(atl_broker::tree_mounts(&a.art_root)?);
    // by host path, read-only: the package records (--packages) and the system dir's keyboard maps (--keychars, under it)
    let packages_json = r.root.join("packages.json");
    let system_dir = r.system();
    let linked: Vec<&Path> = std::iter::once(a.framework_res.as_path())
        .chain(std::iter::once(packages_json.as_path()))
        .chain(std::iter::once(system_dir.as_path()))
        .chain(a.boot_jars.iter().map(PathBuf::as_path))
        .chain(a.run_boot_jars.iter().map(PathBuf::as_path))
        .chain(
            a.shared_libraries
                .iter()
                .map(|(_, lib)| Path::new(&lib.file)),
        )
        .chain(a.files.iter().map(|(host, _)| host.as_path()))
        .collect();
    shared_ro.extend(linked.into_iter().map(ro_at_own_path));
    let system_spawn = |name: &str,
                        program: &Path,
                        args: Vec<OsString>,
                        cm: bool,
                        stdin: Option<std::os::fd::OwnedFd>|
     -> Result<Spawn> {
        let (stdout, stderr) = to_log(std::fs::File::create(r.log(name))?)?;
        Ok(Spawn {
            uid: AID_SYSTEM,
            context_manager: cm,
            // the VINTF manifests are servicemanager's alone to read; the others reach HALs through it
            mounts: shared_ro
                .iter()
                .cloned()
                .chain(if cm {
                    [("system", r.system()), ("vendor", r.vendor())]
                        .into_iter()
                        .map(|(at, host)| Mount {
                            host,
                            at: PathBuf::from("/").join(at),
                            writable: false,
                        })
                        .collect::<Vec<Mount>>()
                } else {
                    vec![]
                })
                .collect(),
            links: vec![],
            devices: vec![],
            property_area: Some(r.properties()),
            rights: vec![],
            program: program.to_owned(),
            args,
            env: vec![lib.clone()],
            cwd: None,
            stdin,
            stdout,
            stderr,
        })
    };
    let mut sm = broker
        .spawn(system_spawn(
            "servicemanager",
            &a.servicemanager,
            vec![],
            true,
            None,
        )?)
        .context("starting servicemanager")?;
    log(format_args!("servicemanager: host pid {}", sm.host_pid));
    let (control_ours, control_theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let (done_tx, done_rx): (Sender<End>, Receiver<End>) = channel();
    // the services and every app process answer with this one display
    let display = a.display.unwrap_or_else(host_display);
    let host = Arc::new(Host {
        app_devices,
        broker,
        launcher: a.launcher.clone(),
        lib: lib.clone(),
        log_tags: a.log_tags.clone(),
        art_root: a.art_root.clone(),
        boot,
        fonts,
        defaults,
        display,
        headless_pointer: a.headless_pointer.clone(),
        ui: a.ui.clone(),
        tilt: a.tilt_keys.then(|| {
            let d = compositor_registry::TiltConfig::default();
            compositor_registry::TiltConfig {
                max_deg: a.tilt_max_deg.unwrap_or(d.max_deg),
                ramp_ms: a.tilt_ramp_ms.unwrap_or(d.ramp_ms),
                period_ms: a.tilt_period_ms.unwrap_or(d.period_ms),
                probe_unfocused: a.tilt_probe_unfocused,
            }
        }),
        no_audio_service: a.no_audio_service,
        break_cblk: a.break_cblk,
        no_media_player_service: a.no_media_player_service,
        break_mix: a.break_mix,
        controlled: Mutex::new(BTreeSet::new()),
        done: done_tx,
        am: a.am.clone(),
        shared_ro: shared_ro.clone(),
        desktop_ro: if a.ui.is_some() {
            desktop_mounts()?
        } else {
            vec![]
        },
        pipewire: atl_platform_host::app_access::pipewire_socket(),
        packages: records,
        root: Root {
            root: r.root.clone(),
        },
    });
    // the control server answers from the moment the services exist: a start request may come with their first breath (--launch)
    {
        let host = Arc::clone(&host);
        std::thread::Builder::new()
            .name("process-control".into())
            .spawn(move || serve_control(host, control_ours))?;
    }
    let mut services_args: Vec<OsString> = vec![
        "--packages".into(),
        r.root.join("packages.json").into_os_string(),
    ];
    for library in &libraries {
        services_args.extend(["--shared-library".into(), library.into()]);
    }
    if let Some(p) = &a.launch {
        services_args.extend(["--launch".into(), p.into()]);
    }
    services_args.extend(["--display".into(), display.to_arg().into()]);
    if a.no_ime_death_link {
        services_args.push("--no-ime-death-link".into());
    }
    if a.no_new_intent {
        services_args.push("--no-new-intent".into());
    }
    if a.no_activity_result {
        services_args.push("--no-activity-result".into());
    }
    if a.no_root_back {
        services_args.push("--no-root-back".into());
    }
    if a.no_sensor_service {
        services_args.push("--no-sensor-service".into());
    }
    for s in &a.omit_service {
        services_args.extend(["--omit-service".into(), s.into()]);
    }
    if a.tilt_keys {
        services_args.extend([
            "--sensor-injection".into(),
            atl_services::sensor::TILT_INJECTOR_PACKAGE.into(),
        ]);
    }
    if a.no_sensor_focus_check {
        services_args.push("--no-sensor-focus-check".into());
    }
    // the key maps are /system files (`--file`, usr/keychars); the services read them where they lie in the root
    services_args.extend([
        "--keychars".into(),
        r.system().join("usr/keychars").into_os_string(),
    ]);
    let mut services_spawn = system_spawn(
        "services",
        &a.services,
        services_args,
        false,
        Some(control_theirs),
    )?;
    // sensorservice's source reads the machine's accelerometers (PlatformSensors), which the minimal root does not show: their sysfs and udev entries read-only, their char devices bound (the buffered tier)
    let sensors = atl_platform_host::PlatformSensors::default();
    let sensor_paths = sensors.read_paths();
    log(format_args!("services: sensor paths {sensor_paths:?}"));
    services_spawn
        .mounts
        .extend(sensor_paths.iter().map(|p| ro_at_own_path(p)));
    let sensor_devices = sensors.device_paths();
    log(format_args!("services: sensor devices {sensor_devices:?}"));
    services_spawn.devices.extend(sensor_devices);
    let mut services = host
        .broker
        .spawn(services_spawn)
        .context("starting atl-system-services")?;
    wait_ready(
        &mut services.child,
        &r.log("services"),
        READY_LINE,
        "atl-system-services",
    )?;
    log(format_args!("services ready"));
    let mut gralloc = None;
    if let Some(program) = &a.gralloc {
        let mut spawn = system_spawn("gralloc", program, vec![], false, None)?;
        spawn.devices = vec![PathBuf::from(DRM_DIR)];
        let mut p = host
            .broker
            .spawn(spawn)
            .context("starting atl-gralloc-allocator")?;
        wait_ready(
            &mut p.child,
            &r.log("gralloc"),
            GRALLOC_READY_LINE,
            "atl-gralloc-allocator",
        )?;
        log(format_args!("gralloc ready"));
        gralloc = Some(p);
    }

    if let (Some(class), Some(uid), Some(name)) = (&a.run_class, a.run_uid, &a.run_name) {
        let boot: Vec<String> = host.boot.iter().cloned().chain(run_boot).collect();
        let spawn = host.app_spawn(uid, name, PLATFORM_SDK, &boot, class, &a.run_args)?;
        let pid = host.start(spawn, true, false)?;
        log(format_args!("{name}: uid {uid}, host pid {pid}: {class}"));
    }
    // the control socket is up once everything above is: a `start` reaches running services
    let control_path = r.root.join(CONTROL_SOCKET);
    let listener = UnixListener::bind(&control_path)
        .with_context(|| format!("binding {}", control_path.display()))?;
    {
        let host = Arc::clone(&host);
        std::thread::Builder::new()
            .name("run-control".into())
            .spawn(move || serve_run_control(host, listener))?;
    }
    log(format_args!("control: {}", control_path.display()));
    let end = loop {
        match done_rx.recv_timeout(SERVICES_POLL) {
            Ok(end) => break end,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(s) = services.child.try_wait()? {
                    break End::Services(s);
                }
            }
            Err(RecvTimeoutError::Disconnected) => bail!("the run's end channel closed"),
        }
    };
    let code = match &end {
        End::Command(status) => {
            log(format_args!("command ended: {status}"));
            status.code()
        }
        End::Stop => Some(0),
        End::Services(status) => {
            log(format_args!("services ended: {status}"));
            None
        }
    };
    // what the services started dies with the run; then the services and servicemanager (their sandboxes' first processes, so everything in them). Every thread holding the host ends with its process,
    // and the broker's admin connection then closes; binderd is killed anyway, as nothing needs it any more.
    let left: Vec<i32> = host
        .controlled
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .collect();
    for pid in left {
        log(format_args!("left at the end: {pid}"));
        let _ = kill_sandbox(pid);
    }
    for p in [Some(&mut services), gralloc.as_mut(), Some(&mut sm)]
        .into_iter()
        .flatten()
    {
        let _ = kill_sandbox(p.host_pid);
        let _ = p.child.wait();
    }
    drop(host);
    let _ = kill_sandbox(binderd.host_pid);
    let _ = binderd.child.wait();
    Ok(match code {
        Some(0) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

fn main() -> ExitCode {
    let a = Args::parse();
    match run(&a) {
        Ok(c) => c,
        Err(e) => {
            log(format_args!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}
