// SPDX-License-Identifier: MIT
//! atld, M2 wiring: starts the system the Android way, each process through the broker, then serves the services' process control until its main process ends.
//! 1. the runtime root `--root` (made fresh): `properties` (the default property area, atl-property-service; `properties.d/` one area per app process: the defaults plus the properties only that process has, [`Host::process_properties`]), `system/` (build.prop, `framework/` = the boot jars and shared libraries),
//!    `data/` (`app/<package>/base.apk`, `user/0/<package>/`, `dalvik-cache/`), `packages.json` (the installed packages), `log/<process>.log` (each process's stdout + stderr; a System.out line is a liblog record there).
//! 2. the install: each `--package` (an APK and the manifest xtask read from it) gets the next app uid, its data directories and `/data/app/<package>/base.apk`; the records go to `packages.json`.
//! 3. atl-binderd, its admin connection on stdin; the broker holds the other end.
//! 4. AOSP servicemanager as uid 1000 (AID_SYSTEM), the one process that may become context manager.
//! 5. atl-system-services as uid 1000 with the packages and, as its stdin, the process-control connection (atl_broker::control): it starts app processes (ActivityManager's ProcessList) and kills them, here, through
//!    the broker's sandbox. atld waits for its `services: ready`.
//! 6. `--run-class`: a command as uid `--run-uid` (the shell), `am instrument` for example. Without it the main process is the first one the services start.
//!
//! Exit status: the main process's (bwrap passes it on). Plan: binderd, servicemanager and the services move into this process later; here each is a process, as `xtask test binder` runs them.
#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_android_paths::{DATA, SDCARD, SHARED_STORAGE, SYSTEM, SYSTEM_FRAMEWORK};
use atl_broker::control::{self, Request, Response, StartRequest};
use atl_broker::{Broker, Link, Mount, Process, Spawn, kill_sandbox};
use atl_gralloc::vintf;
use atl_property_service::Property;
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
    /// A file of /system or /vendor besides the framework's (repeat): `<host path>=<absolute Android path>`, linked there (the system fonts: /system/etc/font_fallback.xml, /system/fonts/<name>; the GL driver: /vendor/lib64/egl/<name>).
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
}

/// Android uids (system/core/libcutils/include/private/android_filesystem_config.h): AID_SYSTEM, AID_APP_START.
const AID_SYSTEM: u32 = 1000;
const AID_APP_START: u32 = 10_000;
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
fn host_display() -> DisplayModel {
    match atl_platform_host::first_monitor() {
        Ok(Some(m)) => {
            let model = DisplayModel::from_monitor(
                m.size.width,
                m.size.height,
                m.refresh_hz,
                m.scale.get(),
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
fn property_area(a: &Args, r: &Root) -> Result<Vec<Property>> {
    let mut text = std::fs::read_to_string(&a.build_prop)
        .with_context(|| format!("reading {}", a.build_prop.display()))?;
    let vendor = Path::new("vendor/build.prop");
    if let Some((host, _)) = a.files.iter().find(|(_, at)| at == vendor) {
        text.push('\n');
        text += &std::fs::read_to_string(host)
            .with_context(|| format!("reading {}", host.display()))?;
    }
    let props = atl_property_service::defaults(&text)?;
    std::fs::write(r.properties(), atl_property_service::area(&props)?)?;
    std::fs::create_dir(r.root.join("properties.d"))?;
    Ok(props)
}

/// atl-binderd with its admin connection on stdin, its stderr (counters) in its log; the broker over the other end.
fn start_binderd(a: &Args, r: &Root) -> Result<(Child, Broker)> {
    let (admin, theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let binderd = Command::new(&a.binderd)
        .stdin(Stdio::from(theirs))
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(r.log("binderd"))?)
        .spawn()
        .with_context(|| format!("starting {}", a.binderd.display()))?;
    let broker = if a.no_app_seccomp {
        Broker::new(admin).without_app_seccomp()
    } else {
        Broker::new(admin)
    };
    Ok((binderd, broker))
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
    /// the default property set and the display model: what [`Host::process_properties`] builds an app process's area from
    defaults: Vec<Property>,
    display: DisplayModel,
    /// the headless pointer script; pushed to each app process as `ro.atl.headless.pointer`
    headless_pointer: Option<String>,
    /// the UI mode; pushed to each app process as `ro.atl.ui`
    ui: Option<String>,
    /// the processes the services started and may kill: the host pids binderd tags them with
    controlled: Mutex<BTreeSet<i32>>,
    /// set by the first process that claims to be the main one
    main_claimed: AtomicBool,
    /// the main process's end
    done: Sender<ExitStatus>,
    /// whether the first process the services start is the main one (no `--run-class`)
    services_start_main: bool,
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

    /// The property area of the app process `name` of `uid`: the defaults plus what only this process has. `ro.atl.surfaceflinger.suffix` makes libgui look up this process's own compositor
    /// (compositor_registry.rs); `ro.atl.display` is the display that compositor answers with, the model the services answer with too; `ro.atl.headless.pointer` is the input script, present only with `--headless-pointer`.
    fn process_properties(&self, uid: u32, name: &str) -> Result<PathBuf> {
        let mut props = self.defaults.clone();
        props.push((
            compositor_registry::SUFFIX_PROPERTY.to_owned(),
            compositor_registry::suffix(uid, name),
        ));
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
        let launcher_args: Vec<OsString> = [
            format!("--target-sdk={target_sdk}"),
            "--app".to_owned(),
            format!("-Xbootclasspath:{boot}"),
            format!("-Xbootclasspath-locations:{boot}"),
            "-Xnoimage-dex2oat".to_owned(),
            class.to_owned(),
        ]
        .into_iter()
        .chain(args.iter().cloned())
        .map(OsString::from)
        .collect();
        Ok(Spawn {
            uid,
            context_manager: false,
            mounts: vec![
                Mount {
                    host: self.root.system(),
                    at: SYSTEM.into(),
                    writable: false,
                },
                Mount {
                    host: self.root.data(),
                    at: DATA.into(),
                    writable: true,
                },
            ],
            links: vec![],
            devices: vec![],
            property_area: Some(self.process_properties(uid, name)?),
            program: self.launcher.clone(),
            args: launcher_args,
            env: self.art_env(),
            stdin: None,
            log: OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.log(name))?,
        })
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

    /// Starts `spawn`; a thread reaps it. When it is the main process its end is atld's (`done`).
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
                        log(format_args!("process {host_pid} ended: {status}"));
                        if main {
                            let _ = host.done.send(status);
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
        // the first process the services start is the main one when no command is
        let main = self.services_start_main && !self.main_claimed.swap(true, Ordering::SeqCst);
        let pid = self.start(spawn, main, true)?;
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
    let defaults = property_area(a, &r)?;
    let boot = system_layout(a, &r)?;
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
    let (mut binderd, broker) = start_binderd(a, &r)?;
    let lib = (OsString::from("LD_LIBRARY_PATH"), a.lib_path.clone());
    let system_spawn = |name: &str,
                        program: &Path,
                        args: Vec<OsString>,
                        cm: bool,
                        stdin: Option<std::os::fd::OwnedFd>|
     -> Result<Spawn> {
        Ok(Spawn {
            uid: AID_SYSTEM,
            context_manager: cm,
            // the VINTF manifests are servicemanager's alone to read; the others reach HALs through it
            mounts: if cm {
                [("system", r.system()), ("vendor", r.vendor())]
                    .into_iter()
                    .map(|(at, host)| Mount {
                        host,
                        at: PathBuf::from("/").join(at),
                        writable: false,
                    })
                    .collect()
            } else {
                vec![]
            },
            links: vec![],
            devices: vec![],
            property_area: Some(r.properties()),
            program: program.to_owned(),
            args,
            env: vec![lib.clone()],
            stdin,
            log: std::fs::File::create(r.log(name))?,
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
    let (done_tx, done_rx): (Sender<ExitStatus>, Receiver<ExitStatus>) = channel();
    // the services and every app process answer with this one display
    let display = a.display.unwrap_or_else(host_display);
    let host = Arc::new(Host {
        broker,
        launcher: a.launcher.clone(),
        lib: lib.clone(),
        log_tags: a.log_tags.clone(),
        art_root: a.art_root.clone(),
        boot,
        defaults,
        display,
        headless_pointer: a.headless_pointer.clone(),
        ui: a.ui.clone(),
        controlled: Mutex::new(BTreeSet::new()),
        main_claimed: AtomicBool::new(false),
        done: done_tx,
        services_start_main: a.run_class.is_none(),
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
    // the key maps are /system files (`--file`, usr/keychars); the services read them where they lie in the root
    services_args.extend([
        "--keychars".into(),
        r.system().join("usr/keychars").into_os_string(),
    ]);
    let mut services = host
        .broker
        .spawn(system_spawn(
            "services",
            &a.services,
            services_args,
            false,
            Some(control_theirs),
        )?)
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
        let pid = host.start(
            spawn,
            !host.main_claimed.swap(true, Ordering::SeqCst),
            false,
        )?;
        log(format_args!("{name}: uid {uid}, host pid {pid}: {class}"));
    }
    let status = done_rx.recv().context("no main process ended")?;
    log(format_args!("main process ended: {status}"));
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
    let _ = binderd.kill();
    let _ = binderd.wait();
    Ok(match status.code() {
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
