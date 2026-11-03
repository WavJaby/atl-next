// SPDX-License-Identifier: MIT
//! `xtask test app`: the M2 gate. The test APK (xtask/testdata/app: a manifest and one Activity whose onCreate logs [`MARKER`]) is started the Android way by atld: atl-binderd,
//! AOSP servicemanager, the M2 services (atl-system-services), then the app through the broker's sandbox; ActivityThread attaches, is bound, launches the activity.
//! The APK is built here as an app is built, against the SDK's android.jar of the platform's API level (prebuilts/sdk/<ro.build.version.sdk>/public): aapt2 link (manifest only),
//! javac + d8 (the Java pipeline's), classes.dex added uncompressed. The package record atld registers is read back from the APK (`aapt2 dump badging`), not restated.
//! Pass: [`MARKER`] in the app's log within [`LIMIT`]. Fail: the app log's last lines, and the services' unimplemented-call counters (atl_services::counters lines).
//! Needs the converter's ART, servicemanager and atl-services in --out (`xtask native ... servicemanager libart libnativehelper libjavacore libopenjdk libicu_jni libandroid_runtime libandroid libsigchain atl-aidl-stubs-rust`).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use atl_layout::Layout;

use crate::host_art::{self, HostArt};
use crate::{farm, packages};

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default <workspace>/out/native/<host>, as `xtask native`).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, d8, aapt2 (default <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
}

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/app";
/// What Main.onCreate logs (tag `AtlTest`), as the launcher's logger writes it (`<tag>: <message>`).
const MARKER: &str = "AtlTest: onCreate reached";
/// The test app's uid: AID_APP_START + 123, any app id.
const APP_UID: u32 = 10_123;
/// How long a cold start may take: ART without a boot image interprets framework.jar's class initialisation (the parcel stage's runs take seconds); 180 s for a loaded machine.
const LIMIT: Duration = Duration::from_secs(180);
/// App log lines shown on failure.
const TAIL: usize = 40;
/// The services' counter lines (atl_services::counters).
const COUNTER: &str = "services: unimplemented ";
/// framework-res.apk in the Java pipeline's out: built as Soong builds it (manifest_fixer, link, merge, align; bp2ninja aapt.go), unsigned.
const FRAMEWORK_RES: &str = "gen/device/framework-res.aapt/framework-res.apk";

/// The package as `aapt2 dump badging` reports it.
#[derive(Debug, PartialEq)]
pub struct Badging {
    pub package: String,
    pub version_code: i64,
    pub min_sdk: i32,
    pub target_sdk: i32,
    pub activity: String,
}

/// `key='value'` in `line`.
fn quoted<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.split(&format!("{key}='")).nth(1)?;
    rest.split('\'').next()
}

/// aapt2's badging (aapt2 dump/DumpManifest.cpp): `package: name='..' versionCode='..'`, `minSdkVersion:'..'`, `targetSdkVersion:'..'`, `launchable-activity: name='..'`.
pub fn parse_badging(text: &str) -> Result<Badging> {
    let line = |prefix: &str| {
        text.lines()
            .find(|l| l.starts_with(prefix))
            .with_context(|| format!("no `{prefix}` line in aapt2's badging"))
    };
    let package = line("package:")?;
    let sdk = |prefix: &str| -> Result<i32> {
        let l = line(prefix)?;
        Ok(l[prefix.len()..].trim().trim_matches('\'').parse()?)
    };
    Ok(Badging {
        package: quoted(package, "name").context("package name")?.to_owned(),
        version_code: quoted(package, "versionCode")
            .context("versionCode")?
            .parse()?,
        min_sdk: sdk("minSdkVersion:")?,
        target_sdk: sdk("targetSdkVersion:")?,
        activity: quoted(line("launchable-activity:")?, "name")
            .context("launchable activity name")?
            .to_owned(),
    })
}

/// The platform's API level: `ro.build.version.sdk` of the converter's build.prop.
fn sdk_level(build_prop: &Path) -> Result<String> {
    let text = std::fs::read_to_string(build_prop)?;
    text.lines()
        .find_map(|l| l.strip_prefix("ro.build.version.sdk="))
        .map(|v| v.trim().to_owned())
        .with_context(|| format!("no ro.build.version.sdk in {}", build_prop.display()))
}

/// Builds the fixture APK into `work`; its path and badging.
fn build_apk(
    root: &Path,
    art: &HostArt,
    aapt2: &Path,
    android_jar: &Path,
    work: &Path,
) -> Result<(PathBuf, Badging)> {
    if work.exists() {
        std::fs::remove_dir_all(work)?;
    }
    let classes = work.join("classes");
    let dex = work.join("dex");
    for d in [&classes, &dex] {
        std::fs::create_dir_all(d)?;
    }
    let fixture = root.join(FIXTURE);
    let apk = work.join("base.apk");
    host_art::exec(
        Command::new(aapt2)
            .arg("link")
            .arg("--manifest")
            .arg(fixture.join("AndroidManifest.xml"))
            .arg("-I")
            .arg(android_jar)
            .arg("-o")
            .arg(&apk),
        "aapt2 link",
    )?;
    let mut sources = Vec::new();
    host_art::collect(&fixture.join("java"), "java", &mut sources)?;
    let jdk_bin = art.jdk.join("bin");
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
            .arg(android_jar)
            .arg("-d")
            .arg(&classes)
            .args(&sources),
        "javac",
    )?;
    let jar = work.join("classes.jar");
    host_art::exec(
        Command::new(jdk_bin.join("jar"))
            .arg("cf")
            .arg(&jar)
            .arg("-C")
            .arg(&classes)
            .arg("."),
        "jar",
    )?;
    let path = std::env::join_paths(std::iter::once(jdk_bin.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    host_art::exec(
        Command::new(&art.d8)
            .env("PATH", path)
            .arg("--lib")
            .arg(android_jar)
            .arg("--output")
            .arg(&dex)
            .arg(&jar),
        "d8",
    )?;
    // the dex stored, as the platform's packagers store it (ART maps an uncompressed dex in place)
    host_art::exec(
        Command::new(jdk_bin.join("jar"))
            .args(["--update", "--no-manifest", "--no-compress", "--file"])
            .arg(&apk)
            .arg("-C")
            .arg(&dex)
            .arg("classes.dex"),
        "jar --update",
    )?;
    let badging = host_art::output(
        Command::new(aapt2).args(["dump", "badging"]).arg(&apk),
        "aapt2 dump badging",
    )?;
    Ok((apk, parse_badging(&badging)?))
}

/// `package`'s `bin`, built plainly (release) into `out`'s cargo target dir; its path.
fn cargo_bin(root: &Path, out: &Path, package: &str) -> Result<PathBuf> {
    host_art::exec(
        crate::cargo_command()
            .current_dir(root)
            .args(["build", "--release", "--locked", "-p", package])
            .arg("--target-dir")
            .arg(out.join("cargo")),
        &format!("cargo build -p {package}"),
    )?;
    Ok(out.join("cargo/release").join(package))
}

fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let layout = Layout::of(root)?;
    let out = match &args.out {
        Some(dir) => std::path::absolute(dir)?,
        None => layout.workspace().join("out/native").join(&args.host),
    };
    let java_out = match &args.java_out {
        Some(dir) => std::path::absolute(dir)?,
        None => layout.workspace().join("out/java/build"),
    };
    let art = HostArt::new(
        root,
        &args.host,
        Some(&java_out),
        &out,
        "app-art",
        args.verbose,
    )?;
    let servicemanager = out.join("device/servicemanager/servicemanager");
    ensure!(
        servicemanager.is_file(),
        "{} missing: build servicemanager",
        servicemanager.display()
    );
    let services = packages::bin(root, &out, "atl-services", "atl-system-services")?;
    let binderd = cargo_bin(root, &out, "atl-binderd")?;
    let atld = cargo_bin(root, &out, "atld")?;
    let aapt2 = java_out.join("host/aapt2/aapt2");
    let android_jar = farm::shared_tree(&layout)
        .join("prebuilts/sdk")
        .join(sdk_level(&art.build_prop)?)
        .join("public/android.jar");
    let framework_res = java_out.join(FRAMEWORK_RES);
    for p in [&aapt2, &android_jar, &framework_res] {
        ensure!(p.is_file(), "missing input {}", p.display());
    }
    let work = out.join("app");
    let (apk, b) = build_apk(root, &art, &aapt2, &android_jar, &work.join("apk"))?;
    println!(
        "app: built {} ({} versionCode {}, sdk {}..{}, launcher activity {})",
        apk.display(),
        b.package,
        b.version_code,
        b.min_sdk,
        b.target_sdk,
        b.activity
    );

    let run_root = work.join("root");
    let atld_log = work.join("atld.log");
    let mut cmd = Command::new(&atld);
    cmd.arg("--root")
        .arg(&run_root)
        .arg("--binderd")
        .arg(&binderd)
        .arg("--servicemanager")
        .arg(&servicemanager)
        .arg("--services")
        .arg(&services)
        .arg("--launcher")
        .arg(&art.launcher)
        .arg("--lib-path")
        .arg(&art.lib_path)
        .arg("--art-root")
        .arg(&art.art)
        .arg("--build-prop")
        .arg(&art.build_prop)
        .arg("--framework-res")
        .arg(&framework_res)
        .arg("--apk")
        .arg(&apk)
        .args(["--package", &b.package])
        .args(["--version-code", &b.version_code.to_string()])
        .args(["--min-sdk", &b.min_sdk.to_string()])
        .args(["--target-sdk", &b.target_sdk.to_string()])
        .args(["--activity", &b.activity])
        .args(["--uid", &APP_UID.to_string()])
        .args(["--log-tags", &args.log_tags]);
    for jar in &art.boot {
        cmd.arg("--boot-jar").arg(jar);
    }
    let log_file = std::fs::File::create(&atld_log)?;
    cmd.stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    let mut atld_child = cmd.spawn().context("starting atld")?;
    let app_log = run_root.join("log/app.log");
    let start = Instant::now();
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let (passed, ended) = loop {
        if read(&app_log).lines().any(|l| l.contains(MARKER)) {
            break (true, None);
        }
        if let Some(s) = atld_child.try_wait()? {
            break (read(&app_log).lines().any(|l| l.contains(MARKER)), Some(s));
        }
        if start.elapsed() > LIMIT {
            break (false, None);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let elapsed = start.elapsed();
    if ended.is_none() {
        let _ = atld_child.kill();
        atld_child.wait()?;
    }
    let app = read(&app_log);
    let services_log = read(&run_root.join("log/services.log"));
    if args.verbose {
        println!("---- app log\n{app}");
    }
    for l in services_log.lines().filter(|l| l.starts_with(COUNTER)) {
        println!("app: counter {}", &l[COUNTER.len()..]);
    }
    if passed {
        println!(
            "app: pass ({} Activity.onCreate logged after {elapsed:.1?})",
            b.activity
        );
        return Ok(ExitCode::SUCCESS);
    }
    let why = match ended {
        Some(s) => format!("atld exited {s} before the marker"),
        None if elapsed > LIMIT => format!("no marker after {LIMIT:?}"),
        None => "no marker".to_owned(),
    };
    println!("app: FAIL {why}");
    println!("---- atld log\n{}", tail(&read(&atld_log), TAIL));
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    Ok(ExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// aapt2 2.20's badging of the fixture, as measured 2026-11-03.
    const BADGING: &str = "package: name='com.example.atltest' versionCode='1' versionName='1.0' platformBuildVersionName='16' platformBuildVersionCode='36' compileSdkVersion='36' compileSdkVersionCodename='16'\nminSdkVersion:'21'\ntargetSdkVersion:'36'\napplication: label='ATL test' icon=''\nlaunchable-activity: name='com.example.atltest.Main'  label='' icon=''\n";

    #[test]
    fn badging() {
        // the values the fixture's AndroidManifest.xml declares
        assert_eq!(
            parse_badging(BADGING).unwrap(),
            Badging {
                package: "com.example.atltest".into(),
                version_code: 1,
                min_sdk: 21,
                target_sdk: 36,
                activity: "com.example.atltest.Main".into(),
            }
        );
        assert!(parse_badging("package: name='x'\n").is_err());
    }
}
