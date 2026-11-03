// SPDX-License-Identifier: MIT
//! atld, M2 wiring: starts the system and one app the Android way, each process through the broker, and runs until the app process ends.
//! 1. the runtime root `--root` (made fresh): `properties` (the default property area, atl-property-service), `system/` (build.prop, `framework/` = the boot jars),
//!    `data/` (`app/<package>/base.apk`, `user/0/<package>/`, `dalvik-cache/`), `log/<process>.log` (each process's stdout + stderr).
//! 2. atl-binderd, its admin connection on stdin; the broker holds the other end.
//! 3. AOSP servicemanager as uid 1000 (AID_SYSTEM), the one process that may become context manager.
//! 4. atl-system-services as uid 1000, told the package and the start it is to expect; atld waits for its `services: ready`.
//! 5. the app: atl-app-launcher `--app` running android.app.ActivityThread with `seq=<startSeq>` as the package's uid, in the sandbox with `/system`, `/data` and the property area.
//!
//! Exit status: the app process's (bwrap passes it on). Plan: binderd, servicemanager and the services move into this process later; here each is a process, as `xtask test binder` runs them.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_broker::{Broker, Mount, Process, Spawn};
use clap::Parser;
use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};

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
    /// A file of /system besides the framework's (repeat): `<host path>=<path below /system>`, linked there (the system fonts: etc/font_fallback.xml, fonts/<name>).
    #[arg(long = "system-file", value_parser = system_file)]
    system_files: Vec<(PathBuf, PathBuf)>,
    /// The app's APK.
    #[arg(long)]
    apk: PathBuf,
    /// Its package name, version code, SDK levels and launcher activity, as its manifest declares them.
    #[arg(long)]
    package: String,
    #[arg(long)]
    version_code: i64,
    #[arg(long)]
    min_sdk: i32,
    #[arg(long)]
    target_sdk: i32,
    #[arg(long)]
    activity: String,
    /// The app's uid (an app uid: AID_APP_START 10000 and above).
    #[arg(long)]
    uid: u32,
    /// liblog's ANDROID_LOG_TAGS for the app.
    #[arg(long, default_value = "*:i")]
    log_tags: OsString,
}

/// Android uids (system/core/libcutils/include/private/android_filesystem_config.h): AID_SYSTEM, AID_APP_START.
const AID_SYSTEM: u32 = 1000;
const AID_APP_START: u32 = 10_000;
/// The startSeq of the one start (ProcessList.mProcStartSeqCounter's first value: it counts from 0 and is pre-incremented).
const START_SEQ: i64 = 1;
/// How long the services may take to register: servicemanager's start, then two addService round trips (well under a second measured by `xtask test binder`; libbinder waits 1 s
/// per retry while the context manager is missing); 20 s for a loaded machine.
const READY: Duration = Duration::from_secs(20);
/// What atl-system-services prints once registered.
const READY_LINE: &str = "services: ready";
/// ActivityThread (ActivityThread.PROC_START_SEQ_IDENT) and its class.
const ACTIVITY_THREAD: &str = "android.app.ActivityThread";
const SEQ: &str = "seq=";
/// framework-res's installed name in /system/framework (AssetManager.FRAMEWORK_APK_PATH_DEVICE).
const FRAMEWORK_RES: &str = "framework-res.apk";

fn log(msg: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "atld: {msg}");
}

/// The runtime root's layout.
struct Root {
    root: PathBuf,
}

impl Root {
    fn system(&self) -> PathBuf {
        self.root.join("system")
    }
    fn data(&self) -> PathBuf {
        self.root.join("data")
    }
    fn properties(&self) -> PathBuf {
        self.root.join("properties")
    }
    fn log(&self, name: &str) -> PathBuf {
        self.root.join("log").join(format!("{name}.log"))
    }
}

/// /system/framework/<name> of each boot jar (a link to it: the host's tree is visible in the sandbox), the boot classpath as the sandbox sees it.
fn system_layout(a: &Args, r: &Root) -> Result<String> {
    let framework = r.system().join("framework");
    std::fs::create_dir_all(&framework)?;
    std::fs::copy(&a.build_prop, r.system().join("build.prop"))?;
    std::os::unix::fs::symlink(
        std::path::absolute(&a.framework_res)?,
        framework.join(FRAMEWORK_RES),
    )?;
    let mut boot = Vec::new();
    for jar in &a.boot_jars {
        let name = jar
            .file_name()
            .with_context(|| format!("{}: no file name", jar.display()))?;
        let at = framework.join(name);
        ensure!(
            !at.exists(),
            "two boot jars named {}",
            name.to_string_lossy()
        );
        std::os::unix::fs::symlink(std::path::absolute(jar)?, &at)?;
        boot.push(format!("/system/framework/{}", name.to_string_lossy()));
    }
    for (host, at) in &a.system_files {
        let at = r.system().join(at);
        ensure!(!at.exists(), "two /system files at {}", at.display());
        std::fs::create_dir_all(at.parent().context("a path below /system")?)?;
        std::os::unix::fs::symlink(std::path::absolute(host)?, &at)?;
    }
    Ok(boot.join(":"))
}

/// `--system-file`'s `<host path>=<path below /system>`; the second must be relative and stay below /system.
fn system_file(s: &str) -> Result<(PathBuf, PathBuf), String> {
    let (host, at) = s
        .split_once('=')
        .ok_or_else(|| format!("{s}: not <host path>=<path below /system>"))?;
    let at = PathBuf::from(at);
    if at
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "{}: not a relative path below /system",
            at.display()
        ));
    }
    Ok((PathBuf::from(host), at))
}

/// The installed paths of the package, as the sandbox sees them.
struct DataPaths {
    /// /data/app/<package>/base.apk
    apk: String,
    /// the credential-protected data dir, /data/user/0/<package>
    data: String,
    /// the device-protected data dir, /data/user_de/0/<package>
    device_data: String,
}

/// /data/app/<package>/base.apk (PackageManager's install path for a base APK), the app's credential- and device-protected data dirs, ART's dalvik-cache.
fn data_layout(a: &Args, r: &Root) -> Result<DataPaths> {
    let app = format!("app/{}", a.package);
    let user = format!("user/0/{}", a.package);
    let user_de = format!("user_de/0/{}", a.package);
    for d in [
        app.as_str(),
        user.as_str(),
        user_de.as_str(),
        "dalvik-cache",
    ] {
        std::fs::create_dir_all(r.data().join(d))?;
    }
    std::fs::copy(&a.apk, r.data().join(&app).join("base.apk"))
        .with_context(|| format!("copying {}", a.apk.display()))?;
    Ok(DataPaths {
        apk: format!("/data/{app}/base.apk"),
        data: format!("/data/{user}"),
        device_data: format!("/data/{user_de}"),
    })
}

fn property_area(a: &Args, r: &Root) -> Result<()> {
    let text = std::fs::read_to_string(&a.build_prop)
        .with_context(|| format!("reading {}", a.build_prop.display()))?;
    let props = atl_property_service::defaults(&text)?;
    std::fs::write(r.properties(), atl_property_service::area(&props)?)?;
    Ok(())
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
    Ok((binderd, Broker::new(admin)))
}

fn run(a: &Args) -> Result<ExitCode> {
    ensure!(a.uid >= AID_APP_START, "--uid {}: not an app uid", a.uid);
    if a.root.exists() {
        std::fs::remove_dir_all(&a.root)?;
    }
    let r = Root {
        root: std::path::absolute(&a.root)?,
    };
    std::fs::create_dir_all(r.root.join("log"))?;
    property_area(a, &r)?;
    let boot = system_layout(a, &r)?;
    let paths = data_layout(a, &r)?;
    let (mut binderd, broker) = start_binderd(a, &r)?;
    let lib = (OsString::from("LD_LIBRARY_PATH"), a.lib_path.clone());
    let system_spawn =
        |name: &str, program: &Path, args: Vec<OsString>, cm: bool| -> Result<Process> {
            broker
                .spawn(Spawn {
                    uid: AID_SYSTEM,
                    context_manager: cm,
                    mounts: vec![],
                    property_area: Some(r.properties()),
                    program: program.to_owned(),
                    args,
                    env: vec![lib.clone()],
                    log: std::fs::File::create(r.log(name))?,
                })
                .with_context(|| format!("starting {name}"))
        };
    let mut sm = system_spawn("servicemanager", &a.servicemanager, vec![], true)?;
    log(format_args!("servicemanager: host pid {}", sm.host_pid));
    let services_args: Vec<OsString> = [
        ("--package", a.package.clone()),
        ("--uid", a.uid.to_string()),
        ("--apk", paths.apk),
        ("--data", paths.data),
        ("--device-data", paths.device_data),
        ("--activity", a.activity.clone()),
        ("--start-seq", START_SEQ.to_string()),
        ("--version-code", a.version_code.to_string()),
        ("--min-sdk", a.min_sdk.to_string()),
        ("--target-sdk", a.target_sdk.to_string()),
    ]
    .into_iter()
    .flat_map(|(k, v)| [OsString::from(k), OsString::from(v)])
    .collect();
    let mut services = system_spawn("services", &a.services, services_args, false)?;
    let start = Instant::now();
    while !std::fs::read_to_string(r.log("services"))?.contains(READY_LINE) {
        if let Some(s) = services.child.try_wait()? {
            bail!(
                "atl-system-services exited {s} (log {})",
                r.log("services").display()
            );
        }
        if start.elapsed() > READY {
            bail!("atl-system-services not ready after {READY:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    log(format_args!("services ready"));

    let art = &a.art_root;
    let env: Vec<(OsString, OsString)> = [
        ("ANDROID_ROOT", art.clone()),
        ("ANDROID_ART_ROOT", art.join("com.android.art")),
        ("ANDROID_I18N_ROOT", art.join("com.android.i18n")),
        ("ANDROID_TZDATA_ROOT", art.join("com.android.tzdata")),
        ("ANDROID_DATA", PathBuf::from("/data")),
        ("LD_BIND_NOW", PathBuf::from("1")),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), v.into_os_string()))
    .chain([lib.clone(), ("ANDROID_LOG_TAGS".into(), a.log_tags.clone())])
    .collect();
    let args: Vec<OsString> = [
        format!("--target-sdk={}", a.target_sdk),
        "--app".to_owned(),
        format!("-Xbootclasspath:{boot}"),
        format!("-Xbootclasspath-locations:{boot}"),
        "-Xnoimage-dex2oat".to_owned(),
        ACTIVITY_THREAD.to_owned(),
        format!("{SEQ}{START_SEQ}"),
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let mut app = broker
        .spawn(Spawn {
            uid: a.uid,
            context_manager: false,
            mounts: vec![
                Mount {
                    host: r.system(),
                    at: "/system".into(),
                    writable: false,
                },
                Mount {
                    host: r.data(),
                    at: "/data".into(),
                    writable: true,
                },
            ],
            property_area: Some(r.properties()),
            program: a.launcher.clone(),
            args,
            env,
            log: std::fs::File::create(r.log("app"))?,
        })
        .context("starting the app")?;
    log(format_args!(
        "app {}: uid {}, host pid {}, startSeq {START_SEQ}",
        a.package, a.uid, app.host_pid
    ));
    let status = app.child.wait()?;
    log(format_args!("app ended: {status}"));
    for p in [&mut services, &mut sm] {
        let _ = p.child.kill();
        let _ = p.child.wait();
    }
    drop(broker);
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
