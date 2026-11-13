// SPDX-License-Identifier: MIT
//! atld, M2 wiring: starts the system the Android way, each process through the broker, then serves the services' process control until the run ends (step 7).
//! 1. the runtime root `--root` (made fresh): `properties` (the default property area, atl-property-service; `properties.d/` one area per app process: the defaults plus the properties only that process has, [`Host::process_properties`]), `system/` (build.prop, `framework/` = the boot jars and shared libraries),
//!    `data/` (`app/<package>/{base.apk,split_<name>.apk,lib/<isa>/}`, `user/0/<package>/`, `dalvik-cache/`), `install/` (the installer's frames of each session, read by the services), `log/<process>.log` (each process's stdout + stderr).
//! 2. the install sessions: each `--package` (its APK files) is a session the services install at their start (owner option I, 2026-11-13): they own the package list; atld does installd's
//!    file work on their request (control `Install`: stage under session names (hard links where possible) + run atl-installer in the broker's installer profile; `Commit`: rename each file by its
//!    parsed split (`base.apk`, `split_<name>.apk`), then the dir to `/data/app/<package>`, create the data dirs).
//! 3. atl-binderd, its admin connection on stdin; the broker holds the other end.
//! 4. AOSP servicemanager as uid 1000 (AID_SYSTEM), the one process that may become context manager.
//! 5. atl-system-services as uid 1000 with the packages and, as its stdin, the process-control connection (atl_broker::control): it starts app processes (ActivityManager's ProcessList) and kills them, here, through
//!    the broker's sandbox. atld waits for its `services: ready`.
//! 6. `--run-class`: a command as uid `--run-uid` (the shell), `am instrument` for example; its exit ends the run.
//! 7. the control socket `<root>/control.sock` (serve_run_control): `stop` ends the run, `start <package>/<class>` is `am start`. An app process ending does not end the run: the services see its death.
//!
//! Exit status: the command's (bwrap passes it on); 0 after `stop`; failure when the services exited. Plan: binderd, servicemanager and the services move into this process later; here each is a process, as `xtask test binder` runs them.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
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
    DATA, DATA_APP, DATA_USER, DATA_USER_DE, PROPERTY_AREA, PROPERTY_AREA_ENV, SDCARD,
    SHARED_STORAGE, SYSTEM, SYSTEM_FONTS, SYSTEM_FRAMEWORK, VENDOR,
};
use atl_broker::control::{self, Request, Response, StagedName, StartRequest};
use atl_broker::{
    Broker, INSTALLER_UID, Link, Mount, Process, Spawn, gpu, kill_sandbox, to_log, tree_mounts,
};
use atl_gralloc::node::{self, NodeSource};
use atl_gralloc::vintf;
use atl_property_service::{LinearTier, Property};
use atl_services_types::compositor_registry;
use atl_services_types::display_model::DisplayModel;
use atl_services_types::gl_driver;
use atl_services_types::native_libs::LIB_DIR_NAME;
use atl_services_types::packages::Library;
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
    /// atl-egl-probe executable: with `--gralloc` and `--ui`, run once (`--display-node`) to ask the desktop's Wayland EGL display which GPU it renders on; gralloc and every app then use that node (atl_gralloc::node).
    /// Needed whenever both are given. Headless runs, and displays naming no node, keep gralloc's own trial (the baseline tier).
    #[arg(long)]
    gpu_probe: Option<PathBuf>,
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
    /// A package to install (repeat; session i, in uid order): a JSON file `{"files": [{"split": null|<name>, "path": <host path>}]}`, base first.
    #[arg(long = "package", required = true)]
    packages: Vec<PathBuf>,
    /// atl-installer executable: AOSP's package parse and native-code install in a VM of its own, run in the broker's installer profile on each session's staged dir.
    #[arg(long)]
    installer: PathBuf,
    /// The api-impl dex (its ParsingPackageUtils.Callback forwarder): the installer VM's `-Djava.class.path`.
    #[arg(long)]
    installer_class_path: PathBuf,
    /// A device ABI with its instruction set (repeat, preferred first): `<abi>=<isa>` (arches.toml row key); forwarded to the services, which map primaryCpuAbi to the instruction set.
    #[arg(long = "abi", required = true)]
    abis: Vec<String>,
    /// The platform's permission definitions (framework-res's `<permission>` elements): a JSON array of atl_services_types::packages::PermissionDecl, the install-time grant rule's platform half, forwarded to the services.
    /// Omitted: only the packages' own definitions count, so no platform permission is granted.
    #[arg(long)]
    platform_permissions: Option<PathBuf>,
    /// The first app uid, above the installer's (atl_broker::INSTALLER_UID); the i-th package gets uid + i (the services assign it).
    #[arg(long)]
    first_uid: u32,
    /// liblog's ANDROID_LOG_TAGS for every app process.
    #[arg(long, default_value = "*:i")]
    log_tags: OsString,
    /// A package whose launcher activity the services start once they are up.
    #[arg(long)]
    launch: Option<String>,
    /// The display the services answer with (`WxH@hz,dpi`, atl_services_types::display_model): forwarded to atl-system-services; omitted, the desktop's first monitor (atl-platform-host, DisplayModel::from_monitor).
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
    /// atl-screencap: run for the control command `capture` (the shell uid, a display capture of an app's window). Omitted: it is refused.
    #[arg(long)]
    screencap: Option<PathBuf>,
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
    /// A variable every app process gets (repeat): `KEY=VALUE`. The only way besides the desktop's forwarded names (atl-broker app_env::APP_ENV) a value reaches an app; e.g. a run's RUST_LOG or GDK_DEBUG. Not the allocator, the probes or the services.
    #[arg(long = "app-env", value_parser = app_env_var)]
    app_env: Vec<(OsString, OsString)>,
    /// Test only: each render-path candidate (atl-broker gpu::RENDER_PATHS) is probed without its device nodes, the deliberate break of the render-path choice: the run must fall back to the baseline, naming the probe failure.
    #[arg(long)]
    break_render_path: bool,
    /// Test only: passed to the services (`--no-ime-death-link`), the deliberate break of the ime gate's death sub-gate.
    #[arg(long)]
    no_ime_death_link: bool,
    /// Test only: passed to the services (`--no-new-intent`), the deliberate break of the start-finish stage's new-intent sub-gate.
    #[arg(long)]
    no_new_intent: bool,
    /// Test only: passed to the services (`--no-activity-result`), the deliberate break of the app gate's result sub-gate: starts owe no result.
    #[arg(long)]
    no_activity_result: bool,
    /// Test only: passed to the services (`--no-back-finish`), the deliberate break of the dialog gate's back sub-gate.
    #[arg(long)]
    no_back_finish: bool,
    /// What an unhandled Back on an app's root activity does, passed to the services: `minimize` (the window is minimized, the app keeps running; Android 12+) or `close` (the app's window closes).
    #[arg(long, value_parser = ["minimize", "close"], default_value = "minimize")]
    root_back: String,
    /// Test only: passed to the services (`--no-sensor-service`), the deliberate break of the sensors gate's empty stage.
    #[arg(long)]
    no_sensor_service: bool,
    /// Test only: passed to the services (`--omit-service <name>`, repeat), the deliberate break of the app gate's line for that service.
    #[arg(long)]
    omit_service: Vec<String>,
    /// Desktop tilt for accelerometer games: while an app window has focus, held Left / Right (or A / D) tilt the device, injected as accelerometer data (sensorservice HAL bypass injection); the keys still reach the app. Off: no emulation. No effect on a machine with a real accelerometer (the services probe it).
    #[arg(long)]
    tilt_keys: bool,
    /// Desktop adaptation: the left mouse button is one finger, its press / drag / release reach the app as touchscreen events (hover and the other buttons stay the mouse's). AOSP has no such conversion and touch-only games ignore the mouse.
    /// On by default, so touch-only apps are usable with a mouse; `--mouse-touch false` is exact AOSP input.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    mouse_touch: bool,
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
    /// Test only: every package installs with the native GL driver (no owner row, no ES1 rule), the GL selection gate's break.
    #[arg(long)]
    no_gl_selection: bool,
    /// Test only: apps start with `--break-mix` (their AudioFlinger's mixer plays silence), the audio gate's third break.
    #[arg(long)]
    break_mix: bool,
}

/// Android uids (system/core/libcutils/include/private/android_filesystem_config.h): AID_SYSTEM, AID_SHELL.
const AID_SYSTEM: u32 = 1000;
const AID_SHELL: u32 = 2000;
/// The run's control socket, below the root (serve_run_control).
const CONTROL_SOCKET: &str = "control.sock";
/// How often the run checks that the services still run while it waits for its end: a dead services process is noticed within this.
const SERVICES_POLL: Duration = Duration::from_millis(200);
/// How long the services may take to register: servicemanager's start, then two addService round trips (well under a second measured by `xtask test binder`; libbinder waits 1 s
/// per retry while the context manager is missing); 20 s for a loaded machine.
const READY: Duration = Duration::from_secs(20);
/// What each install session adds to the services' start (they install before registering): the installer measured 0.8-5.1 s cold (inst-m3/m5) plus staging copies of up to ~600 MB (an asset-pack split)
/// at a slow disk's ~10 MB/s; a loaded machine's margin.
const INSTALL_READY: Duration = Duration::from_secs(120);
/// What atl-system-services prints once registered.
const READY_LINE: &str = "services: ready";
/// What atl-gralloc-allocator prints once registered: `gralloc: ready (<DRM node>)`.
const GRALLOC_READY_LINE: &str = "gralloc: ready";
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

/// A package to install, as xtask writes it: one install session.
#[derive(Deserialize)]
struct PackageSource {
    /// base first
    files: Vec<PackageFile>,
}

/// One APK of a session. Which file is the base and what each split is called comes from the installer's parse, not from here.
#[derive(Deserialize, Clone)]
struct PackageFile {
    path: PathBuf,
}

impl PackageFile {
    /// Its session name: the source's file name, as `pm install-multiple` writes each file (PackageManagerShellCommand.doWriteSplits). It must end in `.apk`: the cluster parse sees only those
    /// (ApkLiteParseUtils.isApkFile), so another name would be dropped without a word.
    fn session_name(&self) -> Result<String> {
        let name = self
            .path
            .file_name()
            .with_context(|| format!("{}: no file name", self.path.display()))?
            .to_string_lossy()
            .into_owned();
        ensure!(
            name.ends_with(".apk"),
            "{name}: a session file must end in .apk"
        );
        Ok(name)
    }
}

/// Puts `from` at `to` without a second copy where the filesystem allows: a hard link, else a copy (another filesystem). The staged dir is mounted read-only into the installer, so nothing
/// it runs (AOSP's hole punching in the APK among it) can change the linked source.
fn stage_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::hard_link(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("staging {}", from.display()))?;
    Ok(())
}

/// `FileUtils.isValidExtFilename` as PackageInstallerSession applies it to a target name: non-empty, not `.`/`..`, no NUL or `/`.
fn valid_ext_filename(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['\0', '/'])
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
    let table = atl_fonts_table::Table::parse(atl_fonts_table::FONTS_TOML)?;
    let src = atl_platform_host::PlatformFonts::new()?;
    let model = atl_fonts::build(
        &src,
        &atl_fonts::FileReader,
        &table,
        atl_fonts_table::parse_aliases(&data("alias.json")?)?,
        &atl_fonts_table::parse_order(&data("fallback_order.json")?)?,
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

/// `--app-env`'s `KEY=VALUE`; the key non-empty.
fn app_env_var(s: &str) -> Result<(OsString, OsString), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.into(), v.into())),
        _ => Err(format!("{s}: not KEY=VALUE")),
    }
}

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

/// Whether a native library under `lib/<isa>/` DT_NEEDs [`gl_driver::ES1_LIB`]: the install-time ES1 rule (an ES1 app links it, or a JNI fixture of one does).
fn needs_es1(lib: &Path) -> Result<bool> {
    if !lib.is_dir() {
        return Ok(false);
    }
    for isa in std::fs::read_dir(lib)? {
        for entry in std::fs::read_dir(isa?.path())? {
            let so = entry?.path();
            if so.extension().is_some_and(|e| e == "so") && atl_broker::is_elf(&so) {
                let (_, needed) = atl_broker::elf_deps(&so)
                    .with_context(|| format!("reading {}", so.display()))?;
                if needed.iter().any(|n| n == gl_driver::ES1_LIB) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// installd's file work for the services' install (owner option I): the sessions' files, the installer's view, and what was staged and committed.
struct Installd {
    /// session i's files, base first (`--package` i)
    sessions: Vec<Vec<PackageFile>>,
    installer: PathBuf,
    /// VM options + `-Djava.class.path` (the api-impl dex)
    vm: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    /// read-only: the ART root, boot jars, api-impl dex at their own paths; framework-res at its Android path
    mounts: Vec<Mount>,
    /// staged sessions -> the uid they were staged for
    staged: Mutex<BTreeMap<u32, u32>>,
    /// committed packages -> uid: what a start request may name
    committed: Mutex<BTreeMap<String, u32>>,
}

/// PackageInstallerSession's uniform naming in `dir`: each staged `.apk` file renamed to its `names` entry. Every staged APK is named exactly once and every target is `base.apk` or
/// `split_<name>.apk` with a valid name; otherwise nothing is renamed. Session names end in `.apk` and target names are distinct, but a session file may already carry a target's name: the renames go
/// through a temporary name each, so no rename lands on a file still to be moved.
fn rename_staged(dir: &Path, names: &[StagedName]) -> Result<()> {
    let mut staged = BTreeSet::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if e.file_type()?.is_file() && name.ends_with(".apk") {
            staged.insert(name);
        }
    }
    let named: BTreeSet<&str> = names.iter().map(|n| n.staged.as_str()).collect();
    ensure!(
        named.len() == names.len() && named == staged.iter().map(String::as_str).collect(),
        "commit names {named:?}, staged {staged:?}: not every staged APK once"
    );
    let targets: BTreeSet<&str> = names.iter().map(|n| n.name.as_str()).collect();
    ensure!(
        targets.len() == names.len(),
        "two staged APKs under one name"
    );
    for n in names {
        let split = n
            .name
            .strip_prefix("split_")
            .and_then(|s| s.strip_suffix(".apk"));
        ensure!(
            n.name == "base.apk"
                || (split.is_some_and(valid_ext_filename) && valid_ext_filename(&n.name)),
            "{:?}: not base.apk or split_<name>.apk",
            n.name
        );
    }
    let tmp = |i: usize| dir.join(format!(".rename-{i}"));
    for (i, n) in names.iter().enumerate() {
        std::fs::rename(dir.join(&n.staged), tmp(i))?;
    }
    for (i, n) in names.iter().enumerate() {
        std::fs::rename(tmp(i), dir.join(&n.name))?;
    }
    Ok(())
}

/// A staging dir's name below /data/app (PackageInstallerService.buildTmpSessionDir: `vmdl<sessionId>.tmp`).
fn staging_name(session: u32) -> String {
    format!("vmdl{session}.tmp")
}

impl Host {
    /// Request `Install`: stages session `session` for `uid` (copies, AOSP names) and runs the installer over it in its profile; the frames file. A failed run removes the staging dir; its error carries the installer's stderr.
    fn install(&self, session: u32, uid: u32) -> Result<String> {
        ensure!(
            uid > INSTALLER_UID,
            "uid {uid}: at or below the installer's ({INSTALLER_UID}); package uids start above it"
        );
        let i = &self.installd;
        let files = i
            .sessions
            .get(usize::try_from(session)?)
            .with_context(|| format!("session {session}: no such session"))?;
        ensure!(
            !i.staged
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&session),
            "session {session}: already staged"
        );
        let name = staging_name(session);
        let host_dir = self.root.data().join("app").join(&name);
        let at = Path::new(DATA_APP).join(&name);
        std::fs::create_dir_all(&host_dir)?;
        let staged = (|| -> Result<String> {
            for f in files {
                stage_file(&f.path, &host_dir.join(f.session_name()?))?;
            }
            let lib = LIB_DIR_NAME;
            std::fs::create_dir_all(host_dir.join(lib))?;
            let install_dir = self.root.root.join("install");
            let frames = install_dir.join(format!("{session}.frames"));
            let log_path = self.root.log(&format!("installer-{session}"));
            let spawn = Spawn {
                uid: INSTALLER_UID,
                context_manager: false,
                mounts: i
                    .mounts
                    .iter()
                    .cloned()
                    .chain([
                        Mount {
                            host: host_dir.clone(),
                            at: at.clone(),
                            writable: false,
                        },
                        // the native-library root, the one writable path (NativeLibraryHelper copies into it)
                        Mount {
                            host: host_dir.join(lib),
                            at: at.join(lib),
                            writable: true,
                        },
                    ])
                    .collect(),
                links: vec![],
                devices: vec![],
                property_area: Some(self.root.properties()),
                rights: vec![],
                program: i.installer.clone(),
                args: i
                    .vm
                    .iter()
                    .cloned()
                    .chain([at.clone().into_os_string(), at.join(lib).into_os_string()])
                    .collect(),
                env: i.env.clone(),
                cwd: None,
                stdin: None,
                stdout: std::fs::File::create(&frames)?.into(),
                stderr: std::fs::File::create(&log_path)?.into(),
            };
            let start = Instant::now();
            let mut p = self
                .broker
                .spawn_installer(spawn)
                .with_context(|| format!("starting {}", i.installer.display()))?;
            let status = p.child.wait()?;
            let stderr = std::fs::read_to_string(&log_path).unwrap_or_default();
            ensure!(
                status.success(),
                "session {session}: atl-installer exited {status}: {}",
                stderr.trim()
            );
            log(format_args!(
                "install session {session}: installer {} ms",
                start.elapsed().as_millis()
            ));
            Ok(frames.to_string_lossy().into_owned())
        })();
        match staged {
            Ok(frames) => {
                i.staged
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(session, uid);
                Ok(frames)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&host_dir);
                Err(e)
            }
        }
    }

    /// Request `Commit`: session `session` (staged for `uid`) becomes `/data/app/<package>` (PackageInstallerSession's rename), with its data dirs (installd createAppData) and external dirs; whether a library needs ES1.
    fn commit(&self, session: u32, package: &str, uid: u32, names: &[StagedName]) -> Result<bool> {
        ensure!(
            !package.is_empty()
                && package
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
                && !package.starts_with('.'),
            "{package:?}: not a package name"
        );
        let i = &self.installd;
        let mut staged = i.staged.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            staged.get(&session) == Some(&uid),
            "session {session}: not staged for uid {uid}"
        );
        let mut committed = i.committed.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !committed.contains_key(package),
            "{package}: already installed"
        );
        let app = self.root.data().join("app");
        let code = app.join(package);
        let staged_dir = app.join(staging_name(session));
        rename_staged(&staged_dir, names)?;
        std::fs::rename(&staged_dir, &code)
            .with_context(|| format!("moving session {session} to {}", code.display()))?;
        for d in [format!("user/0/{package}"), format!("user_de/0/{package}")] {
            std::fs::create_dir_all(self.root.data().join(d))?;
        }
        for d in ["data", "obb"] {
            std::fs::create_dir_all(self.root.external(package).join(d))?;
        }
        let es1 = needs_es1(&code.join(LIB_DIR_NAME))?;
        staged.remove(&session);
        committed.insert(package.to_owned(), uid);
        log(format_args!("installed {package}: uid {uid}"));
        Ok(es1)
    }
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
    /// the desktop input adaptations (`--mouse-touch`, `--tilt-keys`); pushed to each app process as `ro.atl.desktop-input`
    desktop_input: compositor_registry::DesktopInput,
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
    /// atl-screencap (`--screencap`): what the control command `capture` runs
    screencap: Option<PathBuf>,
    /// The host paths every process of the runtime reads, at the same paths (ro_at_own_path): the LD_LIBRARY_PATH dirs, the ART root, the host files /system and /vendor link to.
    shared_ro: Vec<Mount>,
    /// With `--ui`: what each app's GTK side reads from the desktop, read-only at its own path (atl_platform_host::app_access): the compositor socket, fontconfig's file and font dirs. Headless: none.
    desktop_ro: Vec<Mount>,
    /// The desktop's PipeWire socket (atl_platform_host::app_access::pipewire_socket): every app process gets it read-only at its own path and names it to its launcher (`--pipewire-socket`). None: no server, and the apps' tracks fail.
    pipewire: Option<PathBuf>,
    /// The device nodes each app process may open: the run's GPU's class and the heap tier's heap (`run`).
    app_devices: Vec<PathBuf>,
    /// Variables only app processes get, after the runtime's own: the chosen render path's ([`choose_render_path`]), then `--app-env` (later wins). Never the allocator's or the services'.
    app_env: Vec<(OsString, OsString)>,
    /// The run's GPU: gralloc's DRM node and how it was chosen, pushed to each app process (atl_gralloc::node). None: no gralloc in this run.
    gpu: Option<(String, NodeSource)>,
    /// installd's role: the sessions' files, the installer, what is staged and installed (an app process mounts its package's own directories, package_mounts).
    installd: Installd,
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

/// ANDROID_ROOT & co. and the library path: the environment of an ART process (the converter's staged ART root `art`); app processes and the installer.
fn art_env(
    art: &Path,
    lib: &(OsString, OsString),
    log_tags: &OsString,
) -> Vec<(OsString, OsString)> {
    [
        ("ANDROID_ROOT", art.to_path_buf()),
        ("ANDROID_ART_ROOT", art.join("com.android.art")),
        ("ANDROID_I18N_ROOT", art.join("com.android.i18n")),
        ("ANDROID_TZDATA_ROOT", art.join("com.android.tzdata")),
        ("ANDROID_DATA", PathBuf::from(DATA)),
        ("LD_BIND_NOW", PathBuf::from("1")),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), v.into_os_string()))
    .chain([lib.clone(), ("ANDROID_LOG_TAGS".into(), log_tags.clone())])
    .collect()
}

impl Host {
    fn art_env(&self) -> Vec<(OsString, OsString)> {
        art_env(&self.art_root, &self.lib, &self.log_tags)
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
        props.push((
            compositor_registry::DESKTOP_INPUT_PROPERTY.to_owned(),
            self.desktop_input.to_arg(),
        ));
        if let Some((node, source)) = &self.gpu {
            props.push((node::RENDER_NODE_PROPERTY.to_owned(), node.clone()));
            props.push((
                node::NODE_SOURCE_PROPERTY.to_owned(),
                source.name().to_owned(),
            ));
        }
        let path = self.root.process_properties(uid, name);
        std::fs::write(&path, atl_property_service::area(&props)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// The launcher running `class` as zygote's child runs it (`--app`), as `uid`, in the sandbox with /system, /data and its property area; its log is `log/<name>.log` (appended to: a process
    /// that starts again keeps the earlier run's lines). `instruction_set`: an app package's ([`InstructionSets`]), passed as zygote args carry it; None for a process of no package (the shell's), which runs
    /// the runtime's own ISA.
    #[allow(clippy::too_many_arguments)]
    fn app_spawn(
        &self,
        uid: u32,
        name: &str,
        target_sdk: i32,
        instruction_set: Option<&str>,
        boot: &[String],
        class: &str,
        args: &[String],
    ) -> Result<Spawn> {
        let boot = boot.join(":");
        let launcher_args: Vec<OsString> =
            [format!("--target-sdk={target_sdk}"), "--app".to_owned()]
                .into_iter()
                .chain(instruction_set.map(|isa| format!("--instruction-set={isa}")))
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
            env: self
                .art_env()
                .into_iter()
                .chain(self.app_env.iter().cloned())
                .collect(),
            cwd: None,
            stdin: None,
            stdout,
            stderr,
        })
    }

    /// A package's own directories (as commit made them): its APKs and native libraries read-only at /data/app/<package>, its data and device-protected data read-write. Never the whole /data.
    fn package_mounts(&self, package: &str) -> Result<Vec<Mount>> {
        [
            (format!("{DATA_APP}/{package}"), false),
            (format!("{DATA_USER}/0/{package}"), true),
            (format!("{DATA_USER_DE}/0/{package}"), true),
        ]
        .into_iter()
        .map(|(at, writable)| {
            let rel = Path::new(&at)
                .strip_prefix(DATA)
                .with_context(|| format!("{at}: not below {DATA}"))?;
            Ok(Mount {
                host: self.root.data().join(rel),
                at: PathBuf::from(&at),
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
        let uid = self
            .installd
            .committed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&req.package)
            .copied()
            .with_context(|| format!("{}: not an installed package", req.package))?;
        ensure!(
            uid == req.uid,
            "{}: installed as uid {uid}, asked for {}",
            req.package,
            req.uid
        );
        let mut spawn = self.app_spawn(
            req.uid,
            &req.process_name,
            req.target_sdk,
            Some(&req.instruction_set),
            &self.boot,
            &req.entry_point,
            &req.args,
        )?;
        self.external_storage(&mut spawn, &req.package);
        spawn.mounts.extend(self.package_mounts(&req.package)?);
        spawn.rights = req.rights.clone();
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

    /// The control command `capture <package> <out>`: the package's window picture, RGBA rows, is written to `out` by atl-screencap run as the shell uid; waits for it.
    /// Reply `ok <screencap line>` or `error <why>`.
    fn capture(&self, args: &str) -> String {
        match args.split_once(' ') {
            Some((package, out)) if !package.is_empty() && !out.is_empty() => {
                let reply = self.screencap(package, out);
                log(format_args!("control: capture {package}: {reply}"));
                reply
            }
            _ => "error capture takes <package> <out>".to_owned(),
        }
    }

    fn screencap(&self, package: &str, out: &str) -> String {
        let Some(screencap) = &self.screencap else {
            return "error atld has no --screencap".to_owned();
        };
        let log = self.root.log("screencap");
        let spawn = || -> Result<Spawn> {
            // the picture is stdout, the file opened here (the sandbox has no mount of `out`); the log is stderr
            let (_, stderr) = to_log(OpenOptions::new().create(true).append(true).open(&log)?)?;
            Ok(Spawn {
                uid: AID_SHELL,
                context_manager: false,
                mounts: vec![],
                links: vec![],
                devices: vec![],
                property_area: Some(self.root.properties()),
                rights: vec![],
                program: screencap.clone(),
                args: vec![package.into()],
                env: vec![self.lib.clone()],
                cwd: None,
                stdin: None,
                stdout: std::fs::File::create(out)?.into(),
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
            .find(|l| l.starts_with("screencap: "))
            .unwrap_or("")
            .to_owned();
        match outcome {
            Ok(s) if s.success() => format!("ok {last}"),
            Ok(s) => format!("error atl-screencap {s}: {last}"),
            Err(e) => format!("error {e:#}"),
        }
    }
}

/// The run's control socket, `<root>/control.sock` (owner decision 2026-11-06: the run ends on an explicit stop, not with an app): one command line per connection, one reply line.
/// `stop` ends the run (reply `ok`); `start <package>/<class>` starts an activity as `am start` does ([`Host::am_start`]); `capture <package> <out>` writes the package's window picture to `out` ([`Host::capture`]). Anything else: `error unknown command`.
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
            ("capture", rest) => host.capture(rest),
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
            Request::Install { session, uid } => host
                .install(session, uid)
                .map(|frames| Response::Staged { frames }),
            Request::Commit {
                session,
                package,
                uid,
                names,
            } => host
                .commit(session, &package, uid, &names)
                .map(|needs_es1| Response::Committed { needs_es1 }),
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

/// The node in gralloc's ready line (`gralloc: ready (<node>)`).
fn gralloc_node(log: &str) -> Option<String> {
    let rest = log
        .lines()
        .find_map(|l| l.strip_prefix(GRALLOC_READY_LINE))?;
    let node = rest.trim().strip_prefix('(')?.strip_suffix(')')?;
    Some(node.to_owned())
}

/// Waits until `child`'s `log` holds `line`; fails when it exits first (naming its log, which says why) or after `limit`.
fn wait_ready(
    child: &mut Child,
    log: &Path,
    line: &str,
    what: &str,
    limit: Duration,
) -> Result<()> {
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
        if start.elapsed() > limit {
            bail!("{what} not ready after {limit:?} (log {})", log.display());
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
    ensure!(
        a.first_uid > INSTALLER_UID,
        "--first-uid {}: at or below the installer's uid {INSTALLER_UID}",
        a.first_uid
    );
    std::fs::create_dir_all(r.data().join("dalvik-cache"))?;
    // the install sessions: their files stay where xtask put them until staged; the installer's frames go to install/ for the services
    let install_dir = r.root.join("install");
    std::fs::create_dir_all(&install_dir)?;
    let mut sessions = Vec::new();
    let mut session_args: Vec<OsString> = Vec::new();
    for (i, file) in a.packages.iter().enumerate() {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let source: PackageSource =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        let names = source
            .files
            .iter()
            .map(PackageFile::session_name)
            .collect::<Result<BTreeSet<_>>>()?;
        ensure!(
            !source.files.is_empty() && names.len() == source.files.len(),
            "{}: no files, or two with one file name (a session holds each name once)",
            file.display()
        );
        session_args.extend(["--session".into(), i.to_string().into()]);
        sessions.push(source.files);
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
    // by host path, read-only: the system dir's keyboard maps (--keychars, under it)
    let system_dir = r.system();
    let linked: Vec<&Path> = std::iter::once(a.framework_res.as_path())
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
    // gralloc before any app can start (a --launch request may come with the services' first breath): the node it allocates on goes into every app's properties
    let mut gralloc = None;
    let mut gpu = None;
    if let Some(program) = &a.gralloc {
        let display_node = match (&a.ui, &a.gpu_probe) {
            (None, _) => Err("headless run".to_owned()),
            (Some(_), None) => bail!("--gralloc with --ui needs --gpu-probe (atl-egl-probe)"),
            (Some(_), Some(probe)) => {
                let mut spawn = system_spawn(
                    "gpu-probe",
                    probe,
                    vec!["--display-node".into()],
                    false,
                    None,
                )?;
                // every GPU's class: the probe asks which one the display picks, and NVIDIA's EGL opens its nodes even to decline
                spawn.devices = gpu::all_device_classes().context("the GPUs' device nodes")?;
                log(format_args!("gpu-probe: devices {:?}", spawn.devices));
                spawn.mounts.extend(desktop_mounts()?);
                let mut p = broker
                    .spawn(spawn)
                    .with_context(|| format!("starting {}", probe.display()))?;
                let status = p.child.wait()?;
                let text = std::fs::read_to_string(r.log("gpu-probe"))?;
                let (node, renderer) = node::parse_display_line(&text)
                    .filter(|_| status.success())
                    .with_context(|| {
                        format!(
                            "{} --display-node exited {status}:\n{}",
                            probe.display(),
                            text.trim()
                        )
                    })?;
                let modifiers = node::parse_modifiers_lines(&text);
                node.map(|n| (n, renderer.clone(), modifiers))
                    .ok_or(format!("renderer {renderer}"))
            }
        };
        let mut spawn = system_spawn("gralloc", program, vec![], false, None)?;
        // the display's GPU only; the trial tier tries every node, so it sees every GPU
        spawn.devices = match &display_node {
            Ok((n, _, modifiers)) => {
                spawn.args = vec!["--drm-node".into(), n.into()];
                for (fourcc, mods) in modifiers {
                    let arg = node::modifier_arg(*fourcc, mods);
                    // own prefix: `gpu:` is the one node line the run-apk gate reads
                    log(format_args!("gpu-modifiers: {arg} (probe)"));
                    spawn.args.push("--modifiers".into());
                    spawn.args.push(arg.into());
                }
                gpu::device_class(Path::new(n))
            }
            Err(_) => gpu::all_device_classes(),
        }
        .context("gralloc's device nodes")?;
        let mut p = broker
            .spawn(spawn)
            .context("starting atl-gralloc-allocator")?;
        wait_ready(
            &mut p.child,
            &r.log("gralloc"),
            GRALLOC_READY_LINE,
            "atl-gralloc-allocator",
            READY,
        )?;
        let allocated = gralloc_node(&std::fs::read_to_string(r.log("gralloc"))?)
            .context("gralloc's ready line names no node")?;
        log(format_args!("gralloc ready"));
        gralloc = Some(p);
        let source = match display_node {
            Ok((n, renderer, _)) => {
                ensure!(
                    allocated == n,
                    "gralloc allocates on {allocated}, the display renders on {n}"
                );
                log(format_args!("gpu: node {n} (display, renderer {renderer})"));
                NodeSource::Display
            }
            Err(why) => {
                log(format_args!(
                    "gpu: display reports no node ({why}); allocator node {allocated} by trial"
                ));
                NodeSource::Trial
            }
        };
        gpu = Some((allocated, source));
    }
    let (control_ours, control_theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let (done_tx, done_rx): (Sender<End>, Receiver<End>) = channel();
    // the services and every app process answer with this one display
    let display = a.display.unwrap_or_else(host_display);
    // the run's GPU's device class (gpu::device_class), as AOSP grants an app gpu_device per device (system/sepolicy private/app.te:393): the host EGL driver renders into, and imports, the gralloc dmabufs through them;
    // without them it falls back to the software device, which cannot import a window's buffers. No gralloc: no chosen GPU, every GPU's class.
    // + the DMABUF system heap on the heap tier only (narrowest grant): Codec2 allocates linear blocks from it in the app process
    let mut app_devices = match &gpu {
        Some((n, _)) => gpu::device_class(Path::new(n)),
        None => gpu::all_device_classes(),
    }
    .context("the apps' device nodes")?;
    if tier == LinearTier::DmaBufHeap {
        app_devices.push(PathBuf::from(atl_property_service::DMA_HEAP_SYSTEM));
    }
    // the render path: baseline (Mesa's default on the run's GPU) unless a kernel-offered candidate initialises and renders on something else (gpu::RENDER_PATHS); one `gpu-render: path` line records which ran
    let mut app_env = Vec::new();
    if let (Some((n, _)), Some(probe)) = (&gpu, &a.gpu_probe) {
        let probe_renderer = |name: &str,
                              devices: Vec<PathBuf>,
                              env: Vec<(OsString, OsString)>|
         -> Result<std::result::Result<(String, String), String>> {
            let mut spawn = system_spawn(
                name,
                probe,
                vec!["--renderer".into(), "--render".into(), n.into()],
                false,
                None,
            )?;
            spawn.devices = devices;
            spawn.env.extend(env);
            spawn.mounts.extend(desktop_mounts()?);
            let mut p = broker
                .spawn(spawn)
                .with_context(|| format!("starting {}", probe.display()))?;
            let status = p.child.wait()?;
            let text = std::fs::read_to_string(r.log(name))?;
            Ok(atl_gralloc::node::parse_renderer_line(&text)
                .filter(|_| status.success())
                .ok_or_else(|| format!("probe exited {status}: {}", text.trim())))
        };
        let candidates = gpu::render_paths();
        let chosen = if candidates.is_empty() {
            Err("no kernel render path offered".to_owned())
        } else {
            let baseline = probe_renderer("gpu-render-baseline", app_devices.clone(), vec![])?;
            let mut chosen = Err(format!("baseline {baseline:?}"));
            for path in candidates {
                let mut devices = app_devices.clone();
                if !a.break_render_path {
                    devices.extend(path.nodes.iter().map(PathBuf::from));
                }
                let env: Vec<(OsString, OsString)> = path
                    .env
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect();
                let name = format!("gpu-render-{}", path.module);
                match (probe_renderer(&name, devices, env.clone())?, &baseline) {
                    (Ok((v, rend)), Ok((_, base))) if rend != *base => {
                        chosen = Ok((path, v, rend, env));
                        break;
                    }
                    (Ok((_, rend)), _) => {
                        chosen = Err(format!("{}: renders on the baseline's {rend}", path.module))
                    }
                    (Err(e), _) => chosen = Err(format!("{}: {e}", path.module)),
                }
            }
            chosen
        };
        match chosen {
            Ok((path, version, renderer, env)) => {
                log(format_args!(
                    "gpu-render: path {} (renderer {renderer}, {version})",
                    path.module
                ));
                app_devices.extend(path.nodes.iter().map(PathBuf::from));
                app_env = env;
            }
            Err(why) => log(format_args!("gpu-render: path baseline ({why})")),
        }
    }
    app_env.extend(a.app_env.iter().cloned());
    log(format_args!("apps' devices {app_devices:?}"));
    let host = Arc::new(Host {
        app_env,
        app_devices,
        gpu,
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
        desktop_input: compositor_registry::DesktopInput {
            mouse_touch: a.mouse_touch,
            tilt: a.tilt_keys.then(|| {
                let d = compositor_registry::TiltConfig::default();
                compositor_registry::TiltConfig {
                    max_deg: a.tilt_max_deg.unwrap_or(d.max_deg),
                    ramp_ms: a.tilt_ramp_ms.unwrap_or(d.ramp_ms),
                    period_ms: a.tilt_period_ms.unwrap_or(d.period_ms),
                    probe_unfocused: a.tilt_probe_unfocused,
                }
            }),
        },
        no_audio_service: a.no_audio_service,
        break_cblk: a.break_cblk,
        no_media_player_service: a.no_media_player_service,
        break_mix: a.break_mix,
        controlled: Mutex::new(BTreeSet::new()),
        done: done_tx,
        am: a.am.clone(),
        screencap: a.screencap.clone(),
        shared_ro: shared_ro.clone(),
        desktop_ro: if a.ui.is_some() {
            desktop_mounts()?
        } else {
            vec![]
        },
        pipewire: atl_platform_host::app_access::pipewire_socket(),
        installd: Installd {
            sessions,
            installer: a.installer.clone(),
            vm: {
                let boot = a
                    .boot_jars
                    .iter()
                    .map(|j| j.display().to_string())
                    .collect::<Vec<_>>()
                    .join(":");
                [
                    format!("-Xbootclasspath:{boot}"),
                    format!("-Xbootclasspath-locations:{boot}"),
                    "-Xnoimage-dex2oat".to_owned(),
                    format!("-Djava.class.path={}", a.installer_class_path.display()),
                ]
                .into_iter()
                .map(OsString::from)
                .collect()
            },
            // ART's environment with the property area at bionic's default path, where the profile mounts it (as the app profile does)
            env: art_env(&a.art_root, &lib, &a.log_tags)
                .into_iter()
                .chain([(PROPERTY_AREA_ENV.into(), PROPERTY_AREA.into())])
                .collect(),
            mounts: {
                let mut m = tree_mounts(&a.art_root)?;
                m.extend(
                    a.boot_jars
                        .iter()
                        .chain([&a.installer_class_path])
                        .map(|p| ro_at_own_path(p)),
                );
                m.push(Mount {
                    host: a.framework_res.clone(),
                    at: Path::new(SYSTEM_FRAMEWORK).join(FRAMEWORK_RES),
                    writable: false,
                });
                m
            },
            staged: Mutex::new(BTreeMap::new()),
            committed: Mutex::new(BTreeMap::new()),
        },
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
    // the services install the sessions at their start (atl_services::install): the files through the control socket, the rest by argument
    let mut services_args: Vec<OsString> = session_args;
    services_args.extend(["--first-uid".into(), a.first_uid.to_string().into()]);
    for abi in &a.abis {
        services_args.extend(["--abi".into(), abi.into()]);
    }
    if let Some(p) = &a.platform_permissions {
        services_args.extend(["--platform-permissions".into(), p.into()]);
    }
    if a.no_gl_selection {
        services_args.push("--no-gl-selection".into());
    }
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
    if a.no_back_finish {
        services_args.push("--no-back-finish".into());
    }
    services_args.push("--root-back".into());
    services_args.push(a.root_back.clone().into());
    if a.no_sensor_service {
        services_args.push("--no-sensor-service".into());
    }
    for s in &a.omit_service {
        services_args.extend(["--omit-service".into(), s.into()]);
    }
    if a.tilt_keys {
        services_args.extend([
            "--sensor-injection".into(),
            atl_services_wire::sensor::TILT_INJECTOR_PACKAGE.into(),
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
    // what the install reads by host path: the sessions' manifests and the installer's frames, the platform's permission definitions
    services_spawn.mounts.push(ro_at_own_path(&install_dir));
    if let Some(p) = &a.platform_permissions {
        services_spawn.mounts.push(ro_at_own_path(p));
    }
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
        READY + INSTALL_READY * u32::try_from(a.packages.len())?,
    )?;
    log(format_args!("services ready"));

    if let (Some(class), Some(uid), Some(name)) = (&a.run_class, a.run_uid, &a.run_name) {
        let boot: Vec<String> = host.boot.iter().cloned().chain(run_boot).collect();
        let spawn = host.app_spawn(uid, name, PLATFORM_SDK, None, &boot, class, &a.run_args)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh dir holding `files` (each content = its name) and a `lib/` dir, under the system temp dir.
    fn staged(test: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atld-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(LIB_DIR_NAME)).expect("dir");
        for f in files {
            std::fs::write(dir.join(f), f).expect("file");
        }
        dir
    }

    fn name(staged: &str, name: &str) -> StagedName {
        StagedName {
            staged: staged.into(),
            name: name.into(),
        }
    }

    /// A `.so` under `lib/<isa>/` that is not ELF (Unity's symbol data `libil2cpp.usym.so`, Angry Birds 2) is data, not a library: the ES1 rule skips it instead of failing the commit.
    #[test]
    fn es1_rule_skips_non_elf_so_files() {
        let dir = staged("es1", &[]);
        let isa = dir.join(LIB_DIR_NAME).join("arm64");
        std::fs::create_dir_all(&isa).expect("isa dir");
        std::fs::write(isa.join("libil2cpp.usym.so"), b"USYM symbol data").expect("usym");
        assert!(!needs_es1(&dir.join(LIB_DIR_NAME)).expect("no error"));
        // a truncated ELF is still read as one, and its error still fails the commit
        // the ELF magic (ELF spec e_ident[EI_MAG0..3]) and nothing after it
        std::fs::write(isa.join("libbroken.so"), b"\x7fELF").expect("broken");
        assert!(needs_es1(&dir.join(LIB_DIR_NAME)).is_err());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// Files staged under source names, one of which already carries another file's target name: each ends under its parsed name with its own bytes.
    #[test]
    fn staged_files_take_their_parsed_names() {
        let dir = staged("rename", &["a.apk", "split_x.apk"]);
        let names = [
            name("a.apk", "split_x.apk"),
            name("split_x.apk", "base.apk"),
        ];
        rename_staged(&dir, &names).expect("renamed");
        assert_eq!(
            std::fs::read_to_string(dir.join("split_x.apk")).expect("split"),
            "a.apk"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("base.apk")).expect("base"),
            "split_x.apk"
        );
        assert!(!dir.join("a.apk").exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// Break: a staged APK the names leave out, or a target outside AOSP's two forms, is refused before any rename.
    #[test]
    fn incomplete_or_foreign_names_rename_nothing() {
        let dir = staged("refuse", &["a.apk", "b.apk"]);
        assert!(rename_staged(&dir, &[name("a.apk", "base.apk")]).is_err());
        assert!(
            rename_staged(
                &dir,
                &[name("a.apk", "base.apk"), name("b.apk", "other.apk")]
            )
            .is_err()
        );
        assert!(
            rename_staged(
                &dir,
                &[name("a.apk", "base.apk"), name("b.apk", "split_.apk")]
            )
            .is_err()
        );
        assert!(dir.join("a.apk").exists() && dir.join("b.apk").exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn gralloc_node_is_the_ready_lines_parenthesis() {
        // the allocator's stderr: its counters line, then the ready line (atl-gralloc-allocator main)
        let log = "gralloc: counters allocate=0\ngralloc: ready (/dev/dri/renderD128)\n";
        assert_eq!(gralloc_node(log).as_deref(), Some("/dev/dri/renderD128"));
        assert_eq!(gralloc_node("gralloc: ready\n"), None);
    }
}
