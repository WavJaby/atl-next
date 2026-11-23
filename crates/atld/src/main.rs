// SPDX-License-Identifier: MIT
//! atld, M2 wiring: starts the system the Android way, each process through the broker, then serves the services' process control until the run ends (step 7).
//! 1. the runtime root `--root` (made fresh): `properties` (the default property area, atl-property-service; `properties.d/` one area per app process: the defaults plus the properties only that process has, [`Host::process_properties`]), `system/` (build.prop, `framework/` = the boot jars and shared libraries),
//!    `install/` (the installer's frames of each session, read by the services), `log/<process>.log` (each process's stdout + stderr). The data root (`--data-root`, default the user's `$XDG_DATA_HOME/atl`, kept across runs): `apps/<package>/{apk,data,data_de,external}` and `device/data/{system,dalvik-cache}` ([`host_layout`]).
//! 2. the install sessions: each `--package` (its APK files) is a session the services install at their start (maintainer option I, 2026-11-13): they own the package list; atld does installd's
//!    file work on their request (control `Install`: stage under session names (hard links where possible) + run atl-installer in the broker's installer profile; `Commit`: rename each file by its
//!    parsed split (`base.apk`, `split_<name>.apk`), then the dir to `/data/app/<package>`, create the data dirs).
//! 3. atl-binderd, its admin connection on stdin; the broker holds the other end.
//! 4. AOSP servicemanager as uid 1000 (AID_SYSTEM), the one process that may become context manager. With `--health`: AOSP's health HAL (AID_SYSTEM) on the host's power-supply sysfs. With `--keystore2`: AOSP's KeyMint HAL (uid AID_NOBODY), then keystore2 (AID_KEYSTORE) with its database dir.
//! 5. atl-system-services as uid 1000 with the packages and, as its stdin, the process-control connection (atl_broker::control): it starts app processes (ActivityManager's ProcessList) and kills them, here, through
//!    the broker's sandbox. atld waits for its `services: ready`.
//! 6. `--run-class`: a command as uid `--run-uid` (the shell), `am instrument` for example; its exit ends the run.
//! 7. the control socket `<root>/control.sock` (serve_run_control): `stop` ends the run, `start <package>/<class>` is `am start`. An app process ending does not end the run: the services see its death.
//!    Except with `--exit-with-last-app` (default with `--ui`): the services' `LastAppEnded` after the last app's window closed or it finished / exited 0 ends it ([`Host::last_app_ended`]).
//!
//! 8. the end (`stop`, the last app's end, the command's exit, SIGINT/SIGTERM): app processes SIGKILLed; the services, gralloc, servicemanager and step 4's daemons SIGTERMed (then SIGKILLed after `--stop-grace-ms`); binderd by its admin
//!    connection; then the ranked summary of every process's `diag:` lines on stderr (`atld: summary ...`, print_summary).
//!
//! Exit status: the command's (bwrap passes it on); 0 after `stop` and after the last app's end; 128 + n after signal n; failure when the services exited. Plan: binderd, servicemanager and the services move into this process later; here each is a process, as `xtask test binder` runs them.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode, ExitStatus};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_android_paths::valid_package_name;
use atl_android_paths::{
    DATA, DATA_APP, DATA_USER, DATA_USER_DE, KEYSTORE_DB, PROC_CPUINFO, PRODUCT, PROPERTY_AREA,
    PROPERTY_AREA_ENV, SDCARD, SHARED_STORAGE, SYSTEM, SYSTEM_EXT, SYSTEM_FONTS, SYSTEM_FRAMEWORK,
    VENDOR,
};
use atl_broker::control::{self, Request, Response, StagedName, StartRequest};
use atl_broker::{
    BinderPids, Broker, INSTALLER_UID, Link, Mount, Process, Profile, Right, Spawn, gpu,
    kill_sandbox, seccomp_death, to_log, tree_mounts,
};
use atl_gralloc::node::{self, AllocatorTier, NodeSource, UDMABUF_DEVICE};
use atl_gralloc::vintf;
use atl_platform_host::pipewire_context;
use atl_platform_host::wayland_context::{self, AppContext};

/// A process's desktop contexts ([`Host::app_spawn`]), held until it is reaped ([`Host::start`]): the compositor and PipeWire serve its sockets exactly while it lives.
type Held = (Option<AppContext>, Option<pipewire_context::AppContext>);

/// PipeWire contexts made this run (the `contexts=<n>` counter of `pipewire: security context applied`).
static PIPEWIRE_CONTEXTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use atl_platform_host::{
    DesktopNotification, Notifications, PlatformNotifications, Posted, Urgency,
};
use atl_property_service::{LinearTier, Property};
use atl_services_types::compositor_registry;
use atl_services_types::display_model::DisplayModel;
use atl_services_types::native_libs::LIB_DIR_NAME;
use atl_services_types::packages::Library;
use atl_services_types::permission::SYSTEM_PROCESS;
use atl_services_types::system_packages;
use clap::Parser;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

mod applog;
mod cacerts;
mod host_layout;
mod host_props;
mod installed_code;
use serde::Deserialize;

#[derive(Parser)]
#[command(about, long_about = None)]
struct Args {
    /// Runtime root (removed and made again). Holds atld's private copies of the sandbox helpers (`helpers/`: stub, join, shim), which run from there: its filesystem must allow exec.
    #[arg(long)]
    root: PathBuf,
    /// The persistent data root, kept across runs: `apps/<package>/{apk,data,data_de,external}` (each package's code, data, device-protected data, Android/data + obb) and `device/data/` (system state, dalvik-cache).
    /// Default: the user's in the desktop's data dir (Linux: `$XDG_DATA_HOME/atl`, else `~/.local/share/atl`), as a native program keeps its data. A fresh device is a new or empty DIR. Must lie outside `--root`, which is removed at start.
    /// At start, package dirs of the old single tree (`device/data/{app,user/0,user_de/0}/<package>`, `device/external/<package>`) are renamed into `apps/<package>/`; a package present in both is not started.
    #[arg(long, value_name = "DIR")]
    data_root: Option<PathBuf>,
    /// atl-binderd executable.
    #[arg(long)]
    binderd: PathBuf,
    /// atl-sandbox-stub executable: the in-child stage that applies Landlock before binderd runs.
    #[arg(long)]
    sandbox_stub: PathBuf,
    /// atl-sandbox-join executable: starts a web process an app asks for inside the app's pid namespace (atl_broker::webspawn). Given with `--flatpak-spawn`; both omitted: apps get no
    /// web-process channel, and WebKit's own sandbox then cannot start inside theirs (it aborts at the first page load).
    #[arg(long, requires = "flatpak_spawn")]
    sandbox_join: Option<PathBuf>,
    /// The `flatpak-spawn` shim (crate atl-flatpak-spawn) every app sandbox shows on its PATH: WebKit's route to that channel.
    #[arg(long, requires = "sandbox_join")]
    flatpak_spawn: Option<PathBuf>,
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
    /// AOSP's default health HAL executable (`android.hardware.health-service.example`, hardware/interfaces/health/aidl/default): `IHealth/default` over BatteryMonitor, run as AID_SYSTEM (its `.rc`) after
    /// servicemanager with the host's power-supply sysfs read-only (atl_broker::power_supply_sysfs); its VINTF fragment comes with `--vintf-fragment`. The battery services' source.
    /// Omitted: no battery services (BatteryManager is null in apps).
    #[arg(long)]
    health: Option<PathBuf>,
    /// AOSP keystore2 executable (system/security): `IKeystoreService/default` and its companion services, run as AID_KEYSTORE with its database dir (atl_android_paths::keystore_db_host) at /data/misc/keystore, after the KeyMint HAL (`--keymint`).
    /// Omitted: no keystore (apps' AndroidKeyStore finds no service).
    #[arg(long, requires = "keymint")]
    keystore2: Option<PathBuf>,
    /// The KeyMint HAL executable keystore2 uses, AOSP's `android.hardware.security.keymint-service.nonsecure`, run as AID_NOBODY as its `.rc` says.
    #[arg(long, requires = "keystore2")]
    keymint: Option<PathBuf>,
    /// A VINTF manifest fragment (repeat): `<partition>=<file>`, partition `system` or `vendor`; placed in `/<partition>/etc/vintf/manifest/`, where libvintf reads a partition's fragments, as Soong installs `vintf_fragment_modules`.
    #[arg(long = "vintf-fragment", value_parser = vintf_fragment)]
    vintf_fragments: Vec<(String, PathBuf)>,
    /// atl-egl-probe executable: with `--gralloc` and `--ui`, run once (`--display-node`) to ask the desktop's Wayland EGL display which GPU it renders on; gralloc and every app then use that node (atl_gralloc::node).
    /// Headless runs, and displays naming no node, take the node by gralloc's own trial, run in the probe (`--trial-node`) before gralloc starts. Then per render path (`--renderer`), which also reports whether the apps' GPUs import a gralloc buffer: gralloc's allocator tier (node::choose_allocator_tier).
    /// Needed whenever `--gralloc` is given.
    #[arg(long)]
    gpu_probe: Option<PathBuf>,
    /// atl-engine-cpuinfo executable: for each `--abi` ISA whose engine `.so` a `--file` places (atl-android-paths `engine_library`), run once in the sandbox; its stdout, the guest's /proc/cpuinfo as the engine formats it, is cached as `<root>/cpuinfo.<isa>`
    /// and bound read-only over /proc/cpuinfo of every app process of that ISA, so Java and the runtime read what guest code reads (a device binds the bridge's `cpuinfo.<isa>.txt` the same way). Omitted: no bind; host readers see the host's file.
    #[arg(long)]
    cpuinfo_probe: Option<PathBuf>,
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
    /// A file of /system, /vendor, /product or /system_ext besides the framework's (repeat): `<host path>=<absolute Android path>`, linked there (the GL driver: /vendor/lib64/egl/<name>; a
    /// preinstalled package: /product/app/<Name>/<Name>.apk, scanned at start).
    #[arg(long = "file", value_parser = partition_file)]
    files: Vec<(PathBuf, PathBuf)>,
    /// A shared library packages may `<uses-library>` (repeat): `<name>=<host jar>[,<dependency name>...]`, linked as /system/framework/<file name> and registered with the package manager
    /// (SystemConfig's `<library name file dependency>`).
    #[arg(long = "shared-library", value_parser = Library::parse_arg)]
    shared_libraries: Vec<(String, Library)>,
    /// A package to install (repeat; session i, in uid order, after the preinstalled packages): a JSON file `{"files": [{"split": null|<name>, "path": <host path>}]}`, base first. None: only the
    /// preinstalled packages.
    #[arg(long = "package")]
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
    /// framework-res's manifest as a JSON atl_services_types::packages::PlatformManifest (its `<permission>` and `<protected-broadcast>` elements), forwarded to the services.
    /// Omitted: only the packages' own definitions count, so no platform permission is granted and only system packages' broadcasts are protected.
    #[arg(long)]
    platform_manifest: Option<PathBuf>,
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
    /// Test only: passed to the services (`--no-bind-service`), the deliberate break of the app gate's bound-service and jobscheduler lines: every bindService answers 0.
    #[arg(long)]
    no_bind_service: bool,
    /// Test only: passed to the services (`--no-job-runner`), the deliberate break of the app gate's jobscheduler line: jobs are stored, never run.
    #[arg(long)]
    no_job_runner: bool,
    /// What an unhandled Back on an app's root activity does, passed to the services: `minimize` (the window is minimized, the app keeps running; Android 12+) or `close` (the app's window closes).
    #[arg(long, value_parser = ["minimize", "close"], default_value = "minimize")]
    root_back: String,
    /// Which windows an activity's fixed-orientation request reshapes, passed to the services: `every` (a portrait request turns a landscape window portrait, resizeable apps included) or
    /// `unresizeable` (AOSP desktop windowing: only non-resizeable activities).
    #[arg(long, value_parser = ["every", "unresizeable"], default_value = "every")]
    orientation_reshape: String,
    /// Test only: passed to the services (`--no-sensor-service`), the deliberate break of the sensors gate's empty stage.
    #[arg(long)]
    no_sensor_service: bool,
    /// Test only: passed to the services (`--omit-service <name>`, repeat), the deliberate break of the app gate's line for that service.
    #[arg(long)]
    omit_service: Vec<String>,
    /// Desktop tilt for accelerometer games: while an app window has focus, held Left / Right (or A / D) tilt the device, injected as accelerometer data (sensorservice HAL bypass injection); the keys still reach the app. Off: no emulation, a real accelerometer is delivered as AOSP. On a machine with a real accelerometer this wins: every app of this atld session sees only the injected one.
    #[arg(long)]
    tilt_keys: bool,
    /// Desktop adaptation: the left mouse button is one finger, its press / drag / release reach the app as touchscreen events (hover and the other buttons stay the mouse's). AOSP has no such conversion and touch-only games ignore the mouse.
    /// On by default, so touch-only apps are usable with a mouse; `--mouse-touch false` is exact AOSP input.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    mouse_touch: bool,
    /// Desktop adaptation: an Esc the app leaves unhandled falls back to Back (the keyboard's key map gains ESCAPE `base: fallback BACK`; KeyCharacterMap.getFallbackAction), as a desktop keyboard has no Back key. Pinned AOSP maps plain Esc to nothing.
    /// On by default; `--esc-back false` is exact AOSP's map. Passed to the services, which serve the map.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    esc_back: bool,
    /// Desktop adaptation: the run ends (status 0) once the last app process ends after its window was closed or it finished its last activity, or exited 0 by itself (the services'
    /// control `LastAppEnded`), so nothing stays running after the window is gone. AOSP's system_server outlives its apps. Default: on with `--ui`, off headless; `false` = the run ends only
    /// on `stop`, the `--run-class` command's exit or the services' exit.
    #[arg(long, action = clap::ArgAction::Set)]
    exit_with_last_app: Option<bool>,
    /// Desktop adaptation (the services' headless end rule, plan "Linux conventions"): a process nothing holds (no window, service, receiver, provider) ends this many ms after its last
    /// holding change, once its uid is idle. Default 60000 = AOSP's BACKGROUND_SETTLE_TIME (ActivityManagerConstants.java:253), so a cold start lives one settle period. Passed to the services.
    #[arg(long, default_value_t = 60_000)]
    empty_process_grace_ms: u64,
    /// Desktop adaptation (M29): a window's close ends its processes' services with no restart. `false` = AOSP: the services of a killed process restart when STICKY (killServicesLocked allowRestart).
    /// Passed to the services.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    window_close_stops_services: bool,
    /// Decision D2 (per-app "start at login", default off): a package named here gets LOCKED_BOOT_COMPLETED and BOOT_COMPLETED when the services start (repeat). A package not named is
    /// opted out for this run. Passed to the services.
    #[arg(long = "autostart", value_name = "PACKAGE")]
    autostart: Vec<String>,
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
    /// How long, ms, each system process (the services, gralloc, servicemanager, binderd) gets at the run's end between its stop request (SIGTERM; binderd: its admin connection closed) and
    /// SIGKILL: what it takes to write its exit totals (`diag: total`) and end. App processes are SIGKILLed, as AOSP kills them. The C4 config's CLI layer: no TOML layer exists yet.
    #[arg(long, default_value_t = STOP_GRACE_MS)]
    stop_grace_ms: u64,
    /// C4 `prop_diag.max_keys`: distinct `prop:` keys each process counts before further unknown names count `prop:overflow keys` (atl_diag::count_capped); every process reads it as
    /// `ro.atl.diag.prop-max-keys` (atl_diag::PROP_MAX_KEYS_PROPERTY).
    #[arg(long, default_value_t = atl_diag::PROP_MAX_KEYS_DEFAULT)]
    prop_diag_max_keys: usize,
    /// C4 `prop_diag.summary_rows`: rows of the `atld: prop-summary` block (the full list stays in each process's log).
    #[arg(long, default_value_t = PROP_SUMMARY_ROWS)]
    prop_diag_summary_rows: usize,
    /// App log capture (atld applog, journald's `RateLimitIntervalSec=`; also each liblog daemon's logdw log: keystore2, the KeyMint HAL): the window, ms, of `--log-rate-burst`.
    #[arg(long, default_value_t = applog::RATE_INTERVAL_MS_DEFAULT)]
    log_rate_interval_ms: u64,
    /// App log capture (journald's `RateLimitBurst=`): lines an app process may log per window; the rest is dropped and counted. 0: no rate limit.
    #[arg(long, default_value_t = applog::RATE_BURST_DEFAULT)]
    log_rate_burst: u64,
    /// App log capture (journald's `SystemMaxUse=`): bytes an app process's log may hold: 1/4 head (`<name>.log`), the rest a tail ring (`<name>.log.1` newest .. `.4`) whose oldest segment is
    /// retired and counted when a new one starts. 0: no bound.
    #[arg(long, default_value_t = applog::MAX_BYTES_DEFAULT)]
    log_max_bytes: u64,
}

/// `prop_diag.summary_rows` default [est]: one terminal screen beside the API block; the device diff (plan S18) reads the logs, not this block.
const PROP_SUMMARY_ROWS: usize = 20;

/// Android uids (system/core/libcutils/include/private/android_filesystem_config.h): AID_SYSTEM, AID_KEYSTORE (keystore2.rc `user keystore`), AID_SHELL, AID_NOBODY (the KeyMint HAL's .rc `user nobody`).
const AID_SYSTEM: u32 = 1000;
const AID_KEYSTORE: u32 = 1017;
const AID_SHELL: u32 = 2000;
const AID_NOBODY: u32 = 9999;
/// The installer's context: AOSP parses packages in system_server (PackageManagerService); the installer holds no binder, so only its own view of itself could read it.
const INSTALLER_CONTEXT: &str = atl_sepolicy::domains::SYSTEM_SERVER;

/// The image's plat_seapp_contexts (a `--file` at atl_android_paths::SEAPP_CONTEXTS, from the converter's sepolicy goal): every app process's context comes from it, so a run without it does not start.
/// `files`: `--file` rows as [`partition_file`] keeps them, the Android path relative to `/`.
fn load_seapp(files: &[(PathBuf, PathBuf)]) -> Result<atl_sepolicy::Seapp> {
    let at = Path::new(atl_android_paths::SEAPP_CONTEXTS);
    let (host, _) = files
        .iter()
        .find(|(_, a)| Path::new("/").join(a) == at)
        .with_context(|| format!("no --file places {}", at.display()))?;
    let text =
        std::fs::read_to_string(host).with_context(|| format!("reading {}", host.display()))?;
    Ok(atl_sepolicy::Seapp::parse(&text)?)
}

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
/// [`Args::stop_grace_ms`]'s default. A stop writes one line per counter key (tens) and exits: milliseconds; 2 s is a loaded machine's margin. The system processes stop in parallel, then binderd,
/// so a stop takes at most 2 graces: 4 s, inside `xtask remote run-apk`'s KILL_GRACE (10 s after the limit's TERM) and the harness's STOP_WAIT (10 s after `stop`).
const STOP_GRACE_MS: u64 = 2_000;
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

/// The host's default network, read here (outside every sandbox, as a native program reads it: GIO reaches NetworkManager) for the system services' `Request::Network`: at their start and on each change they see.
/// Unreadable = none, said so; the services log the network they got (`connectivity:`).
fn host_network() -> Option<atl_platform_host::DefaultNetwork> {
    use atl_platform_host::NetworkSource;
    atl_platform_host::PlatformNetwork
        .default_network()
        .unwrap_or_else(|e| {
            log(format_args!("network: {e}; no default network"));
            None
        })
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

/// The kernel log of a daemon whose libbase logs there (servicemanager: main.cpp `InitLogging(argv, KernelLogger)`, so every ALOG and LOG of it goes to the kernel log, among them the lookup
/// of a name nobody serves, ServiceManager.cpp `tryStartService`, which atl-diag counts as `missing-service`). init's `file /dev/kmsg w` hands the daemon an fd in
/// `ANDROID_FILE__dev_kmsg` (libbase logging.cpp `OpenKmsg`, any O_WRONLY fd); here that fd is the process's stderr, a pipe atld reads into `log` as logcat lines ([`kernel_line`]), stamped when
/// read. Environment only, as init's is on Android: no servicemanager change.
fn kernel_log_to_log(spawn: &mut Spawn, log: &Path) -> Result<()> {
    let (reader, writer) = std::io::pipe().context("kernel log pipe")?;
    let out = std::fs::OpenOptions::new().append(true).open(log)?;
    spawn.stdout = writer.try_clone()?.into();
    spawn.stderr = writer.into();
    spawn
        .env
        .push(("ANDROID_FILE__dev_kmsg".into(), STDERR_FD.into()));
    std::thread::Builder::new()
        .name("kmsg".into())
        .spawn(move || {
            let mut out = out;
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                let _ = out.write_all(kernel_line(&line).as_bytes());
            }
        })
        .context("kernel log thread")?;
    Ok(())
}

/// The logd write socket of a daemon that logs through liblog (keystore2, the KeyMint HAL: android_logger): on Android logd listens at /dev/socket/logdw; here atld binds one per daemon at `sock`
/// (atl-services-types `serve_logdw`, the app launcher's receiver), bound at that path in the daemon's sandbox, and appends each record to `log` as a logcat line. Environment only, no daemon change.
/// Written through the app log's bound ([`bounded_copier`]: rate window, byte budget, drops counted), as an app can make these daemons log at will (keystore2 logs every error it answers but
/// KEY_NOT_FOUND, error.rs `into_logged_binder`). The copier runs as long as atld: the socket never closes.
fn logdw_to_log(
    mounts: &mut Vec<Mount>,
    sock: &Path,
    name: &str,
    log: &Path,
    limits: applog::Limits,
) -> Result<()> {
    let (out, _copier) = bounded_copier(name, log, limits)?;
    atl_services_types::logcat::serve_logdw(sock, move |line| {
        let _ = (&out).write_all(line.as_bytes());
    })?;
    mounts.push(Mount {
        host: sock.to_owned(),
        at: atl_services_types::logcat::LOGDW.into(),
        // connect(2) on a socket file needs write access to it
        writable: true,
    });
    Ok(())
}

/// A pipe whose read end a thread copies into `log` + its tail ring (appended) under `limits` ([`applog`]); a drop also gives one atld line, `log <name>: suppressed ...`, when the pipe ends.
fn bounded_copier(
    name: &str,
    path: &Path,
    limits: applog::Limits,
) -> Result<(std::io::PipeWriter, std::thread::JoinHandle<()>)> {
    let (reader, writer) = std::io::pipe().context("log pipe")?;
    let mut out = applog::Files::open(path)?;
    let name = name.to_owned();
    let copier = std::thread::Builder::new()
        .name("app-log".into())
        .spawn(move || {
            match applog::copy_bounded(BufReader::new(reader), &mut out, limits, Instant::now) {
                Ok(d) if d.lines > 0 => log(format_args!(
                    "log {name}: suppressed {} lines ({} bytes)",
                    d.lines, d.bytes
                )),
                Ok(_) => {}
                Err(e) => log(format_args!("log {name}: {e}")),
            }
        })
        .context("log copier thread")?;
    Ok((writer, copier))
}

/// The log bounds of `--log-rate-*` / `--log-max-bytes` ([`applog::Limits`]).
fn log_limits(a: &Args) -> applog::Limits {
    applog::Limits {
        rate: (a.log_rate_burst > 0).then(|| {
            (
                Duration::from_millis(a.log_rate_interval_ms),
                a.log_rate_burst,
            )
        }),
        size: (a.log_max_bytes > 0).then(|| applog::Size::of_budget(a.log_max_bytes)),
    }
}

/// stderr's fd number, the `ANDROID_FILE__dev_kmsg` value of [`kernel_log_to_log`].
const STDERR_FD: &str = "2";

/// One line of a [`kernel_log_to_log`] pipe: KernelLogLine's `<level>tag: message` (libbase logging.cpp) as a logcat line (atl-services-types `logcat::format`, source `kmsg`, pid 0: the pipe
/// carries no sender), stamped now; any other line (the sandbox stub's, a crash's) kept as written.
fn kernel_line(line: &str) -> String {
    use atl_services_types::logcat::{self, Level, Record};
    let parsed = line.strip_prefix('<').and_then(|rest| {
        let (level, rest) = rest.split_once('>')?;
        let (tag, message) = rest.split_once(": ")?;
        Some((level.parse::<u8>().ok()?, tag, message))
    });
    let Some((level, tag, message)) = parsed else {
        return format!("{line}\n");
    };
    // the inverse of KernelLogLine's kLogSeverityToKernelLogLevel: VERBOSE and DEBUG -> 7, INFO 6, WARNING 4, ERROR 3, FATAL 2
    let level = match level {
        7 => Level::Debug,
        6 | 5 => Level::Info,
        4 => Level::Warn,
        3 => Level::Error,
        _ => Level::Fatal,
    };
    logcat::format(&Record {
        monotonic: logcat::monotonic_now(),
        realtime: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default(),
        pid: 0,
        tid: 0,
        source: "kmsg",
        tag,
        level,
        location: None,
        message,
    })
}

/// The runtime root's layout: `root` is removed and made again at start; `data_root` (`--data-root`, [`host_layout`]) is kept across runs.
struct Root {
    root: PathBuf,
    data_root: PathBuf,
}

impl Root {
    fn system(&self) -> PathBuf {
        self.root.join("system")
    }
    fn vendor(&self) -> PathBuf {
        self.root.join("vendor")
    }
    /// The partitions holding preinstalled packages beside /system (atl-services-types `system_packages`), each at `/<name>`: host dir and Android path.
    fn package_partitions(&self) -> [(PathBuf, &'static str); 2] {
        [
            (self.root.join("product"), PRODUCT),
            (self.root.join("system_ext"), SYSTEM_EXT),
        ]
    }
    /// /data's state not kept per package: `system/`, `dalvik-cache/`, install staging.
    fn data(&self) -> PathBuf {
        host_layout::device_data(&self.data_root)
    }
    /// `package`'s dir `item` (`apps/<package>/<item>`).
    fn package(&self, package: &str, item: host_layout::Item) -> PathBuf {
        host_layout::package_dir(&self.data_root, package, item)
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
    /// atld's private copies of the helper executables the broker binds or runs ([`pin_helper`]).
    fn helpers(&self) -> PathBuf {
        self.root.join("helpers")
    }
}

/// A private copy of helper executable `exe` in `dir` (under the runtime root, which only this atld uses), for the broker to bind and run for atld's whole life.
/// Why: a bind mount pins the dentry it was made from, and a web process's sandbox binds the stub again from its app's view (atl_broker WebSpawner::start); once the host file is replaced
/// (a rebuild re-linking it, a package upgrade) that dentry is unlinked and the kernel refuses the bind (ENOENT), so the web process never starts. A file nobody else writes stays linked.
fn pin_helper(dir: &Path, exe: &Path) -> Result<PathBuf> {
    let name = exe
        .file_name()
        .with_context(|| format!("{}: no file name", exe.display()))?;
    std::fs::create_dir_all(dir)?;
    let at = dir.join(name);
    // fs::copy keeps the mode bits (executable)
    std::fs::copy(exe, &at)
        .with_context(|| format!("pinning {} at {}", exe.display(), at.display()))?;
    Ok(at)
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
/// Logs what the desktop lacks: one line per fallback group with a missing face (install hint), then the counted `diag` line (atl-fonts report).
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
    for line in atl_fonts::report(
        &model,
        &atl_fonts_table::groups(atl_fonts_table::DEPS_TOML)?,
    ) {
        log(format_args!("fonts: {line}"));
    }
    let etc = r.system().join("etc");
    std::fs::create_dir_all(&etc)?;
    std::fs::write(etc.join("font_fallback.xml"), model.font_fallback_xml())?;
    std::fs::write(etc.join("fonts.xml"), model.fonts_xml())?;
    for k in 0..model.dirs.len() {
        std::fs::create_dir_all(r.system().join("fonts").join(k.to_string()))?;
    }
    Ok(model.dirs)
}

/// fontconfig's paths a UI app reads (atl_platform_host::app_access), read-only at their own paths.
fn font_mounts() -> Result<Vec<Mount>> {
    Ok(atl_platform_host::app_access::fontconfig_paths()?
        .iter()
        .map(|p| ro_at_own_path(p))
        .collect())
}

/// What atld's own display probes (atl-egl-probe, before any app) read from the desktop: the compositor socket as-is ([`probe_socket`]) and [`font_mounts`]; no app code runs in them, so no security context.
fn desktop_mounts(ui: bool) -> Result<Vec<Mount>> {
    let socket = probe_socket(atl_platform_host::app_access::wayland_socket(), ui)?;
    Ok(socket.into_iter().chain(font_mounts()?).collect())
}

/// The display probes' compositor socket. A UI run without one is an error: it cannot show a window. A headless run's renderer probe asks the display too, and `no Wayland display` is one of its answers (node::choose_allocator_tier).
fn probe_socket(socket: Option<PathBuf>, ui: bool) -> Result<Option<Mount>> {
    if ui && socket.is_none() {
        bail!("--ui: no compositor socket (WAYLAND_DISPLAY / XDG_RUNTIME_DIR)");
    }
    Ok(socket.as_deref().map(ro_at_own_path))
}

/// The compositor a UI run's apps connect to: its socket (atl_platform_host::app_access::wayland_socket) and, where it offers `wp_security_context_manager_v1`, the per-process contexts an app connects through instead
/// (its sandbox shows its context socket at the compositor socket's path; the compositor withholds its privileged globals from it). Without the protocol the app sees the socket as-is.
struct Wayland {
    socket: PathBuf,
    contexts: Option<wayland_context::Contexts>,
}

impl Wayland {
    /// Probes the compositor once; logs `wayland: security context available|unavailable (<reason>)`. No compositor socket is an error: a UI run without one cannot show a window.
    fn probe() -> Result<Self> {
        let socket = atl_platform_host::app_access::wayland_socket()
            .context("--ui: no compositor socket (WAYLAND_DISPLAY / XDG_RUNTIME_DIR)")?;
        let contexts = match wayland_context::Contexts::probe(&socket) {
            wayland_context::Probe::Available(c) => {
                log(format_args!(
                    "wayland: security context available (engine {})",
                    wayland_context::SANDBOX_ENGINE
                ));
                Some(c)
            }
            wayland_context::Probe::Unavailable(why) => {
                log(format_args!(
                    "wayland: security context unavailable ({why}): apps get the compositor socket as-is"
                ));
                None
            }
        };
        Ok(Self { socket, contexts })
    }
}

/// The desktop's PipeWire as apps reach it: its socket (atl_platform_host::app_access::pipewire_socket) and, where PipeWire offers SecurityContext, atld's permission manager: each app process
/// gets its own context socket at the PipeWire socket's path, never the socket itself (pipewire_context module doc: tiers enforced / write-restricted).
struct PipeWire {
    socket: PathBuf,
    manager: Option<pipewire_context::Manager>,
}

impl PipeWire {
    /// Probes PipeWire once; logs `pipewire: security context available tier=<tier> (...)|unavailable (<reason>)`. None: no PipeWire socket (apps' tracks fail).
    fn probe() -> Option<Self> {
        let socket = atl_platform_host::app_access::pipewire_socket()?;
        let manager = match pipewire_context::Manager::start(&socket) {
            pipewire_context::Probe::Available(m) => {
                let observed = m.probe().observed();
                match m.tier() {
                    pipewire_context::Tier::Enforced => log(format_args!(
                        "pipewire: security context available tier=enforced {observed} (session manager grants context clients nothing; atld grants own objects, sinks, sources by RECORD_AUDIO/CAMERA)"
                    )),
                    pipewire_context::Tier::WriteRestricted => log(format_args!(
                        "pipewire: security context available tier=write-restricted {observed} (session manager grants the probe client's access value something; apps use pipewire.access=restricted, proven read without write/metadata: capture not restricted)"
                    )),
                }
                Some(m)
            }
            pipewire_context::Probe::Unavailable(why) => {
                log(format_args!(
                    "pipewire: security context unavailable ({why}): apps get the PipeWire socket as-is (whole graph, capture, write)"
                ));
                None
            }
        };
        Some(Self { socket, manager })
    }
}

/// /system/framework/<name> of each boot jar, the boot classpath as the sandbox sees it; /system's CA store from the distro bundle.
fn system_layout(a: &Args, r: &Root) -> Result<Vec<String>> {
    let framework = r.system().join("framework");
    std::fs::create_dir_all(&framework)?;
    // every partition exists, empty or not: each is bound into the sandboxes
    for (host, _) in r.package_partitions() {
        std::fs::create_dir_all(host)?;
    }
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
    for (partition, file) in &a.vintf_fragments {
        let dir = r.root.join(partition).join("etc/vintf/manifest");
        std::fs::create_dir_all(&dir)?;
        let name = file
            .file_name()
            .with_context(|| format!("--vintf-fragment {}: no file name", file.display()))?;
        std::fs::copy(file, dir.join(name))
            .with_context(|| format!("--vintf-fragment {}", file.display()))?;
    }
    Ok(())
}

/// `--vintf-fragment`'s `<partition>=<file>`: the partitions whose fragments servicemanager's libvintf reads here (system: framework manifest, vendor: device manifest).
fn vintf_fragment(s: &str) -> Result<(String, PathBuf), String> {
    match s.split_once('=') {
        Some((p @ ("system" | "vendor"), f)) if !f.is_empty() => Ok((p.to_owned(), f.into())),
        _ => Err(format!("{s:?}: want system=<file> or vendor=<file>")),
    }
}

/// The vendor permission file atld writes with the features it derives from this host and the desktop settings (atl_services_types::system_config format); the services read it with the
/// partitions' other permission files (`--partitions`). The vendor partition, as a device's hardware features come from its vendor build.
const HOST_FEATURES_FILE: &str = "etc/permissions/atl_host_hardware.xml";
/// AOSP's frameworks/native/data/etc/android.hardware.touchscreen.xml: a touchscreen, which also offers faketouch.
const FEATURES_TOUCHSCREEN: [(&str, i32); 2] = [
    ("android.hardware.touchscreen", 0),
    ("android.hardware.faketouch", 0),
];
/// PackageManager.FEATURE_OPENGLES_EXTENSION_PACK (frameworks/native/data/etc/android.hardware.opengles.aep.xml).
const FEATURE_OPENGLES_AEP: (&str, i32) = ("android.hardware.opengles.aep", 0);
/// PackageManager.FEATURE_WEBVIEW (frameworks/native/data/etc/android.software.webview.xml): declared when the web engine module loads (the `webview-probe` process, webview plan S6); absent -> apps
/// get AOSP's no-WebView device (getProvider throws UnsupportedOperationException). The provider package ships with every runtime; without the feature AOSP never asks for it.
const FEATURE_WEBVIEW: (&str, i32) = ("android.software.webview", 0);
/// atld's first argument that makes it the web engine probe instead of a run: load the module (atl_platform_ui::probe_web_engine), say why on stderr, exit 0 when it loaded. atld starts it through
/// the broker with the apps' library path, so the module resolves as an app's first WebView call resolves it, and GTK never loads into atld.
const PROBE_WEB_ENGINE: &str = "--probe-web-engine";

/// The [`PROBE_WEB_ENGINE`] process.
fn probe_web_engine_main() -> ExitCode {
    match atl_platform_ui::probe_web_engine() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(format_args!("webview-probe: {e}"));
            ExitCode::FAILURE
        }
    }
}

/// The device's extension pack and GLES version for a probed host GL, both the host's: AOSP keeps one device-wide list and the driver answers per context (gl-honesty M74 D1). The version is unset unless GL_VERSION is GLES's.
fn device_gl(gl_version: &str, aep: bool) -> (bool, Option<Property>) {
    (aep, atl_property_service::gles_version(gl_version))
}

/// Writes [`HOST_FEATURES_FILE`] (also when empty: the file says which run found nothing) and logs `features: host <names>`.
fn host_features_layout(r: &Root, features: &[(&str, i32)]) -> Result<()> {
    let path = r.vendor().join(HOST_FEATURES_FILE);
    ensure!(!path.exists(), "two files at {}", path.display());
    std::fs::create_dir_all(path.parent().context("a file below etc")?)?;
    std::fs::write(
        &path,
        atl_services_types::system_config::permissions_xml(features)?,
    )?;
    let names: Vec<&str> = features.iter().map(|(n, _)| *n).collect();
    log(format_args!("features: host [{}]", names.join(" ")));
    Ok(())
}

/// Partitions `--file` may place files in: the root's `system`, `vendor`, `product`, `system_ext` (Root::system, Root::vendor, Root::package_partitions).
const FILE_PARTITIONS: &[&str] = &["system", "vendor", "product", "system_ext"];

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
        .ok_or_else(|| format!("{s}: not <host path>=<partition path>"))?;
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
        return Err(format!(
            "{at}: not a file path below a partition ({})",
            FILE_PARTITIONS.join(", ")
        ));
    }
    Ok((PathBuf::from(host), rel.to_path_buf()))
}

/// installd's file work for the services' install (maintainer option I): the sessions' files, the installer's view, and what was staged and committed.
struct Installd {
    /// session i's files, base first (`--package` i)
    sessions: Vec<Vec<PackageFile>>,
    installer: PathBuf,
    /// VM options + `-Djava.class.path` (the api-impl dex)
    vm: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    /// read-only, every installer run: the ART root, boot jars, api-impl dex at their own paths
    mounts: Vec<Mount>,
    /// a session's installer view besides `mounts`: framework-res alone at its Android path
    session_mounts: Vec<Mount>,
    /// a system scan's installer view besides `mounts`: /system, /product, /system_ext read-only at their Android paths and the host files their links name at their own
    scan_mounts: Vec<Mount>,
    /// staged sessions -> the uid they were staged for, and where
    staged: Mutex<BTreeMap<u32, Staged>>,
    /// scanned package dirs (Android paths) the installer parsed, not yet committed
    scanned: Mutex<BTreeSet<String>>,
    /// committed packages: what a start request may name
    committed: Mutex<BTreeMap<String, Committed>>,
}

/// One staged session: the uid it was staged for; `in_place`: the installed package whose code it equals (installed_code.rs), parsed where it lies instead of in a staging dir.
#[derive(Clone, PartialEq, Eq)]
struct Staged {
    uid: u32,
    in_place: Option<String>,
}

/// One committed package: its uid, and whether its code lies in a partition (a system scan) rather than /data/app.
#[derive(Clone, Copy)]
struct Committed {
    uid: u32,
    system: bool,
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
    /// A session equal to an installed package's code (installed_code.rs) is not staged: the installer runs over that code dir where it lies, its native-library root the package's own, with
    /// `--installed` (PMS's boot scan: parse, the install's ABI, no library copied). Log `install skipped: unchanged`.
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
        let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let in_place = installed_code::unchanged(&self.root.data_root, &paths)?;
        let (host_dir, at) = match &in_place {
            Some(p) => {
                log(format_args!(
                    "install session {session}: install skipped: unchanged ({p}'s installed code, parsed in place)"
                ));
                (
                    self.root.package(p, host_layout::Item::Apk),
                    Path::new(DATA_APP).join(p),
                )
            }
            None => {
                let name = staging_name(session);
                let host_dir = self.root.data().join("app").join(&name);
                // a dir a killed run left under this session name (the device outlives runs): the session's files must be the only ones here
                if host_dir.exists() {
                    std::fs::remove_dir_all(&host_dir)?;
                }
                std::fs::create_dir_all(&host_dir)?;
                (host_dir, Path::new(DATA_APP).join(name))
            }
        };
        let staged = (|| -> Result<String> {
            if in_place.is_none() {
                for f in files {
                    stage_file(&f.path, &host_dir.join(f.session_name()?))?;
                }
            }
            let lib = LIB_DIR_NAME;
            std::fs::create_dir_all(host_dir.join(lib))?;
            let install_dir = self.root.root.join("install");
            let frames = install_dir.join(format!("{session}.frames"));
            let log_path = self.root.log(&format!("installer-{session}"));
            let spawn = Spawn {
                uid: INSTALLER_UID,
                context_manager: false,
                binder_pids: BinderPids::Sandbox,
                context: INSTALLER_CONTEXT.to_owned(),
                mounts: i
                    .mounts
                    .iter()
                    .chain(&i.session_mounts)
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
                app_id: None,
                program: i.installer.clone(),
                // in place: the installer's boot-scan mode, no library copied
                args: in_place
                    .as_ref()
                    .map(|_| OsString::from("--installed"))
                    .into_iter()
                    .chain(i.vm.iter().cloned())
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
                "session {session}: {}",
                installer_failure(&status, &stderr)
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
                    .insert(session, Staged { uid, in_place });
                Ok(frames)
            }
            Err(e) => {
                // an installed package's code stays: only a staging dir is this session's own
                if in_place.is_none() {
                    let _ = std::fs::remove_dir_all(&host_dir);
                }
                Err(e)
            }
        }
    }

    /// Request `Commit`: session `session` (staged for `uid`) becomes `/data/app/<package>` (PackageInstallerSession's rename), with its data dirs (installd createAppData) and external dirs.
    fn commit(&self, session: u32, package: &str, uid: u32, names: &[StagedName]) -> Result<()> {
        ensure!(
            valid_package_name(package),
            "{package:?}: not a package name"
        );
        let i = &self.installd;
        let mut staged = i.staged.lock().unwrap_or_else(|e| e.into_inner());
        let in_place = match staged.get(&session) {
            Some(s) if s.uid == uid => s.in_place.clone(),
            _ => bail!("session {session}: not staged for uid {uid}"),
        };
        let mut committed = i.committed.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !committed.contains_key(package),
            "{package}: already installed"
        );
        match in_place {
            // the installed code is the parse's: its files already carry the AOSP names the parse gives
            Some(p) => {
                ensure!(
                    p == package,
                    "session {session}: equals {p}'s code but parsed as {package}"
                );
                ensure!(
                    names.iter().all(|n| n.staged == n.name),
                    "session {session}: {package}'s installed names differ from the parse's"
                );
            }
            None => {
                let code = self.root.package(package, host_layout::Item::Apk);
                let staged_dir = self.root.data().join("app").join(staging_name(session));
                rename_staged(&staged_dir, names)?;
                // the data root keeps the code across runs: the new code replaces an earlier install's, as an update does (the data dirs are its siblings and stay)
                if code.exists() {
                    std::fs::remove_dir_all(&code)?;
                }
                let parent = code.parent().context("package dir has no parent")?;
                std::fs::create_dir_all(parent)?;
                std::fs::rename(&staged_dir, &code)
                    .with_context(|| format!("moving session {session} to {}", code.display()))?;
            }
        }
        self.create_app_data(package)?;
        staged.remove(&session);
        committed.insert(package.to_owned(), Committed { uid, system: false });
        log(format_args!("installed {package}: uid {uid}"));
        Ok(())
    }

    /// Request `Scan`: runs the installer over the preinstalled package dir `dir` (an Android path), which must be one atl-services-types `system_packages::scan` finds in the root: parse only, in
    /// place (no staging, no native-library root). The frames file; a failed run's error carries the installer's stderr.
    fn scan(&self, dir: &str) -> Result<String> {
        let found = system_packages::scan(&self.root.root)?;
        let index = found
            .dirs
            .iter()
            .position(|d| d.android_path() == dir)
            .with_context(|| format!("{dir}: not a preinstalled package dir"))?;
        let i = &self.installd;
        let frames = self
            .root
            .root
            .join("install")
            .join(format!("scan-{index}.frames"));
        let log_path = self.root.log(&format!("installer-scan-{index}"));
        let spawn = Spawn {
            uid: INSTALLER_UID,
            context_manager: false,
            binder_pids: BinderPids::Sandbox,
            context: INSTALLER_CONTEXT.to_owned(),
            mounts: i.mounts.iter().chain(&i.scan_mounts).cloned().collect(),
            links: vec![],
            devices: vec![],
            property_area: Some(self.root.properties()),
            rights: vec![],
            app_id: None,
            program: i.installer.clone(),
            args: i.vm.iter().cloned().chain([OsString::from(dir)]).collect(),
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
            "scan {dir}: {}",
            installer_failure(&status, &stderr)
        );
        log(format_args!(
            "scan {dir}: installer {} ms",
            start.elapsed().as_millis()
        ));
        i.scanned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dir.to_owned());
        Ok(frames.to_string_lossy().into_owned())
    }

    /// Request `ScanCommit`: scanned `dir` is package `package` of `uid`: its data dirs and external dirs (installd createAppData, as for any package); no ES1 fact (no native-library root).
    /// Unlike a session's commit, a system uid is accepted: a system package of a predefined shared user runs as that user's fixed uid (android.uid.system = 1000), and only the runtime's
    /// read-only partitions are scanned. Root and the installer's uid stay refused.
    fn scan_commit(&self, dir: &str, package: &str, uid: u32) -> Result<()> {
        ensure!(
            uid != atl_services_types::permission::ROOT_UID && uid != INSTALLER_UID,
            "uid {uid}: root or the installer's ({INSTALLER_UID}); no package runs as either"
        );
        ensure!(
            valid_package_name(package),
            "{package:?}: not a package name"
        );
        let i = &self.installd;
        ensure!(
            i.scanned
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(dir),
            "{dir}: not scanned"
        );
        let mut committed = i.committed.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            !committed.contains_key(package),
            "{package}: already installed"
        );
        self.create_app_data(package)?;
        committed.insert(package.to_owned(), Committed { uid, system: true });
        log(format_args!("scanned {package}: uid {uid}, code {dir}"));
        Ok(())
    }

    /// installd createAppData: the package's data dirs and its external dirs.
    fn create_app_data(&self, package: &str) -> Result<()> {
        use host_layout::Item;
        for item in [Item::Data, Item::DataDe] {
            std::fs::create_dir_all(self.root.package(package, item))?;
        }
        for d in ["data", "obb"] {
            std::fs::create_dir_all(self.root.package(package, Item::External).join(d))?;
        }
        Ok(())
    }
}

/// Writes the default property area (system processes map it) and returns the properties it holds, to which each app process's area adds its own.
/// Sources as init's property service loads them: /system/build.prop, then /vendor/build.prop when `--file` places one (the device's: `ro.hardware.egl`), then the host's vendor props (`host_vendor`).
/// `tier`: the Codec2 linear allocator tier the host supports (atl_property_service::LinearTier::probe); its properties go into the set.
fn property_area(
    a: &Args,
    r: &Root,
    tier: LinearTier,
    host_vendor: &[Property],
) -> Result<Vec<Property>> {
    let mut text = std::fs::read_to_string(&a.build_prop)
        .with_context(|| format!("reading {}", a.build_prop.display()))?;
    let vendor = Path::new("vendor/build.prop");
    if let Some((host, _)) = a.files.iter().find(|(_, at)| at == vendor) {
        text.push('\n');
        text += &std::fs::read_to_string(host)
            .with_context(|| format!("reading {}", host.display()))?;
    }
    let mut props = host_props::apply(atl_property_service::defaults(&text, tier)?, host_vendor);
    props.push((
        atl_diag::PROP_MAX_KEYS_PROPERTY.to_owned(),
        a.prop_diag_max_keys.to_string(),
    ));
    std::fs::write(r.properties(), atl_property_service::area(&props)?)?;
    std::fs::create_dir(r.root.join("properties.d"))?;
    Ok(props)
}

/// What spawning a process takes beyond the process itself: everything the apps share, and the table of the processes the services started.
struct Host {
    /// The image's plat_seapp_contexts: each app process's context ([`Host::app_context`]).
    seapp: atl_sepolicy::Seapp,
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
    /// each reaped process's exit status, by host pid: a `LastAppEnded` of a live app ends the run only when its process exited 0
    exits: Mutex<BTreeMap<i32, ExitStatus>>,
    /// `--exit-with-last-app`, resolved
    exit_with_last_app: bool,
    /// why the run ends: the `--run-class` command's exit or an explicit stop
    done: Sender<End>,
    /// atl-am (`--am`): what the control command `start` runs
    am: Option<PathBuf>,
    /// atl-screencap (`--screencap`): what the control command `capture` runs
    screencap: Option<PathBuf>,
    /// The host paths every process of the runtime reads, at the same paths (ro_at_own_path): the LD_LIBRARY_PATH dirs, the ART root, the host files /system and /vendor link to.
    shared_ro: Vec<Mount>,
    /// With `--ui`: fontconfig's file and font dirs each app's GTK side reads, read-only at their own paths (atl_platform_host::app_access). Headless: none.
    desktop_ro: Vec<Mount>,
    /// With `--ui`: the compositor an app's GTK side connects to. Headless: None.
    wayland: Option<Wayland>,
    /// The desktop's PipeWire ([`PipeWire`]): every app process gets its context socket (or, without SecurityContext, the socket as-is) read-only at the socket's path and names that path to its launcher
    /// (`--pipewire-socket`). None: no server, and the apps' tracks fail.
    pipewire: Option<PipeWire>,
    /// The device nodes each app process may open: the run's GPU's class and the heap tier's heap (`run`).
    app_devices: Vec<PathBuf>,
    /// Variables only app processes get, after the runtime's own: the chosen render path's ([`choose_render_path`]), then `--app-env` (later wins). Never the allocator's or the services'.
    app_env: Vec<(OsString, OsString)>,
    /// The run's GPU: gralloc's DRM node and how it was chosen, pushed to each app process (atl_gralloc::node). None: no gralloc in this run.
    gpu: Option<(String, NodeSource)>,
    /// The guest cpuinfo of each translated ISA (`--cpuinfo-probe`): bound over /proc/cpuinfo of that ISA's app processes ([`cpuinfo_mount`]).
    cpuinfo: BTreeMap<String, PathBuf>,
    /// installd's role: the sessions' files, the installer, what is staged and installed (an app process mounts its package's own directories, package_mounts).
    installd: Installd,
    /// When the services' first app process started (wall-clock ms): the summary's first-seen offsets count from it.
    first_app_ms: OnceLock<u64>,
    /// The bounds of an app process's log capture ([`Host::bounded_log`]).
    log_limits: applog::Limits,
    /// The app processes' log copiers: joined at the run's end, before the summary reads the logs.
    app_logs: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// The desktop's notification service: the services' `Notify` / `Withdraw` requests land here (notifications plan N2: the sandboxed services have no session bus).
    notifications: PlatformNotifications,
    /// Packages whose post found no notification service, logged once each.
    unshown: Mutex<BTreeSet<String>>,
    /// Packages [`host_layout::migrate`] refused (data in both trees): never started this run.
    unmigrated: BTreeSet<String>,
}

/// `package`'s credential-encrypted data dir of user 0 as the app sees it (ApplicationInfo.dataDir, `/data/user/0/<package>`): writable in its sandbox ([`Host::package_mounts`]).
fn app_data_dir(package: &str) -> String {
    format!("{DATA_USER}/0/{package}")
}

/// The `--abi` ISAs (`<abi>=<isa>`) whose engine `.so` a `--file` places at atl-android-paths `engine_library`: the translated ISAs, each probed once for its guest cpuinfo. Sorted, once each.
/// `files` as [`partition_file`] parses them: Android paths relative to `/`.
fn staged_engines(abis: &[String], files: &[(PathBuf, PathBuf)]) -> Vec<String> {
    abis.iter()
        .filter_map(|a| a.split_once('=').map(|(_, isa)| isa.to_owned()))
        .filter(|isa| {
            let engine = PathBuf::from(atl_android_paths::engine_library(isa));
            files
                .iter()
                .any(|(_, at)| Path::new("/").join(at) == engine)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The guest cpuinfo bind of an app process of `instruction_set`: its cached file read-only at /proc/cpuinfo when that ISA was probed ([`staged_engines`]); none for the host ISA or a process of no package.
fn cpuinfo_mount(
    cpuinfo: &BTreeMap<String, PathBuf>,
    instruction_set: Option<&str>,
) -> Option<Mount> {
    instruction_set
        .and_then(|isa| cpuinfo.get(isa))
        .map(|host| Mount {
            host: host.clone(),
            at: PROC_CPUINFO.into(),
            writable: false,
        })
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
    /// the last app ended (`--exit-with-last-app`): a normal end, as `stop`
    LastApp,
    /// atld got SIGINT or SIGTERM (its number): the run stops as after `stop`
    Signal(i32),
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
    /// stdout + stderr of an app process: one pipe atld copies into `log/<name>.log` + its tail ring (appended) under [`Host::log_limits`] ([`applog`]); a drop also gives one atld line when the
    /// pipe ends.
    fn bounded_log(&self, name: &str) -> Result<(std::process::Stdio, std::process::Stdio)> {
        let (writer, copier) = bounded_copier(name, &self.root.log(name), self.log_limits)?;
        self.app_logs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(copier);
        Ok((writer.try_clone()?.into(), writer.into()))
    }

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
    /// the runtime's own ISA. `package`: the app's, whose data dir ([`app_data_dir`]) ART's native bridge gets as zygote's specialization hands it (`-Xnative-bridge-app-data-dir`, patches/aosp/art 0004: its code cache there, no
    /// mount namespace or cpuinfo mount of ART's own; the sandbox gives both); ignored by ART when no bridge is loaded.
    /// With `--ui` the sandbox shows the compositor socket's path; where the compositor has security contexts, that path is this process's own context socket, returned for [`Host::start`] to hold.
    /// Likewise the PipeWire socket's path shows this process's PipeWire context where PipeWire has SecurityContext; `rights` (the process's [`Right`]s) decide its audio / video source grants.
    /// The context of a process [`Host::app_spawn`] starts, keyed on what the process hosts: an app uid's from plat_seapp_contexts ([`atl_sepolicy::Seapp::app_context`]; seinfo `default`);
    /// the system uid's process named [`SYSTEM_PROCESS`] (SettingsProvider's `android:process="system"`) is system_server's own process on AOSP -> `system_server`; any other system uid package
    /// process (shared user android.uid.system, platform-signed) `system_app` (`user=system seinfo=platform domain=system_app`); the shell's commands (`--run-class` as AID_SHELL) `shell`.
    fn app_context(
        &self,
        uid: u32,
        name: &str,
        package: Option<&str>,
        target_sdk: i32,
    ) -> Result<String> {
        match atl_services_types::permission::app_id(uid) {
            AID_SYSTEM if name == SYSTEM_PROCESS => {
                Ok(atl_sepolicy::domains::SYSTEM_SERVER.to_owned())
            }
            AID_SYSTEM => Ok(atl_sepolicy::domains::SYSTEM_APP.to_owned()),
            AID_SHELL => Ok(atl_sepolicy::domains::SHELL.to_owned()),
            _ => self
                .seapp
                .app_context(atl_sepolicy::AppFacts {
                    uid,
                    package: package.unwrap_or(""),
                    target_sdk,
                })
                .with_context(|| format!("uid {uid}: no plat_seapp_contexts rule gives a domain")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn app_spawn(
        &self,
        uid: u32,
        name: &str,
        target_sdk: i32,
        instruction_set: Option<&str>,
        package: Option<&str>,
        boot: &[String],
        class: &str,
        args: &[String],
        rights: &[Right],
    ) -> Result<(Spawn, Held)> {
        let app_id = package.map(atl_launch::app_id).transpose()?;
        let (pipewire_mount, pipewire) = match &self.pipewire {
            None => (None, None),
            Some(PipeWire {
                socket,
                manager: None,
            }) => (Some(ro_at_own_path(socket)), None),
            Some(PipeWire {
                socket,
                manager: Some(m),
            }) => {
                let grants = pipewire_context::Grants {
                    record_audio: rights.contains(&Right::RecordAudio),
                    camera: rights.contains(&Right::Camera),
                };
                let ctx = m
                    .create(app_id.as_deref(), grants)
                    .with_context(|| format!("{name}: PipeWire security context"))?;
                let n = PIPEWIRE_CONTEXTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                match m.tier() {
                    pipewire_context::Tier::Enforced => log(format_args!(
                        "pipewire: security context applied tier=enforced ({name}: record_audio={} camera={}) contexts={n}",
                        grants.record_audio, grants.camera
                    )),
                    pipewire_context::Tier::WriteRestricted => log(format_args!(
                        "pipewire: security context applied tier=write-restricted (capture not restricted: session manager grants read to restricted clients) ({name}) contexts={n}"
                    )),
                }
                let m = Mount {
                    host: ctx.socket().to_path_buf(),
                    at: socket.clone(),
                    writable: false,
                };
                (Some(m), Some(ctx))
            }
        };
        let (wayland_mount, wayland) = match &self.wayland {
            None => (None, None),
            Some(Wayland {
                socket,
                contexts: None,
            }) => (Some(ro_at_own_path(socket)), None),
            Some(Wayland {
                socket,
                contexts: Some(c),
            }) => {
                let ctx = c
                    .create(app_id.as_deref())
                    .with_context(|| format!("{name}: Wayland security context"))?;
                log(format_args!("wayland: security context applied ({name})"));
                let m = Mount {
                    host: ctx.socket().to_path_buf(),
                    at: socket.clone(),
                    writable: false,
                };
                (Some(m), Some(ctx))
            }
        };
        let boot = boot.join(":");
        let launcher_args: Vec<OsString> =
            [format!("--target-sdk={target_sdk}"), "--app".to_owned()]
                .into_iter()
                .chain(instruction_set.map(|isa| format!("--instruction-set={isa}")))
                .chain(package.map(|p| format!("--package={p}")))
                .chain(
                    self.pipewire
                        .iter()
                        .map(|p| format!("--pipewire-socket={}", p.socket.display())),
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
                ])
                .chain(package.map(|p| format!("-Xnative-bridge-app-data-dir:{}", app_data_dir(p))))
                .chain([class.to_owned()])
                .chain(args.iter().cloned())
                .map(OsString::from)
                .collect();
        let (stdout, stderr) = self.bounded_log(name)?;
        Ok((
            Spawn {
                uid,
                context_manager: false,
                binder_pids: BinderPids::Sandbox,
                context: self.app_context(uid, name, package, target_sdk)?,
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
            // the preinstalled packages' partitions: a system app's code is readable by every app (WebView's CONTEXT_INCLUDE_CODE loads the provider's dex), as on a device
            .chain(self.root.package_partitions().map(|(host, at)| Mount {
                host,
                at: at.into(),
                writable: false,
            }))
            // the runtime's shared host paths; the package's own directories come with start_app (package_mounts)
            .chain(self.shared_ro.iter().cloned())
            .chain(self.desktop_ro.iter().cloned())
            .chain(wayland_mount)
            .chain(pipewire_mount)
            // a translated app's host readers see the guest's CPU, as on a device (libnativebridge MountCpuinfo); the broker binds it after the fresh /proc
            .chain(cpuinfo_mount(&self.cpuinfo, instruction_set))
            .collect(),
                links: vec![],
                devices: self.app_devices.clone(),
                property_area: Some(self.process_properties(uid, name)?),
                rights: vec![],
                app_id,
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
            },
            (wayland, pipewire),
        ))
    }

    /// A package's own directories (as commit made them): its APKs and native libraries read-only at /data/app/<package> (a `system` package's code is in its partition, bound for every app),
    /// its data and device-protected data read-write. Never the whole /data. Host side: the package's `apps/<package>/{apk,data,data_de}` ([`host_layout`]).
    fn package_mounts(&self, package: &str, system: bool) -> Vec<Mount> {
        use host_layout::Item;
        let code = (!system).then(|| (Item::Apk, format!("{DATA_APP}/{package}"), false));
        code.into_iter()
            .chain([
                (Item::Data, app_data_dir(package), true),
                (Item::DataDe, format!("{DATA_USER_DE}/0/{package}"), true),
            ])
            .map(|(item, at, writable)| Mount {
                host: self.root.package(package, item),
                at: PathBuf::from(at),
                writable,
            })
            .collect()
    }

    /// A package's own external dirs (`apps/<package>/external/{data,obb}`), bound at Android/{data,obb}/<package> under the shared storage root, with /sdcard linked to that root: no permission needed,
    /// never the root itself (plan Storage/SAF).
    fn external_storage(&self, spawn: &mut Spawn, package: &str) {
        let external = self.root.package(package, host_layout::Item::External);
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

    /// Starts `spawn`; a thread reaps it. `held`: the process's security contexts ([`Host::app_spawn`]), held until the process is reaped, so the compositor and PipeWire serve its sockets exactly while it lives.
    /// `main`: the `--run-class` command, whose end is the run's (`done`).
    fn start(
        self: &Arc<Self>,
        spawn: Spawn,
        held: Held,
        main: bool,
        controlled: bool,
    ) -> Result<i32> {
        let Process {
            mut child,
            host_pid,
            ..
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
                drop(held);
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
                        host.exits
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(host_pid, status);
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
        let Committed { uid, system } = self
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
        ensure!(
            !self.unmigrated.contains(&req.package),
            "{}: not started: its data is in both the old device tree and apps/{} under {} (not merged); move or remove one copy",
            req.package,
            req.package,
            self.root.data_root.display()
        );
        let (mut spawn, held) = self.app_spawn(
            req.uid,
            &req.process_name,
            req.target_sdk,
            Some(&req.instruction_set),
            Some(&req.package),
            &self.boot,
            &req.entry_point,
            &req.args,
            &req.rights,
        )?;
        self.external_storage(&mut spawn, &req.package);
        spawn
            .mounts
            .extend(self.package_mounts(&req.package, system));
        // /data/system is the system uid's (init.rc `mkdir /data/system 0775 system system`): a process of that uid (SettingsProvider, shared user android.uid.system) writes its state
        // there (Environment.getUserSystemDirectory: /data/system/users/<user>); no other uid sees it
        if atl_services_types::permission::app_id(req.uid) == AID_SYSTEM {
            let host = self.root.data().join("system");
            std::fs::create_dir_all(&host)?;
            spawn.mounts.push(Mount {
                host,
                at: PathBuf::from(format!("{DATA}/system")),
                writable: true,
            });
        }
        spawn.rights = req.rights.clone();
        let pid = self.start(spawn, held, false, true)?;
        let _ = self.first_app_ms.set(wall_ms());
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

    /// The services' program-pid request: only for a process they started.
    fn program_pid(&self, pid: i32) -> Result<i32> {
        ensure!(
            self.controlled
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&pid),
            "pid {pid} is not a process the services started"
        );
        Ok(atl_broker::program_pid(pid)?)
    }

    /// The services' `LastAppEnded`: with `--exit-with-last-app` the run ends when the app finished (window closed, or its last activity finished), or when it died with a live activity
    /// and its process exited 0 (its own exit; a crash or a signal is not a normal end, and the run goes on as without the setting). The status is waited for off the control thread.
    fn last_app_ended(self: &Arc<Self>, pid: i32, finished: bool) {
        if !self.exit_with_last_app {
            log(format_args!(
                "last app ended (pid {pid}): the run goes on (--exit-with-last-app false)"
            ));
            return;
        }
        if finished {
            log(format_args!(
                "last app ended (pid {pid}, finished): the run ends"
            ));
            let _ = self.done.send(End::LastApp);
            return;
        }
        let host = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name(format!("last-app-{pid}"))
            .spawn(move || {
                let deadline = Instant::now() + LAST_APP_EXIT_WAIT;
                let status = loop {
                    let seen = host
                        .exits
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&pid)
                        .copied();
                    if seen.is_some() || Instant::now() >= deadline {
                        break seen;
                    }
                    std::thread::sleep(STOP_POLL);
                };
                match status {
                    Some(s) if s.success() => {
                        log(format_args!(
                            "last app ended (pid {pid}, exited 0): the run ends"
                        ));
                        let _ = host.done.send(End::LastApp);
                    }
                    Some(s) => log(format_args!(
                        "last app ended (pid {pid}, {s}): not a normal end, the run goes on"
                    )),
                    None => log(format_args!(
                        "last app ended (pid {pid}): no exit status within {LAST_APP_EXIT_WAIT:?}, the run goes on"
                    )),
                }
            });
        if let Err(e) = spawned {
            log(format_args!("last app ended (pid {pid}): no thread: {e}"));
        }
    }
}

/// How long a `LastAppEnded` of a live app waits for its process's exit status: the services see the death when its binder fd closes, at its exit; bwrap reports the status a moment
/// later. 2 s is a loaded machine's margin.
const LAST_APP_EXIT_WAIT: Duration = Duration::from_secs(2);

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
                binder_pids: BinderPids::Host,
                context: atl_sepolicy::domains::SHELL.to_owned(),
                mounts: vec![],
                links: vec![],
                devices: vec![],
                property_area: Some(self.root.properties()),
                rights: vec![],
                app_id: None,
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
                binder_pids: BinderPids::Host,
                context: atl_sepolicy::domains::SHELL.to_owned(),
                mounts: vec![],
                links: vec![],
                devices: vec![],
                property_area: Some(self.root.properties()),
                rights: vec![],
                app_id: None,
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

/// The run's control socket, `<root>/control.sock` (maintainer decision 2026-11-06: the run ends on an explicit stop, not with an app): one command line per connection, one reply line.
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

impl Host {
    /// Shows `package`'s notification `key` through the desktop's service. A tier change is logged as it is seen; a post with no service is logged once per package (the services count each).
    fn notify(
        &self,
        package: &str,
        key: &str,
        content: control::NotificationContent,
    ) -> Result<control::NotifyOutcome> {
        let before = self.notifications.tier();
        let n = DesktopNotification {
            // no package label reaches atld yet: the package names the source (notifications plan N3 wants the label)
            app_name: package.to_owned(),
            app_id: atl_launch::app_id(package)?,
            summary: content.summary,
            body: content.body,
            urgency: match content.urgency {
                control::NotificationUrgency::Low => Urgency::Low,
                control::NotificationUrgency::Normal => Urgency::Normal,
            },
            resident: content.resident,
            silent: content.silent,
        };
        let posted = self.notifications.post(key, &n)?;
        let after = self.notifications.tier();
        if after != before {
            log(format_args!("notification: tier {}", after.label()));
        }
        Ok(match posted {
            Posted::Shown => control::NotifyOutcome::Shown,
            Posted::Updated => control::NotifyOutcome::Updated,
            Posted::NoService => {
                let mut unshown = self.unshown.lock().unwrap_or_else(|e| e.into_inner());
                if unshown.insert(package.to_owned()) {
                    log(format_args!(
                        "notification: not shown: no notification service ({package})"
                    ));
                }
                control::NotifyOutcome::NoService
            }
        })
    }

    /// Withdraws notification `key`; whether one of it was shown.
    fn withdraw(&self, key: &str) -> Result<bool> {
        Ok(self.notifications.withdraw(key)?)
    }
}

/// The control connection's server: answers the services' requests until they end it.
fn serve_control(host: Arc<Host>, fd: std::os::fd::OwnedFd) {
    let result = control::serve(&fd, |request| {
        let outcome = match request {
            Request::Start(req) => host.start_app(&req).map(|pid| Response::Started { pid }),
            Request::Kill { pid } => host.kill(pid).map(|()| Response::Killed),
            Request::ProgramPid { pid } => host
                .program_pid(pid)
                .map(|pid| Response::ProgramPid { pid }),
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
                .map(|()| Response::Committed),
            Request::Scan { dir } => host.scan(&dir).map(|frames| Response::Staged { frames }),
            Request::ScanCommit { dir, package, uid } => host
                .scan_commit(&dir, &package, uid)
                .map(|()| Response::Committed),
            Request::Notify {
                package,
                key,
                content,
            } => host
                .notify(&package, &key, content)
                .map(|outcome| Response::Notified { outcome }),
            Request::Withdraw { key } => host
                .withdraw(&key)
                .map(|shown| Response::Withdrawn { shown }),
            Request::LastAppEnded { pid, finished } => {
                host.last_app_ended(pid, finished);
                Ok(Response::Noted)
            }
            Request::Network => Ok(Response::Network {
                network: host_network().map(|n| n.to_arg()),
            }),
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

/// The error text of a failed installer run: its exit, the seccomp line when the filter killed it (atl_broker::seccomp_death, which also counts it), its stderr.
fn installer_failure(status: &std::process::ExitStatus, stderr: &str) -> String {
    match seccomp_death(Profile::Installer, status) {
        Some(sys) => format!("atl-installer exited {status}: {sys}\n{}", stderr.trim()),
        None => format!("atl-installer exited {status}: {}", stderr.trim()),
    }
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
    let started_ms = wall_ms();
    let root = std::path::absolute(&a.root)?;
    let data_root = match &a.data_root {
        Some(d) => std::path::absolute(d)?,
        None => atl_platform_host::user_data_root()
            .context("no default data root: neither XDG_DATA_HOME nor HOME names an absolute dir; pass --data-root")?,
    };
    ensure!(
        !data_root.starts_with(&root),
        "--data-root {}: inside --root {}, which is removed at start",
        data_root.display(),
        root.display()
    );
    log(format_args!("data root {}", data_root.display()));
    // before anything installs or creates package dirs: the old tree's package dirs move into apps/<package>/
    let migration = host_layout::migrate(&data_root)?;
    for (package, items) in &migration.moved {
        log(format_args!(
            "migrate {package}: {items:?} -> apps/{package}"
        ));
    }
    for (package, items) in &migration.refused {
        log(format_args!(
            "migrate {package}: refused, {items:?} in both device/ and apps/{package} (not merged; the package will not start)"
        ));
    }
    if !migration.moved.is_empty() || !migration.refused.is_empty() {
        log(format_args!(
            "migrate: {} packages moved, {} refused",
            migration.moved.len(),
            migration.refused.len()
        ));
    }
    let unmigrated: BTreeSet<String> = migration.refused.into_iter().map(|(p, _)| p).collect();
    if a.root.exists() {
        std::fs::remove_dir_all(&a.root)?;
    }
    let r = Root { root, data_root };
    std::fs::create_dir_all(r.root.join("log"))?;
    let tier = LinearTier::probe();
    log(format_args!("linear allocator tier {}", tier.name()));
    // the host's own vendor props, applied over the bundled ones in property_area; a refused line is logged, the rest stands
    let host_vendor = host_props::read(Path::new(host_props::HOST_VENDOR_PROP));
    for m in &host_vendor.refused {
        log(format_args!("{}: {m}", host_props::HOST_VENDOR_PROP));
    }
    let mut defaults = property_area(a, &r, tier, &host_vendor.props)?;
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
    let stub = pin_helper(&r.helpers(), &a.sandbox_stub)?;
    let (broker, mut binderd) =
        Broker::start(&a.binderd, &stub, std::fs::File::create(r.log("binderd"))?)?;
    let broker = if a.no_app_seccomp {
        broker.without_app_seccomp()
    } else {
        broker
    };
    let broker = match (&a.sandbox_join, &a.flatpak_spawn) {
        (Some(join), Some(shim)) => {
            // the web process's share-sound socket, found as libpulse finds its server (PULSE_SERVER, then the runtime dir's pulse/native)
            let pulse = atl_platform_host::app_access::pulse_socket();
            let from = atl_platform_host::app_access::pulse_socket_source();
            match &pulse {
                Some(p) => log(format_args!(
                    "web process sound: pulse socket {} (from {from})",
                    p.display()
                )),
                None => log(format_args!(
                    "web process sound: no pulse socket (from {from}; share-sound grants nothing)"
                )),
            }
            // pinned as the stub is: a replaced host file would leave a moment with no file (a failed exec or bind) or a version beside a running atld it was not built with
            broker.with_webspawn(atl_broker::WebSpawnHost {
                join: pin_helper(&r.helpers(), join)?,
                shim: pin_helper(&r.helpers(), shim)?,
                pulse,
            })
        }
        _ => broker,
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
            binder_pids: BinderPids::Host,
            // system_server's: atl-system-services and the probes doing system_server's work (ABI choice, WebView provider); a HAL or daemon sets its own domain, as it sets its uid
            context: atl_sepolicy::domains::SYSTEM_SERVER.to_owned(),
            // the VINTF manifests and every partition's service_contexts (libselinux android.c: /system, /system_ext, /product, /vendor) are servicemanager's alone to read; the others
            // reach HALs and services through it
            mounts: shared_ro
                .iter()
                .cloned()
                .chain(if cm {
                    [(SYSTEM, r.system()), (VENDOR, r.vendor())]
                        .into_iter()
                        .chain(r.package_partitions().map(|(host, at)| (at, host)))
                        .map(|(at, host)| Mount {
                            host,
                            at: PathBuf::from(at),
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
            app_id: None,
            program: program.to_owned(),
            args,
            env: vec![lib.clone()],
            cwd: None,
            stdin,
            stdout,
            stderr,
        })
    };
    let mut sm_spawn = system_spawn("servicemanager", &a.servicemanager, vec![], true, None)?;
    sm_spawn.context = atl_sepolicy::domains::SERVICEMANAGER.to_owned();
    kernel_log_to_log(&mut sm_spawn, &r.log("servicemanager"))?;
    let mut sm = broker.spawn(sm_spawn).context("starting servicemanager")?;
    log(format_args!("servicemanager: host pid {}", sm.host_pid));
    // the daemons servicemanager's clients reach, in init's order: the HALs (hal / early_hal classes), then keystore2 (class main); each logs through its own logdw socket
    let mut daemons = Vec::new();
    let sockets = r.root.join("logdw");
    std::fs::create_dir_all(&sockets)?;
    let limits = log_limits(a);
    // the health HAL (its .rc: `user system`), the battery services' source: the host's power-supply sysfs read-only, nothing else of /sys (atl_broker::power_supply_sysfs)
    if let Some(health) = &a.health {
        let mut hal = system_spawn("health", health, vec![], false, None)?;
        hal.context = atl_sepolicy::domains::HEALTH.to_owned();
        logdw_to_log(
            &mut hal.mounts,
            &sockets.join("health"),
            "health",
            &r.log("health"),
            limits,
        )?;
        let (mounts, links) =
            atl_broker::power_supply_sysfs().context("the host's power-supply sysfs")?;
        hal.mounts.extend(mounts);
        hal.links.extend(links);
        daemons.push((
            "health",
            broker.spawn(hal).context("starting the health HAL")?,
        ));
    }
    // the KeyMint HAL, then keystore2 on it (init's order: early_hal before keystore2's class main); keystore2 alone holds its database dir
    if let (Some(keystore2), Some(keymint)) = (&a.keystore2, &a.keymint) {
        let mut hal = system_spawn("keymint", keymint, vec![], false, None)?;
        hal.uid = AID_NOBODY;
        hal.context = atl_sepolicy::domains::KEYMINT.to_owned();
        logdw_to_log(
            &mut hal.mounts,
            &sockets.join("keymint"),
            "keymint",
            &r.log("keymint"),
            limits,
        )?;
        daemons.push((
            "keymint",
            broker.spawn(hal).context("starting the KeyMint HAL")?,
        ));
        let db = atl_android_paths::keystore_db_host(&r.data_root);
        std::fs::create_dir_all(&db)?;
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o700))?;
        let mut ks = system_spawn(
            "keystore2",
            keystore2,
            vec![KEYSTORE_DB.into()],
            false,
            None,
        )?;
        ks.uid = AID_KEYSTORE;
        ks.context = atl_sepolicy::domains::KEYSTORE.to_owned();
        logdw_to_log(
            &mut ks.mounts,
            &sockets.join("keystore2"),
            "keystore2",
            &r.log("keystore2"),
            limits,
        )?;
        ks.mounts.push(Mount {
            host: db,
            at: KEYSTORE_DB.into(),
            writable: true,
        });
        daemons.push(("keystore2", broker.spawn(ks).context("starting keystore2")?));
    }
    for (name, p) in &daemons {
        log(format_args!("{name}: host pid {}", p.host_pid));
    }
    // the guest cpuinfo before any app can start: the probe under the installed engine, with /system as apps see it
    let mut cpuinfo = BTreeMap::new();
    if let Some(probe) = &a.cpuinfo_probe {
        for isa in staged_engines(&a.abis, &a.files) {
            let name = format!("cpuinfo-probe-{isa}");
            let cached = r.root.join(format!("cpuinfo.{isa}"));
            let mut spawn = system_spawn(&name, probe, vec![isa.clone().into()], false, None)?;
            spawn.mounts.push(Mount {
                host: r.system(),
                at: SYSTEM.into(),
                writable: false,
            });
            spawn.stdout = std::fs::File::create(&cached)?.into();
            let status = broker
                .spawn(spawn)
                .with_context(|| format!("starting {}", probe.display()))?
                .child
                .wait()?;
            let len = std::fs::metadata(&cached)?.len();
            ensure!(
                status.success() && len > 0,
                "{} {isa} exited {status} after {len} bytes:\n{}",
                probe.display(),
                std::fs::read_to_string(r.log(&name))?.trim()
            );
            log(format_args!("cpuinfo-probe: {isa} {len} bytes"));
            cpuinfo.insert(isa, cached);
        }
    }
    // the run's GPU node before gralloc starts (gralloc starts below the render-path probe, before any app can: a --launch request may come with the services' first breath): the display's node, else gralloc's trial run in the probe
    // (node, how chosen, the display's modifier lists, the display probe's renderer or why it named no node)
    type Planned = (String, NodeSource, Vec<(u32, Vec<u64>)>, String);
    let mut planned: Option<Planned> = None;
    if a.gralloc.is_some() {
        let Some(probe) = &a.gpu_probe else {
            bail!("--gralloc needs --gpu-probe (atl-egl-probe)");
        };
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
                spawn.context = atl_sepolicy::domains::GRAPHICS_ALLOCATOR.to_owned();
                // every GPU's class: the probe asks which one the display picks, and NVIDIA's EGL opens its nodes even to decline
                spawn.devices = gpu::all_device_classes().context("the GPUs' device nodes")?;
                log(format_args!("gpu-probe: devices {:?}", spawn.devices));
                spawn.mounts.extend(desktop_mounts(true)?);
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
        planned = Some(match display_node {
            Ok((n, renderer, modifiers)) => (n, NodeSource::Display, modifiers, renderer),
            Err(why) => {
                // gralloc's own trial (atl_gralloc_service Backend::start), in the probe, with the grant the trial needs: every GPU
                let mut spawn =
                    system_spawn("gpu-trial", probe, vec!["--trial-node".into()], false, None)?;
                spawn.context = atl_sepolicy::domains::GRAPHICS_ALLOCATOR.to_owned();
                spawn.devices = gpu::all_device_classes().context("the GPUs' device nodes")?;
                let mut p = broker
                    .spawn(spawn)
                    .with_context(|| format!("starting {}", probe.display()))?;
                let status = p.child.wait()?;
                let text = std::fs::read_to_string(r.log("gpu-trial"))?;
                let n = node::parse_trial_line(&text)
                    .filter(|_| status.success())
                    .with_context(|| {
                        format!(
                            "{} --trial-node exited {status}:\n{}",
                            probe.display(),
                            text.trim()
                        )
                    })?;
                (n, NodeSource::Trial, vec![], why)
            }
        });
    }
    let (control_ours, control_theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let (done_tx, done_rx): (Sender<End>, Receiver<End>) = channel();
    // SIGINT (a terminal's Ctrl-C) and SIGTERM (a supervisor, `timeout`) end the run as `stop` does, so the system processes still stop in order and write their totals
    {
        let mut signals = Signals::new([SIGINT, SIGTERM]).context("SIGINT/SIGTERM handler")?;
        let done = done_tx.clone();
        std::thread::Builder::new()
            .name("run-signals".into())
            .spawn(move || {
                if let Some(sig) = signals.forever().next() {
                    let _ = done.send(End::Signal(sig));
                }
            })?;
    }
    // the services and every app process answer with this one display
    let display = a.display.unwrap_or_else(host_display);
    // its density, as a device's vendor build.prop states its panel's (DisplayMetrics DENSITY_DEVICE); into the services' and the apps' areas below
    defaults.push(atl_property_service::lcd_density(display.dpi));
    // the desktop's UI language (LC_MESSAGES, atl_platform_host::ui_locale): the apps' persist.sys.locale and the services' --locale; none: AOSP's en-US
    let locale = atl_platform_host::ui_locale();
    log(format_args!(
        "locale: {}",
        locale.as_deref().unwrap_or("none, en-US")
    ));
    if let Some(tag) = &locale {
        defaults.push(atl_property_service::persist_locale(tag));
    }
    // the desktop's time zone (TZ, else /etc/localtime: atl_platform_host::time_zone): the apps' persist.sys.timezone; none: AOSP's GMT
    let zone = atl_platform_host::time_zone();
    log(format_args!(
        "timezone: {}",
        zone.as_deref().unwrap_or("none, GMT")
    ));
    if let Some(id) = &zone {
        defaults.push(atl_property_service::persist_timezone(id));
    }
    // the run's GPU's device class (gpu::device_class), as AOSP grants an app gpu_device per device (system/sepolicy private/app.te:393): the host EGL driver renders into, and imports, the gralloc dmabufs through them;
    // without them it falls back to the software device, which cannot import a window's buffers. No gralloc: no chosen GPU, every GPU's class.
    // + the DMABUF system heap on the heap tier only (narrowest grant): Codec2 allocates linear blocks from it in the app process
    let mut app_devices = match &planned {
        Some((n, ..)) => gpu::device_class(Path::new(n)),
        None => gpu::all_device_classes(),
    }
    .context("the apps' device nodes")?;
    if tier == LinearTier::DmaBufHeap {
        app_devices.push(PathBuf::from(atl_property_service::DMA_HEAP_SYSTEM));
    }
    // the render path: baseline (Mesa's default on the run's GPU) unless a kernel-offered candidate initialises and renders on something else (gpu::RENDER_PATHS); one `gpu-render: path` line records which ran
    let mut app_env = Vec::new();
    // the system features this host gives the apps (HOST_FEATURES_FILE): what the probes find and the desktop adaptations make true
    let mut host_features: Vec<(&str, i32)> = Vec::new();
    if a.ui.is_some() && a.mouse_touch {
        // the left button is one finger on SOURCE_TOUCHSCREEN: a single-touch screen, as AOSP's android.hardware.touchscreen.xml declares it (no multitouch: one pointer)
        host_features.extend(FEATURES_TOUCHSCREEN);
    }
    // whether the apps' GPUs import a gralloc buffer, as the probe of the path that runs reports it (node::IMPORT_LINE): gralloc's allocator tier
    let mut imports = None;
    if let (Some((n, ..)), Some(probe)) = (&planned, &a.gpu_probe) {
        let probe_renderer = |name: &str,
                              devices: Vec<PathBuf>,
                              env: Vec<(OsString, OsString)>|
         -> Result<
            std::result::Result<(atl_gralloc::node::Renderer, Option<node::Imports>), String>,
        > {
            let mut spawn = system_spawn(
                name,
                probe,
                vec!["--renderer".into(), "--render".into(), n.into()],
                false,
                None,
            )?;
            spawn.context = atl_sepolicy::domains::GRAPHICS_ALLOCATOR.to_owned();
            spawn.devices = devices;
            // as arguments, not the probe's environment: it allocates its import buffer before setting them, as the allocator (without them) does
            for (k, v) in env {
                let mut kv = k;
                kv.push("=");
                kv.push(v);
                spawn.args.push("--render-env".into());
                spawn.args.push(kv);
            }
            spawn.mounts.extend(desktop_mounts(a.ui.is_some())?);
            let mut p = broker
                .spawn(spawn)
                .with_context(|| format!("starting {}", probe.display()))?;
            let status = p.child.wait()?;
            let text = std::fs::read_to_string(r.log(name))?;
            Ok(atl_gralloc::node::parse_renderer_line(&text)
                .filter(|_| status.success())
                .map(|rend| (rend, node::parse_import_lines(&text)))
                .ok_or_else(|| format!("probe exited {status}: {}", text.trim())))
        };
        let candidates = gpu::render_paths();
        // probed even without candidates: its GL_VERSION is what the apps get on the baseline (ro.opengles.version)
        let baseline = probe_renderer("gpu-render-baseline", app_devices.clone(), vec![])?;
        let chosen = if candidates.is_empty() {
            Err("no kernel render path offered".to_owned())
        } else {
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
                    (Ok(rend), Ok(base)) if rend.0.renderer != base.0.renderer => {
                        chosen = Ok((path, rend, env));
                        break;
                    }
                    (Ok(rend), _) => {
                        chosen = Err(format!(
                            "{}: renders on the baseline's {}",
                            path.module, rend.0.renderer
                        ))
                    }
                    (Err(e), _) => chosen = Err(format!("{}: {e}", path.module)),
                }
            }
            chosen
        };
        // what the GL of the path that runs reports; None when no probe answered
        let ran = match chosen {
            Ok((path, (rend, imp), env)) => {
                log(format_args!(
                    "gpu-render: path {} (renderer {}, {})",
                    path.module, rend.renderer, rend.version
                ));
                app_devices.extend(path.nodes.iter().map(PathBuf::from));
                app_env = env;
                imports = imp;
                Some(rend)
            }
            Err(why) => {
                log(format_args!("gpu-render: path baseline ({why})"));
                baseline.ok().map(|(rend, imp)| {
                    imports = imp;
                    rend
                })
            }
        };
        // the extension pack (a device with it declares the feature, CDD 7.1.4.1, as its vendor permission file does) and the GLES version of the path that runs (the vendor build.prop's, on Android); unset when no probe answered or the string is not GLES's; both via device_gl
        let advertised = ran.as_ref().map(|r| (r, device_gl(&r.version, r.aep)));
        match advertised {
            Some((r, (aep, gles))) => {
                if aep {
                    host_features.push(FEATURE_OPENGLES_AEP);
                }
                match gles {
                    Some(prop) => {
                        log(format_args!("gles: {}={} ({})", prop.0, prop.1, r.version));
                        defaults.push(prop);
                    }
                    None => log(format_args!(
                        "gles: unset (GL_VERSION {:?} not GLES)",
                        r.version
                    )),
                }
            }
            None => log(format_args!("gles: unset (no renderer probed)")),
        }
    }
    // gralloc on the planned node, with its allocator tier: udmabuf only where no GPU of an app process imports its buffer and /dev/udmabuf opens (node::choose_allocator_tier); the device is bound only then
    let mut gralloc = None;
    let mut gpu = None;
    if let (Some(program), Some((n, source, modifiers, renderer))) = (&a.gralloc, planned) {
        let udmabuf = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(UDMABUF_DEVICE)
            .map(drop)
            .map_err(|e| format!("{UDMABUF_DEVICE}: {e}"));
        let (want, why) = node::choose_allocator_tier(imports.as_ref(), udmabuf);
        let mut spawn = system_spawn(
            "gralloc",
            program,
            vec![
                "--drm-node".into(),
                (&n).into(),
                "--allocator-tier".into(),
                want.name().into(),
            ],
            false,
            None,
        )?;
        spawn.context = atl_sepolicy::domains::GRAPHICS_ALLOCATOR.to_owned();
        for (fourcc, mods) in &modifiers {
            let arg = node::modifier_arg(*fourcc, mods);
            // own prefix: `gpu:` is the one node line the run-apk gate reads
            log(format_args!("gpu-modifiers: {arg} (probe)"));
            // which modifier tier GPU-only buffers of this fourcc take: explicit, implicit (node::with_implicit) or none (refused)
            log(format_args!(
                "modifier tier {} {}",
                node::fourcc_name(*fourcc),
                node::modifier_tier_name(mods)
            ));
            spawn.args.push("--modifiers".into());
            spawn.args.push(arg.into());
        }
        spawn.devices = gpu::device_class(Path::new(&n)).context("gralloc's device nodes")?;
        if want == AllocatorTier::Udmabuf {
            spawn.devices.push(PathBuf::from(UDMABUF_DEVICE));
        }
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
        let text = std::fs::read_to_string(r.log("gralloc"))?;
        let allocated = gralloc_node(&text).context("gralloc's ready line names no node")?;
        // the tier gralloc runs (a udmabuf request that does not allocate falls back to gbm and says why)
        let (ran, fallback) =
            node::parse_tier_line(&text).context("gralloc printed no allocator tier line")?;
        let why = if ran == want {
            why
        } else {
            format!("{why}; {fallback}")
        };
        log(format_args!("{}", node::tier_line(ran, &why)));
        log(format_args!("gralloc ready"));
        gralloc = Some(p);
        ensure!(
            allocated == n,
            "gralloc allocates on {allocated}, the probe chose {n}"
        );
        match source {
            NodeSource::Display => {
                log(format_args!("gpu: node {n} (display, renderer {renderer})"));
            }
            NodeSource::Trial => log(format_args!(
                "gpu: display reports no node ({renderer}); allocator node {n} by trial"
            )),
        }
        gpu = Some((allocated, source));
    }
    // the host's WebView capability: the web engine module loads in a process with the apps' library path (PROBE_WEB_ENGINE)
    {
        let name = "webview-probe";
        let exe = std::env::current_exe().context("atld's own path")?;
        let spawn = system_spawn(name, &exe, vec![PROBE_WEB_ENGINE.into()], false, None)?;
        let mut p = broker
            .spawn(spawn)
            .with_context(|| format!("starting {name}"))?;
        let status = p.child.wait()?;
        if status.success() {
            host_features.push(FEATURE_WEBVIEW);
        } else {
            let text = std::fs::read_to_string(r.log(name))?;
            log(format_args!(
                "webview: no engine ({status}: {})",
                text.trim()
            ));
        }
    }
    host_features_layout(&r, &host_features)?;
    // the services' area: the defaults with what the probes above added (ActivityTaskManagerService reads ro.opengles.version); the processes started before the probes keep the first area
    let system_area = r.root.join("properties.d").join("system");
    std::fs::write(&system_area, atl_property_service::area(&defaults)?)?;
    // AOSP's ro.hw_timeout_multiplier (Build.java:1744): the services scale their timers by it; the host file's value, when set, is the one in `defaults`
    let (hw, source) = host_props::hw_multiplier(&defaults, &host_vendor.props);
    log(format_args!("hw timeouts: x{} {source}", hw.to_arg()));
    app_env.extend(a.app_env.iter().cloned());
    log(format_args!("apps' devices {app_devices:?}"));
    let host = Arc::new(Host {
        seapp: load_seapp(&a.files)?,
        app_env,
        app_devices,
        gpu,
        cpuinfo,
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
            esc_back: a.esc_back,
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
        exits: Mutex::new(BTreeMap::new()),
        exit_with_last_app: a.exit_with_last_app.unwrap_or(a.ui.is_some()),
        done: done_tx,
        am: a.am.clone(),
        screencap: a.screencap.clone(),
        shared_ro: shared_ro.clone(),
        desktop_ro: if a.ui.is_some() {
            font_mounts()?
        } else {
            vec![]
        },
        wayland: a.ui.is_some().then(Wayland::probe).transpose()?,
        pipewire: PipeWire::probe(),
        first_app_ms: OnceLock::new(),
        log_limits: log_limits(a),
        app_logs: Mutex::new(Vec::new()),
        notifications: {
            let n = PlatformNotifications::connect();
            log(format_args!("notification: tier {}", n.tier().label()));
            n
        },
        unshown: Mutex::new(BTreeSet::new()),
        unmigrated,
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
                m
            },
            session_mounts: vec![Mount {
                host: a.framework_res.clone(),
                at: Path::new(SYSTEM_FRAMEWORK).join(FRAMEWORK_RES),
                writable: false,
            }],
            // /system whole (its framework-res link resolves to the host file, bound at its own path), the package partitions, the `--file` sources their links name
            scan_mounts: std::iter::once((r.system(), SYSTEM))
                .chain(r.package_partitions())
                .map(|(host, at)| Mount {
                    host,
                    at: at.into(),
                    writable: false,
                })
                .chain(std::iter::once(ro_at_own_path(&a.framework_res)))
                .chain(a.files.iter().map(|(host, _)| ro_at_own_path(host)))
                .collect(),
            staged: Mutex::new(BTreeMap::new()),
            scanned: Mutex::new(BTreeSet::new()),
            committed: Mutex::new(BTreeMap::new()),
        },
        root: Root {
            root: r.root.clone(),
            data_root: r.data_root.clone(),
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
    for abi in &a.abis {
        services_args.extend(["--abi".into(), abi.into()]);
    }
    if let Some(p) = &a.platform_manifest {
        services_args.extend(["--platform-manifest".into(), p.into()]);
    }
    for library in &libraries {
        services_args.extend(["--shared-library".into(), library.into()]);
    }
    if let Some(p) = &a.launch {
        services_args.extend(["--launch".into(), p.into()]);
    }
    services_args.extend(["--display".into(), display.to_arg().into()]);
    services_args.extend(["--hw-timeout-multiplier".into(), hw.to_arg().into()]);
    if let Some(tag) = &locale {
        services_args.extend(["--locale".into(), tag.into()]);
    }
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
    if a.no_bind_service {
        services_args.push("--no-bind-service".into());
    }
    if a.no_job_runner {
        services_args.push("--no-job-runner".into());
    }
    services_args.push("--root-back".into());
    services_args.push(a.root_back.clone().into());
    services_args.push("--orientation-reshape".into());
    services_args.push(a.orientation_reshape.clone().into());
    services_args.extend([
        "--empty-process-grace-ms".into(),
        a.empty_process_grace_ms.to_string().into(),
    ]);
    services_args.extend([
        "--window-close-stops-services".into(),
        a.window_close_stops_services.to_string().into(),
    ]);
    for package in &a.autostart {
        services_args.extend(["--autostart".into(), package.clone().into()]);
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
            atl_services_wire::sensor::TILT_INJECTOR_PACKAGE.into(),
        ]);
    }
    if a.no_sensor_focus_check {
        services_args.push("--no-sensor-focus-check".into());
    }
    // the same adaptations the app processes get (`ro.atl.desktop-input`): the input service serves the keyboard map they select
    services_args.extend(["--desktop-input".into(), host.desktop_input.to_arg().into()]);
    // the key maps are /system files (`--file`, usr/keychars); the services read them where they lie in the root
    services_args.extend([
        "--keychars".into(),
        r.system().join("usr/keychars").into_os_string(),
    ]);
    // SystemConfig's permission files lie in the root's partitions: system (mounted for every process) and vendor (HOST_FEATURES_FILE and `--file` placements), by host path
    services_args.extend(["--partitions".into(), r.root.clone().into_os_string()]);
    // the services' own state (shortcut_service, package_state), kept across runs in the data root and writable by them alone
    let system_dir = host_layout::system_dir(&r.data_root);
    std::fs::create_dir_all(&system_dir)?;
    services_args.extend(["--system-dir".into(), system_dir.clone().into_os_string()]);
    let mut services_spawn = system_spawn(
        "services",
        &a.services,
        services_args,
        false,
        Some(control_theirs),
    )?;
    services_spawn.property_area = Some(system_area);
    // what the install reads by host path: the sessions' manifests and the installer's frames, the platform's permission definitions
    services_spawn.mounts.push(ro_at_own_path(&install_dir));
    services_spawn.mounts.push(ro_at_own_path(&r.vendor()));
    services_spawn.mounts.push(Mount {
        host: system_dir.clone(),
        at: system_dir,
        writable: true,
    });
    // the system scan lists the package partitions' priv-app/ and app/ dirs (atl-services-types system_packages) where they lie in the root
    for (host, _) in r.package_partitions() {
        services_spawn.mounts.push(ro_at_own_path(&host));
    }
    if let Some(p) = &a.platform_manifest {
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
        // one installer run per session and per preinstalled package dir
        READY
            + INSTALL_READY
                * u32::try_from(a.packages.len() + system_packages::scan(&r.root)?.dirs.len())?,
    )?;
    log(format_args!("services ready"));

    if let (Some(class), Some(uid), Some(name)) = (&a.run_class, a.run_uid, &a.run_name) {
        let boot: Vec<String> = host.boot.iter().cloned().chain(run_boot).collect();
        let (spawn, held) = host.app_spawn(
            uid,
            name,
            PLATFORM_SDK,
            None,
            None,
            &boot,
            class,
            &a.run_args,
            &[],
        )?;
        let pid = host.start(spawn, held, true, false)?;
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
    // system daemons that exit before the run's end: one line + a `system:died` diag count each, the run's status fails; a daemon is not restarted (an orderly stop never happens mid-run)
    let mut died: Vec<(&'static str, ExitStatus)> = Vec::new();
    let end = loop {
        match done_rx.recv_timeout(SERVICES_POLL) {
            Ok(end) => break end,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(s) = services.child.try_wait()? {
                    break End::Services(s);
                }
                reap_died(
                    system_processes(&mut sm, gralloc.as_mut(), &mut daemons),
                    &mut died,
                )?;
            }
            Err(RecvTimeoutError::Disconnected) => bail!("the run's end channel closed"),
        }
    };
    // a daemon that died after the last poll still gets its verdict, and is not stopped below
    reap_died(
        system_processes(&mut sm, gralloc.as_mut(), &mut daemons),
        &mut died,
    )?;
    let code = match &end {
        End::Command(status) => {
            log(format_args!("command ended: {status}"));
            status.code()
        }
        End::Stop | End::LastApp => Some(0),
        End::Services(status) => {
            log(format_args!("services ended: {status}"));
            None
        }
        End::Signal(sig) => {
            log(format_args!("signal {sig}: stopping"));
            None
        }
    };
    // what the services started dies with the run, SIGKILLed as AOSP kills an app process; then the system processes stop the Linux way (SIGTERM, the grace, SIGKILL; maintainer decision 3), each
    // writing its exit totals, and binderd last, by its admin connection, printing its own counters
    let grace = Duration::from_millis(a.stop_grace_ms);
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
    // a daemon seen dead mid-run was reaped already: nothing to stop
    let mut stopping = system_processes(&mut sm, gralloc.as_mut(), &mut daemons);
    stopping.retain(|(name, _)| !died.iter().any(|(n, _)| n == name));
    stopping.push(("services", &mut services));
    std::thread::scope(|s| {
        for (name, p) in stopping {
            s.spawn(move || stop_system(name, p, grace));
        }
    });
    for (name, status) in &died {
        log(format_args!(
            "verdict: system process {name} died during the run: {status}"
        ));
    }
    if let Err(e) = host.broker.close_admin() {
        log(format_args!("binderd: closing its admin connection: {e}"));
    }
    let deadline = Instant::now() + grace;
    loop {
        if binderd.child.try_wait()?.is_some() {
            log(format_args!("stopped binderd"));
            break;
        }
        if Instant::now() >= deadline {
            log(format_args!(
                "binderd: no exit {grace:?} after its admin connection closed: killed"
            ));
            let _ = binderd.kill();
            let _ = binderd.child.wait();
            break;
        }
        std::thread::sleep(STOP_POLL);
    }
    let since = host.first_app_ms.get().copied().unwrap_or(started_ms);
    // the app logs' copiers end at their pipes' EOF (every app process was killed above); the summary reads what they wrote
    let copiers = std::mem::take(&mut *host.app_logs.lock().unwrap_or_else(|e| e.into_inner()));
    let deadline = Instant::now() + grace;
    for c in copiers {
        while !c.is_finished() && Instant::now() < deadline {
            std::thread::sleep(STOP_POLL);
        }
        if c.is_finished() {
            let _ = c.join();
        } else {
            log(format_args!(
                "an app log copier still runs {grace:?} after the end: its log may be cut"
            ));
        }
    }
    drop(host);
    // the C4 row cap of each family block (atl_diag::FAMILIES)
    let prop_rows = a.prop_diag_summary_rows;
    print_summary(&r.root.join("log"), since, |fam| match fam {
        atl_diag::PROP_FAMILY => prop_rows,
        _ => usize::MAX,
    });
    Ok(match (end, code) {
        // a signal's conventional status (128 + its number), as a shell reports a process it ended
        (End::Signal(sig), _) => {
            u8::try_from(SIGNAL_STATUS_BASE + sig).map_or(ExitCode::FAILURE, ExitCode::from)
        }
        (_, Some(0)) if died.is_empty() => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

/// atld's diag outcome for a system daemon that exited before the run's end (api = its name, cause = its status).
const SYSTEM_DIED: &str = "system:died";

/// Every system daemon atld started except the services (whose exit ends the run): servicemanager, gralloc, step 4's daemons (health HAL, the keystore pair).
fn system_processes<'a>(
    sm: &'a mut Process,
    gralloc: Option<&'a mut Process>,
    daemons: &'a mut [(&'static str, Process)],
) -> Vec<(&'static str, &'a mut Process)> {
    let mut v = vec![("servicemanager", sm)];
    v.extend(gralloc.map(|p| ("gralloc", p)));
    v.extend(daemons.iter_mut().map(|(n, p)| (*n, p)));
    v
}

/// Records each daemon of `processes` whose sandbox has ended (its pidfd: exact, no pid reuse) and has no entry in `died` yet: one line + a `system:died` diag count, its bwrap reaped for the status.
fn reap_died(
    processes: Vec<(&'static str, &mut Process)>,
    died: &mut Vec<(&'static str, ExitStatus)>,
) -> Result<()> {
    for (name, p) in processes {
        if died.iter().any(|(n, _)| *n == name) || !p.ended(None)? {
            continue;
        }
        // the sandbox's init exited: bwrap, its parent, exits with it
        let s = p.child.wait()?;
        log(format_args!("system process {name} died: {s}"));
        atl_diag::count(SYSTEM_DIED, name, Some(&s.to_string()));
        died.push((name, s));
    }
    Ok(())
}

/// A shell's exit status for a process a signal ended: this plus the signal's number.
const SIGNAL_STATUS_BASE: i32 = 128;
/// Poll step while binderd ends: short against the grace.
const STOP_POLL: Duration = Duration::from_millis(10);

/// Stops one system process at the run's end ([`Process::terminate`]: SIGTERM, `grace`, SIGKILL, through its pidfd: a sandbox that ended is never signalled) and reaps its bwrap; one line says which
/// ended it.
fn stop_system(name: &str, p: &mut Process, grace: Duration) {
    match p.terminate(grace) {
        Ok(true) => log(format_args!("stopped {name}")),
        Ok(false) => log(format_args!(
            "{name}: no exit {grace:?} after SIGTERM: killed"
        )),
        Err(e) => {
            log(format_args!("{name}: SIGTERM: {e}: killed"));
            let _ = p.kill();
        }
    }
    let _ = p.child.wait();
}

/// The run's ranked API outcomes (atl_diag::summary over every process's log, first-seen as offset from `since_ms`): `atld: summary <n> keys`, then one `atld: summary <row>` line per key, most hits first.
/// Then one block per atl_diag::FAMILIES member, `atld: <family>-summary <n> keys` and its first `rows_of(family)` rows: a family's keys never enter the API block.
/// Read after every system process stopped, so their counts are exact; app processes were SIGKILLed, so theirs read `x>=n`.
fn print_summary(log_dir: &Path, since_ms: u64, rows_of: impl Fn(&str) -> usize) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(log_dir)
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    paths.retain(|p| p.extension().is_some_and(|x| x == "log"));
    paths.sort();
    // head + tail ring: a process's exit totals sit in its newest tail segment
    let logs: Vec<String> = paths.iter().map(|p| atl_diag::logfiles::read(p)).collect();
    let rows = atl_diag::summary::summary(&logs, since_ms, None);
    log(format_args!("summary {} keys", rows.len()));
    for row in rows {
        log(format_args!("summary {row}"));
    }
    for fam in atl_diag::FAMILIES {
        let rows = atl_diag::summary::summary(&logs, since_ms, Some(fam));
        log(format_args!("{fam}-summary {} keys", rows.len()));
        for row in rows.iter().take(rows_of(fam)) {
            log(format_args!("{fam}-summary {row}"));
        }
    }
}

/// Wall-clock ms since the epoch, the clock of the `diag:` lines' stamps (atl_diag::wall_stamp).
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn main() -> ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == PROBE_WEB_ENGINE)
    {
        return probe_web_engine_main();
    }
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

    /// A stand-in daemon: `sh -c script`, its own pid as `host_pid` (no sandbox: the pidfd logic is the same).
    fn daemon(script: &str) -> Process {
        #[expect(
            clippy::disallowed_types,
            reason = "a test's stand-in daemon, not a sandbox"
        )]
        let child = std::process::Command::new("sh")
            .args(["-c", script])
            .spawn()
            .expect("sh");
        let host_pid = i32::try_from(child.id()).expect("pid");
        let pid = rustix::process::Pid::from_raw(host_pid).expect("pid");
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .expect("pidfd_open");
        Process {
            child,
            host_pid,
            pidfd,
        }
    }

    /// A pinned helper outlives the replacement of its source the way cargo's uplift and a package upgrade replace it (a new file renamed over the old name): the copy keeps the bytes
    /// and mode it was pinned with and is a different file from the new source, so nothing done to the source's name reaches what the broker binds.
    #[test]
    fn pinned_helper_survives_source_replacement() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("atld-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let src = dir.join("atl-sandbox-stub");
        std::fs::write(&src, b"v1").expect("write");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let pinned = pin_helper(&dir.join("helpers"), &src).expect("pin");
        assert_eq!(pinned, dir.join("helpers/atl-sandbox-stub"));
        let next = dir.join("next");
        std::fs::write(&next, b"v2").expect("write");
        std::fs::rename(&next, &src).expect("rename over");
        assert_eq!(std::fs::read(&pinned).expect("read"), b"v1");
        let meta = std::fs::metadata(&pinned).expect("stat");
        // 0o755: the mode the source had when pinned
        assert_eq!(meta.mode() & 0o777, 0o755);
        assert_ne!(meta.ino(), std::fs::metadata(&src).expect("stat").ino());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    /// R7-8: a daemon's logdw records go through the app log's rate bound. Burst 3 per 300 ms window: of 10 records sent at once the log keeps the first 3 (the rule: `burst` lines per window,
    /// journald's RateLimitBurst), and the first record of the next window brings the window's note counting the other 7.
    #[test]
    fn logdw_log_is_rate_bounded() {
        let dir = std::env::temp_dir().join(format!("atld-logdw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let (sock, log_path) = (dir.join("ks"), dir.join("keystore2.log"));
        let window = Duration::from_millis(300);
        let limits = applog::Limits {
            rate: Some((window, 3)),
            size: None,
        };
        let mut mounts = Vec::new();
        logdw_to_log(&mut mounts, &sock, "keystore2", &log_path, limits).expect("logdw");
        assert_eq!(mounts.len(), 1);
        // a datagram as liblog's logd_writer.cpp builds it: log id, tid u16, sec u32, nsec u32, then priority (6 = ANDROID_LOG_ERROR) + tag NUL + message NUL
        let entry = |msg: &str| {
            let mut d = vec![0u8];
            d.extend_from_slice(&7u16.to_le_bytes());
            d.extend_from_slice(&1_762_000_000u32.to_le_bytes());
            d.extend_from_slice(&0u32.to_le_bytes());
            d.push(6);
            d.extend_from_slice(b"keystore2\0");
            d.extend_from_slice(msg.as_bytes());
            d.push(0);
            d
        };
        let sender = std::os::unix::net::UnixDatagram::unbound().expect("socket");
        let records = |text: &str| {
            text.lines()
                .filter(|l| l.contains("liblog/keystore2: e"))
                .count()
        };
        let read_until = |want: usize| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let text = std::fs::read_to_string(&log_path).unwrap_or_default();
                if records(&text) >= want || Instant::now() > deadline {
                    return text;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        for n in 0..10 {
            sender
                .send_to(&entry(&format!("e{n}")), &sock)
                .expect("send");
        }
        let text = read_until(3);
        std::thread::sleep(window * 2);
        sender.send_to(&entry("e10"), &sock).expect("send");
        let after = read_until(4);
        assert_eq!(records(&text), 3, "{text}");
        assert!(after.contains("atld-log: suppressed 7 lines"), "{after}");
        assert!(after.contains("liblog/keystore2: e10"), "{after}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R7-7: a daemon that exits after the run's last poll gets its verdict from the end-of-run pass, once, with its status (3: the `exit 3` it ran); a live one gets none; the end's stop of the
    /// dead one signals nothing (its pidfd says it ended: no signal can reach a process reusing its pid).
    #[test]
    fn reap_died_after_last_poll() {
        let mut dead = daemon("exit 3");
        let mut alive = daemon("sleep 30");
        assert!(dead.ended(Some(Duration::from_secs(10))).expect("poll"));
        let mut died = Vec::new();
        for _ in 0..2 {
            reap_died(
                vec![("keystore2", &mut dead), ("keymint", &mut alive)],
                &mut died,
            )
            .expect("reap");
        }
        assert_eq!(died.len(), 1);
        assert_eq!(died[0].0, "keystore2");
        assert_eq!(died[0].1.code(), Some(3));
        assert!(dead.terminate(Duration::ZERO).expect("terminate"));
        alive.kill().expect("kill");
        assert!(alive.child.wait().expect("wait").code().is_none());
    }

    /// Fragments go to the two partitions servicemanager reads; anything else is refused.
    #[test]
    fn vintf_fragment_arg() {
        assert_eq!(
            vintf_fragment("vendor=/t/k.xml"),
            Ok(("vendor".to_owned(), PathBuf::from("/t/k.xml")))
        );
        for bad in ["odm=/t/k.xml", "system=", "/t/k.xml"] {
            assert!(vintf_fragment(bad).is_err(), "{bad}");
        }
    }

    /// The device's extension pack and GLES version are the host's (no ceiling, gl-honesty H3); the version is encoded major << 16 | minor (GLES_VERSION).
    #[test]
    fn device_gl_is_the_hosts() {
        // ES 3.2 with the pack (sifive): both kept
        assert_eq!(
            device_gl("OpenGL ES 3.2 Mesa 22.3.5", true),
            (
                true,
                Some((
                    "ro.opengles.version".to_owned(),
                    ((3 << 16) | 2).to_string()
                ))
            )
        );
        // ES 3.1 without the pack (WSL D3D12): unchanged
        assert_eq!(
            device_gl("OpenGL ES 3.1 Mesa 25.0.7-2", false),
            (
                false,
                Some((
                    "ro.opengles.version".to_owned(),
                    ((3 << 16) | 1).to_string()
                ))
            )
        );
        // ES 2.0: unchanged
        assert_eq!(
            device_gl("OpenGL ES 2.0", false),
            (
                false,
                Some(("ro.opengles.version".to_owned(), (2 << 16).to_string()))
            )
        );
        // not a GLES string: unset
        assert_eq!(device_gl("4.6 (Core Profile)", true), (true, None));
    }

    /// KernelLogLine's shape (`<6>` = INFO, libbase logging.cpp) becomes a stamped logcat line atl-diag reads; other lines pass unchanged.
    #[test]
    fn kernel_lines_become_logcat_lines() {
        let l = kernel_line("<6>servicemanager: Caller(pid=9) Since 'x' could not be found");
        assert!(
            l.ends_with(" I kmsg/servicemanager: Caller(pid=9) Since 'x' could not be found\n"),
            "{l}"
        );
        let stamp = l.split(' ').next().unwrap();
        assert!(atl_diag::summary::parse_stamp(stamp).is_some(), "{l}");
        assert!(kernel_line("<4>t: w").contains(" W kmsg/t: w"));
        assert_eq!(
            kernel_line("sandbox-stub: landlock abi=3"),
            "sandbox-stub: landlock abi=3\n"
        );
    }

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

    /// A headless run on a target without a compositor (an ssh host with no session) still starts: only a UI run needs the socket. With one, both kinds show it at its own path, read-only.
    #[test]
    fn only_a_ui_run_needs_the_compositor_socket() {
        let socket = PathBuf::from("/run/user/1/wayland-0");
        for ui in [false, true] {
            let m = probe_socket(Some(socket.clone()), ui)
                .expect("socket")
                .expect("mount");
            assert!(m.host == socket && m.at == socket && !m.writable);
        }
        assert!(probe_socket(None, true).is_err());
        assert_eq!(probe_socket(None, false).expect("headless"), None);
    }

    #[test]
    fn gralloc_node_is_the_ready_lines_parenthesis() {
        // the allocator's stderr: its counters line, then the ready line (atl-gralloc-allocator main)
        let log = "gralloc: counters allocate=0\ngralloc: ready (/dev/dri/renderD128)\n";
        assert_eq!(gralloc_node(log).as_deref(), Some("/dev/dri/renderD128"));
        assert_eq!(gralloc_node("gralloc: ready\n"), None);
    }

    /// Only an ISA whose engine `.so` is placed is probed: the host ISA (identity, no engine) and an ISA with no engine file are not; an ABI pair sharing one ISA probes it once. Files as `--file` parses them.
    #[test]
    fn cpuinfo_probe_runs_for_staged_engines_only() {
        let abis = ["x86_64=x86_64", "arm64-v8a=arm64", "armeabi-v7a=arm"].map(String::from);
        let engine = |isa: &str| {
            partition_file(&format!(
                "/out/libatl_engine.so={}",
                atl_android_paths::engine_library(isa)
            ))
            .expect("engine --file")
        };
        let other = partition_file("/out/x=/vendor/lib64/egl/x.so").expect("other --file");
        assert_eq!(
            staged_engines(&abis, &[other.clone(), engine("arm64")]),
            ["arm64"]
        );
        assert!(staged_engines(&abis, &[other]).is_empty());
        let twice = ["arm64-v8a=arm64", "arm64-v8a-hwasan=arm64"].map(String::from);
        assert_eq!(staged_engines(&twice, &[engine("arm64")]), ["arm64"]);
    }

    /// A probed ISA's app process gets its cached file read-only at /proc/cpuinfo; the host ISA and a process of no package get none.
    #[test]
    fn cpuinfo_mount_only_for_a_probed_isa() {
        let cpuinfo = BTreeMap::from([("arm64".to_owned(), PathBuf::from("/r/cpuinfo.arm64"))]);
        assert_eq!(
            cpuinfo_mount(&cpuinfo, Some("arm64")),
            Some(Mount {
                host: "/r/cpuinfo.arm64".into(),
                at: PROC_CPUINFO.into(),
                writable: false,
            })
        );
        assert_eq!(cpuinfo_mount(&cpuinfo, Some("x86_64")), None);
        assert_eq!(cpuinfo_mount(&cpuinfo, None), None);
    }
}
