// SPDX-License-Identifier: MIT
//! What the runtime gates that start atld share (`xtask test app`, `xtask test instrument`): the converter's ART staged as an install lays it out (host_art.rs), servicemanager, atl-system-services, atl-binderd and atld,
//! the Java pipeline's boot jars, aapt2, d8 and the SDK's android.jar, the test fonts, an APK built the way an app is built, and atld's command line for it.
//! The APK is built against the SDK's android.jar of the platform's API level (prebuilts/sdk/<ro.build.version.sdk>/public): aapt2 link (manifest only), javac + d8 (the Java pipeline's), classes.dex added uncompressed.
//! Native inputs: gate_goals.rs `runtime`, plus `gralloc` for [`Runtime::with_gralloc`] / `with_gl_driver` and `nb_guest` for [`Runtime::with_native_bridge_guest`]; each gate's composition is its `GATES` row.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;
use atl_services_types::display_model::DisplayModel;
use atl_services_types::native_libs::SupportedAbi;
use atl_services_types::packages::Library;
use serde::{Deserialize, Serialize};

use crate::gate_goals::{
    ANGLE_FRONTS, EXTRACTOR_PLUGINS, GL_FRONTS, NB_EXE, NB_GUEST_LIBS, NB_LINKER, NDK_PROXIED,
};
use crate::host_art::{self, HostArt};
use crate::{manifest, packages, remote, slot, tree};

/// framework-res.apk in the Java pipeline's out: built as Soong builds it (manifest_fixer, link, merge, align; bp2ninja app.go), unsigned.
const FRAMEWORK_RES: &str = "device/framework-res/framework-res.apk";
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

/// The /system files of the media stack an app process reads: the extractor plugins (MediaExtractorFactory loads `lib64/extractors`) and AOSP's codec lists (`media_codecs.xml` is the software list; the Codec2 list is `media_codecs_c2.xml`, read from the parser's default search dirs, which include /system/etc).
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
    for (host, _) in &rows {
        ensure!(
            host.is_file(),
            "missing input {} (the media stack's /system files: xtask native builds the plugins)",
            host.display()
        );
    }
    Ok(rows)
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

/// The sensors gate's fake kernel views (see [`Runtime::with_fake_devices`]).
struct FakeDevices {
    sys_bus: PathBuf,
    udev_data: PathBuf,
    /// `(name under /dev, host node)`.
    nodes: Vec<(String, PathBuf)>,
}

/// The staged runtime and the tools around it.
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
    atld: PathBuf,
    /// atl-installer and the api-impl dex: atld's `--installer`, `--installer-class-path`
    installer: PathBuf,
    api_impl: PathBuf,
    framework_res: PathBuf,
    /// The pinned test fonts atld reads as the desktop's (test_fonts.rs): its FONTCONFIG_FILE and `--font-data`.
    fonts: crate::test_fonts::TestFonts,
    /// AOSP's keyboards dir: its [`tree::KEYBOARD_FILES`] are linked into /system (the input service loads the maps, the app's input thread the layout).
    keychars: PathBuf,
    /// The graphics allocator HAL and the directory of `mapper.atl.so`, when [`Runtime::with_gralloc`] asked for them.
    gralloc: Option<(PathBuf, PathBuf)>,
    /// atl-egl-probe, atld's GPU selector (`--gpu-probe`), with [`Runtime::with_gralloc`].
    gpu_probe: Option<PathBuf>,
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
    /// The headless pointer script (atld `--headless-pointer`), when [`Runtime::with_headless_pointer`] set it.
    headless_pointer: Option<String>,
    /// The display the run answers with (atld `--display`, `WxH@hz,dpi`).
    display: Option<String>,
    /// The app processes' UI mode (atld `--ui`), when [`Runtime::with_ui`] set it.
    ui: Option<String>,
    /// A composer rule a gate breaks on purpose (`ATL_UI_BREAK`, atl-composer start.rs), when [`Runtime::with_ui_break`] set it.
    ui_break: Option<&'static str>,
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
    /// Packages take their GL driver from the owner list and the ES1 rule unless this is cleared (`--no-gl-selection`, the GL selection gate's break).
    gl_selection: bool,
    /// Apps' AudioFlinger mixes silence when this is set (`--break-mix`, the audio gate's third break).
    audio_break_mix: bool,
    /// atld's `PIPEWIRE_REMOTE` (the desktop's server socket, which atld grants the apps) when set: a stale socket file makes an unreachable server (audio_app.rs `--unreachable`).
    pipewire_remote: Option<PathBuf>,
    /// The Wayland socket atld's children connect to (the window gate's proxy); the environment's when unset.
    wayland: Option<PathBuf>,
    /// Apps run under atld's app seccomp filter (atl-broker seccomp.rs) unless this is cleared (`--no-app-seccomp`, the deliberate-failure check).
    app_seccomp: bool,
    /// The build's `[test_only]` artifacts (licenses.toml): never bound into the runtime root.
    test_only: std::collections::BTreeSet<PathBuf>,
    /// Input method clients get a death link unless this is cleared (`--no-ime-death-link`, the ime gate's deliberate break).
    ime_death_link: bool,
    /// A start may reuse a singleTop / singleTask instance unless this is cleared (`--no-new-intent`, the start-finish stage's deliberate break).
    new_intent: bool,
    /// Starts owe their callers a result unless this is cleared (`--no-activity-result`, the app gate's deliberate break).
    activity_result: bool,
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
    /// Sensors gate kernel fakes: atld runs in a private user + mount namespace (`unshare -Urm`) that shows these as the fixed paths the product reads.
    fake_devices: Option<FakeDevices>,
    /// The gate this runtime was staged for: each run writes its record (remote.rs `record_path`) under `out`.
    gate: String,
    /// The AOSP tree (`src/aosp`): the record's root for the `src/aosp/` bundle names.
    tree: PathBuf,
    /// Where atld runs: this host, or a target running it from the bundle (`remote test`, remote.rs [`remote::Site`]).
    site: remote::Site,
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

/// One atld run.
pub struct Run {
    /// atld's runtime root (atld makes it again); its logs are `<root>/log/<process>.log`.
    pub root: PathBuf,
    /// Where the package files and atld's own log go (outside the root, which atld removes).
    pub scratch: PathBuf,
    /// Each package's files, in uid order.
    pub packages: Vec<Package>,
    pub first_uid: u32,
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

/// The program name ([`atl_bwrap::PROGRAM`]) every process atld starts runs as: the broker launches nothing else.
const BROKER_CHILD_COMM: &str = atl_bwrap::PROGRAM;

/// atld's live children that did not come from the broker, as `<pid> <comm>`: every child of `atld` whose comm is not bwrap's. Read from `/proc/*/stat` (ppid, comm): `/proc/<pid>/task/*/children` needs CONFIG_PROC_CHILDREN, which the WSL kernel lacks.
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
        if ppid == atld && comm != BROKER_CHILD_COMM {
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

/// `package`'s `bin`, built plainly (release) into `out`'s cargo target dir; its path.
fn cargo_bin(root: &Path, out: &Path, package: &str) -> Result<PathBuf> {
    crate::unit_use::exec(
        crate::cargo_command()
            .current_dir(root)
            .args(["build", "--release", "--locked", "-p", package])
            .arg("--target-dir")
            .arg(out.join("cargo")),
        &format!("cargo build -p {package}"),
    )?;
    Ok(out.join("cargo/release").join(package))
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
        let (d8, jdk) = host_art::java_tools(root, host, java_out)?;
        let t = ApkTools {
            aapt2: java_out.join("host/aapt2/aapt2"),
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
    /// the slot is refused up front unless built with its goals.
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
        let java_out = slot::out_dir(root, java_out, "--java-out", |workspace| {
            workspace.join("out/java/build")
        })?;
        let art = HostArt::new(root, host, Some(&java_out), &out, stage, verbose)?;
        let servicemanager = out.join("device/servicemanager/servicemanager");
        ensure!(
            servicemanager.is_file(),
            "{} missing: build servicemanager",
            servicemanager.display()
        );
        let services = packages::bin(root, &out, "atl-services", "atl-system-services")?;
        let am = packages::bin(root, &out, "atl-services", "atl-am")?;
        let screencap = packages::bin(root, &out, "atl-services", "atl-screencap")?;
        let binderd = cargo_bin(root, &out, "atl-binderd")?;
        let sandbox_stub = cargo_bin(root, &out, "atl-sandbox-stub")?;
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
        let platform_files = platform_rows(&out, &layout.src().join("aosp"))?;
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
        let arches = crate::arches::load(root)?;
        let abis = device_abis(&arches, host)?;
        let licenses = crate::licenses::load(root, &crate::sources::load(root)?.upstream_names())?;
        let test_only = crate::licenses::test_only_outputs(&licenses, &out)?;
        Ok(Runtime {
            gate: gate.to_owned(),
            tree: tree::shared_tree(&layout),
            test_only,
            native_bridge,
            platform_module,
            abis,
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
            atld,
            installer,
            api_impl,
            framework_res,
            fonts,
            keychars,
            gralloc: None,
            gpu_probe: None,
            gl_driver: Vec::new(),
            fixture_files: Vec::new(),
            native_bridge_guest: Vec::new(),
            platform_files,
            headless_pointer: None,
            display: None,
            ui: None,
            ui_break: None,
            desktop_env: Vec::new(),
            app_env: Vec::new(),
            audio_service: true,
            audio_break_cblk: false,
            media_player_service: true,
            gl_selection: true,
            audio_break_mix: false,
            pipewire_remote: None,
            wayland: None,
            app_seccomp: true,
            ime_death_link: true,
            new_intent: true,
            activity_result: true,
            back_finish: true,
            root_back_close: false,
            sensor_service: true,
            omitted_services: Vec::new(),
            tilt_keys: None,
            mouse_touch: false,
            fake_devices: None,
            site: remote::Site::Host,
        })
    }

    /// Runs atld on `site` (remote.rs): a target's copy of this runtime's bundle, instead of this host.
    pub fn with_site(mut self, site: remote::Site) -> Self {
        self.site = site;
        self
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

    /// Also builds and places the GL driver (`libGLES_atl.so` over the host's EGL, egl_driver.rs `placement`) where AOSP's libEGL loader finds it: /vendor/lib64/egl and `ro.hardware.egl` in /vendor/build.prop.
    /// Also the fronts at Loader.cpp's `SYSTEM_LIB_PATH` (`/system/lib64` in LP64): it dlopens them there as the "wrapper" libraries (`load_wrapper`) and aborts without them; the links name the files already loaded, so glibc returns the loaded copy (same inode).
    pub fn with_gl_driver(mut self, root: &Path) -> Result<Self> {
        let mut files = crate::egl_driver::placement(root, &self.out)?;
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
        self.gl_driver = files;
        Ok(self)
    }

    /// Restages the test fonts with `family` preferred for sans-serif (the font gate's deliberate break: the mono font).
    pub fn with_sans_serif(mut self, layout: &Layout, family: &str) -> Result<Self> {
        self.fonts = crate::test_fonts::stage(layout, &self.out.join("fonts"), family)?;
        Ok(self)
    }

    /// Also places the guest `xtask native --native-bridge <isa>` built where the NativeBridge core starts it (atl-native-bridge `Config::installed`): linker64 + app_process64 under `native_bridge_bin`, the guest libraries under `native_bridge_lib`.
    /// The host ISA's guest serves identity; a foreign ISA's also needs its engine ([`Runtime::with_qemu_engine`]). Each call adds one ISA's guest.
    /// A foreign ISA's files are read from its own graph dir (native::graph_dir); run-apk checks that graph first (build_state::foreign_out).
    pub fn with_native_bridge_guest(mut self, isa: &str) -> Result<Self> {
        let bin = atl_android_paths::native_bridge_bin(isa);
        let lib = atl_android_paths::native_bridge_lib(isa);
        let graph = crate::native::graph_dir(&self.out, &self.host, isa);
        let module = |m: &str, file: &str| crate::native::nb_module_dir(&graph, isa, m).join(file);
        let mut files = vec![
            (module(NB_LINKER, "linker64"), format!("{bin}/linker64")),
            (
                module(NB_EXE, "app_process64"),
                format!("{bin}/app_process64"),
            ),
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

    /// Also places the QEMU engine of foreign guest ISA `isa` (test images only, [`qemu_engine_files`]): `xtask qemu --pic` of its arch (built when stale), the engine `.so` linked from it and the boot executable, kept under `<qemu dir>/app-engine`.
    pub fn with_qemu_engine(mut self, root: &Path, isa: &str) -> Result<Self> {
        let b = crate::qemu::plan(root, isa, None, true)?;
        crate::qemu::execute(root, &b)?;
        let dir = b.dir.join("app-engine");
        let so = crate::qemu_spike::engine_so_file(root, &b, &dir)?;
        let boot = crate::qemu::boot_exe(root, &b, &dir)?;
        self.platform_files
            .extend(qemu_engine_files(isa, &so, &boot));
        Ok(self)
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

    /// The app processes' composer breaks `rule` (a deliberate-failure run): `ATL_UI_BREAK` in their environment.
    pub fn with_ui_break(mut self, rule: &'static str) -> Self {
        self.ui_break = Some(rule);
        self
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

    /// Packages install with the native GL driver: no owner row, no ES1 rule (app.rs `--gl-selection-break`, test only).
    pub fn without_gl_selection(mut self) -> Self {
        self.gl_selection = false;
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
            self.binderd.clone(),
            self.sandbox_stub.clone(),
            self.atld.clone(),
            self.framework_res.clone(),
            self.fonts.conf.clone(),
        ]);
        // a directory (atld's `--font-data`): the record takes its tree whole
        inputs.dirs.push(self.fonts.font_data.clone());
        inputs.files.extend(self.fonts.files.iter().cloned());
        inputs.files.extend(
            tree::KEYBOARD_FILES
                .iter()
                .map(|(f, _)| self.keychars.join(f)),
        );
        if let Some((allocator, mapper_dir)) = &self.gralloc {
            inputs.files.push(allocator.clone());
            inputs.files.extend(remote::lib_files(mapper_dir)?);
        }
        inputs.files.extend(self.gpu_probe.iter().cloned());
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
        Ok(inputs)
    }

    /// Records what `run` reads for `xtask remote push` (remote.rs `write_record`): the runtime's inputs, the run's packages and boot jars. Written before atld starts, so a run that fails still leaves its record.
    pub fn record_inputs(&self, run: &Run) -> Result<()> {
        let mut inputs = self.runtime_inputs()?;
        let files = run.packages.iter().flat_map(|p| &p.files);
        inputs.files.extend(files.map(|f| f.path.clone()));
        if let Some(shell) = &run.shell {
            inputs.files.extend(shell.boot_jars.iter().cloned());
        }
        remote::write_record(&self.out, &self.gate, &self.java_out, &self.tree, &inputs)
    }

    /// Starts atld for `run`, its stdout and stderr in `run.atld_log()`. The caller waits for it and ends it.
    /// On a remote [`remote::Site`] the same command runs on the target (remote.rs `RemoteRun::spawn`): the child is the transfer, and it ends with the run.
    pub fn start_atld(&self, run: &Run) -> Result<Child> {
        self.record_inputs(run)?;
        std::fs::create_dir_all(&run.scratch)?;
        // atld makes the root again itself, but only once it runs: a poll must not see the previous run's logs
        if run.root.exists() {
            std::fs::remove_dir_all(&run.root)
                .with_context(|| format!("removing {}", run.root.display()))?;
        }
        let (mut cmd, mut written) = self.atld_command(run)?;
        if let remote::Site::Remote(target) = &self.site {
            // a UI window needs the target's desktop session (its WAYLAND_DISPLAY, remote.rs `session`); this host's socket has no target
            ensure!(
                self.fake_devices.is_none()
                    && self.wayland.is_none()
                    && run.libraries.is_empty()
                    && (self.ui.is_none() || target.has_session()),
                "remote run {}: not remotable yet (sensor injection, a gate's own Wayland socket, shared libraries, and a UI without the target's desktop session need a target runner)",
                run.root.display()
            );
            // fontconfig's `<dir>` and `<cachedir>` name this host's paths: the run reads a copy that the script relocates
            let conf = run.scratch.join("fonts.conf");
            std::fs::copy(&self.fonts.conf, &conf)
                .with_context(|| format!("copying {}", self.fonts.conf.display()))?;
            cmd.env("FONTCONFIG_FILE", &conf);
            written.push(conf);
            let roots = remote::Roots::of(&self.out, &self.java_out, &self.tree);
            return target.spawn(&roots, &cmd, &run.root, &run.scratch, &written);
        }
        let log_file = std::fs::File::create(run.atld_log())?;
        cmd.stdin(Stdio::null())
            .stdout(log_file.try_clone()?)
            .stderr(log_file);
        cmd.spawn().context("starting atld")
    }

    /// How a run's child ended: an error naming the transcript's last line when a remote transfer failed by itself (an exit code). A transfer the caller stopped at its limit has no code: its logs are the judge's to read, as on this host.
    pub fn check_end(&self, end: ExitStatus, run: &Run) -> Result<()> {
        match &self.site {
            remote::Site::Remote(target) if end.code().is_some_and(|c| c != 0) => {
                Err(target.failed(&run.root, end))
            }
            _ => Ok(()),
        }
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
        staged.extend(self.fonts.files.iter().map(PathBuf::as_path));
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
        // a remote target has no libc++ of its own: the bundle's `lib/` (remote.rs `LLVM_LIB_DIR`) joins the runtime's library path there
        if matches!(self.site, remote::Site::Remote(_)) {
            lib_path.push(":");
            lib_path.push(remote::LLVM_LIB_DIR);
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
        if let Some(extra) = &self.tilt_keys {
            cmd.arg("--tilt-keys").args(extra);
        }
        cmd.args([
            "--mouse-touch",
            if self.mouse_touch { "true" } else { "false" },
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
        if let Some((allocator, _)) = &self.gralloc {
            cmd.arg("--gralloc").arg(allocator);
        }
        cmd.arg("--root")
            .arg(&run.root)
            .arg("--binderd")
            .arg(&self.binderd)
            .arg("--sandbox-stub")
            .arg(&self.sandbox_stub)
            .arg("--servicemanager")
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
            // atld asks fontconfig for the desktop's fonts: here the pinned set (test_fonts.rs)
            .env("FONTCONFIG_FILE", &self.fonts.conf)
            .args(tree::KEYBOARD_FILES.iter().flat_map(|(file, dir)| {
                [
                    "--file".to_owned(),
                    format!(
                        "{}=/system/{dir}/{file}",
                        self.keychars.join(file).display()
                    ),
                ]
            }))
            .args(["--first-uid", &run.first_uid.to_string()])
            .args(["--log-tags", &run.log_tags]);
        if let Some(script) = &self.headless_pointer {
            cmd.args(["--headless-pointer", script]);
        }
        if let Some(display) = &self.display {
            cmd.args(["--display", display]);
        }
        if let Some(mode) = &self.ui {
            // the compositor logs each frame's `present` line at debug (atl-composer ui_window.rs): the UI runs that read them enable it
            // GTK's dmabuf debug lines: the window gate's import verdict (gdk_dmabuf.rs); both read in the app processes, so they go as --app-env (atld forwards nothing else of its own)
            cmd.args(["--ui", mode])
                .arg("--app-env")
                .arg("RUST_LOG=info,atl_composer=debug")
                .arg("--app-env")
                .arg(format!("GDK_DEBUG={}", crate::gdk_dmabuf::GDK_DEBUG));
        }
        if let Some(rule) = self.ui_break {
            cmd.arg("--app-env").arg(format!("ATL_UI_BREAK={rule}"));
        }
        for var in &self.app_env {
            cmd.arg("--app-env").arg(var);
        }
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
        if !self.gl_selection {
            cmd.arg("--no-gl-selection");
        }
        if self.audio_break_mix {
            cmd.arg("--break-mix");
        }
        if let Some(socket) = &self.pipewire_remote {
            cmd.env("PIPEWIRE_REMOTE", socket);
        }
        if let Some(socket) = &self.wayland {
            cmd.env("WAYLAND_DISPLAY", socket);
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
        // the platform's permission definitions, read from framework-res's manifest as an app's are (aapt2): atld's install grants from them
        let platform = run.scratch.join("platform-permissions.json");
        std::fs::write(
            &platform,
            serde_json::to_vec_pretty(&manifest::read_permissions(
                &self.apk.aapt2,
                &self.framework_res,
            )?)?,
        )?;
        written.push(platform.clone());
        cmd.arg("--platform-permissions").arg(platform);
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

/// One service counter: its kind (`unimplemented`, `answered`, `refused`, `sent`), its name and its total.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counter {
    pub kind: String,
    pub name: String,
    pub count: u64,
}

/// One counter line of the services' log: the counter's running count after that call, and the wall-clock time (ms since the epoch) the call was counted at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at_ms: u64,
    pub counter: Counter,
}

/// Milliseconds per second: the unit of a stamp's `secs.mmm`, logcat's and the services' wall clock.
pub const MS_PER_S: u64 = 1_000;
/// Digits after the point of a stamp: milliseconds, as logcat prints them.
const STAMP_DIGITS: usize = 3;

/// A wall-clock stamp `secs.mmm` (logcat's first field, a counter line's `at=`) as milliseconds since the epoch.
pub fn parse_stamp(text: &str) -> Option<u64> {
    let (secs, frac) = text.split_once('.')?;
    if frac.len() != STAMP_DIGITS || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(secs.parse::<u64>().ok()? * MS_PER_S + frac.parse::<u64>().ok()?)
}

/// A counter line without its ` at=<stamp>`: the text the exact-match checks compare.
pub fn without_stamp(line: &str) -> &str {
    line.rsplit_once(" at=").map_or(line, |(head, _)| head)
}

/// Every counter line of the services' log, in log order: `services: unimplemented|answered|refused|sent <interface>.<method> <n> at=<stamp>`
/// (atl_services_types::counters), and `parcelables: refused <what> <n> at=<stamp>` (atl_parcelables `refuse`). A line without a parsable stamp is not one.
pub fn events(services_log: &str) -> Vec<Event> {
    const KINDS: [(&str, &str); 5] = [
        ("services: unimplemented ", "unimplemented"),
        ("services: answered ", "answered"),
        ("services: refused ", "refused"),
        ("services: sent ", "sent"),
        ("parcelables: refused ", "refused"),
    ];
    let mut out = Vec::new();
    for line in services_log.lines() {
        for (prefix, kind) in KINDS {
            let Some((body, stamp)) = line
                .strip_prefix(prefix)
                .and_then(|rest| rest.rsplit_once(" at="))
            else {
                continue;
            };
            let Some(at_ms) = parse_stamp(stamp) else {
                continue;
            };
            let Some((name, n)) = body.rsplit_once(' ') else {
                continue;
            };
            let Ok(count) = n.parse::<u64>() else {
                continue;
            };
            out.push(Event {
                at_ms,
                counter: Counter {
                    kind: kind.to_owned(),
                    name: name.to_owned(),
                    count,
                },
            });
        }
    }
    out
}

/// The last count of each (kind, name) among `events`: the totals. The services are killed at the end of a run and print no summary, so the last line per name is the total.
pub fn totals<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<Counter> {
    let mut last = std::collections::BTreeMap::new();
    for e in events {
        last.insert(
            (e.counter.kind.clone(), e.counter.name.clone()),
            e.counter.count,
        );
    }
    last.into_iter()
        .map(|((kind, name), count)| Counter { kind, name, count })
        .collect()
}

/// The services' counters, from their log: the [`totals`] of [`events`].
pub fn counters(services_log: &str) -> Vec<Counter> {
    totals(&events(services_log))
}

/// [`counters`] as `<kind> <name> x<total>` lines.
pub fn counter_lines(services_log: &str) -> Vec<String> {
    counters(services_log)
        .into_iter()
        .map(|c| format!("{} {} x{}", c.kind, c.name, c.count))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Every counter kind, the last count per name wins, a name with spaces keeps them, other lines are ignored, and a counter line without a stamp is not one.
    #[test]
    fn counter_lines_take_the_last_count_of_each_kind() {
        let log = "services: ready\n\
                   services: unimplemented IActivityManager.getProcessLimit 1 at=100.000\n\
                   parcelables: refused android.content.Intent.extras 1 at=100.001\n\
                   services: unimplemented IActivityManager.getProcessLimit 2 at=100.002\n\
                   parcelables: refused android.os.Bundle (value type) 1 at=100.003\n\
                   parcelables: refused android.content.Intent.extras 2 at=100.004\n\
                   services: Failure reporting\n\
                   services: sent IInputMethodManager.IInputMethod.bindInput 1 at=100.005\n\
                   services: answered IInputMethodManager.addClient 1 at=100.006\n\
                   services: answered IInputMethodManager.addClient 2 at=100.007\n\
                   services: unimplemented IPackageManager.getMoveStatus 9\n";
        assert_eq!(
            counter_lines(log),
            [
                "answered IInputMethodManager.addClient x2",
                "refused android.content.Intent.extras x2",
                "refused android.os.Bundle (value type) x1",
                "sent IInputMethodManager.IInputMethod.bindInput x1",
                "unimplemented IActivityManager.getProcessLimit x2",
            ]
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
