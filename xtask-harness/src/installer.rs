// SPDX-License-Identifier: MIT
//! `xtask test installer`: the installer process (atl-installer, plan item 4b slices M1-M3) runs AOSP's `ApkLiteParseUtils.parseClusterPackageLite`, the full `ParsingPackageUtils.parsePackage` (through the api-impl ParseCallback forwarder) and then AOSP's
//! native-code choice (`NativeLibraryHelper`, `PackageAbiHelperImpl.derivePackageAbi`) on host ART over a package dir laid out as PackageManager stages one (`base.apk`, `split_<name>.apk`).
//! Oracles, read here independently of the installer: `aapt2 dump badging` of each file (package, versionCode, `split`); the full parse's oracle (installer_parse.rs: badging + the manifest's xmltree, and manifest.rs whose differences are printed as findings);
//! the zip itself through `unzip` (the `lib/<abi>/` entries, their names and sha256); the device's ABI list and SDK from the property set the installer maps (`ro.product.cpu.abilist`, `ro.build.version.sdk`).
//!
//! Checks:
//! 1. `alto`: the altos-odyssey row (google-play: base + config.x86_64, both already in the test-APK cache and verified here by sha256) -> package, versionCode and split names equal aapt2's; the full parse equals its oracle; primary ABI = the first device ABI the row's zip carries (x86_64);
//!    the lib dir `<root>/<isa>` holds exactly the zip's `lib/x86_64/` files, same sha256.
//! 2. `single`: the fossify-notes row (one APK, downloaded on first use) -> as check 1, no split; its manifest says extractNativeLibs=false, so the primary ABI is the first device ABI its zip carries (x86_64) and nothing is copied (lib-dir none, the libraries stay in the APK).
//! 3. `rich`: the material-catalog row (services, receivers, providers, permissions) -> as check 2.
//! 4. `refuse-version`, `refuse-package`: fossify-notes' base + a split linked here by aapt2 whose versionCode (base + 1) or package (base + `.other`) differs -> AOSP's own check refuses (`Inconsistent version` / `Inconsistent package`, PackageManager INSTALL_PARSE_FAILED_BAD_MANIFEST -101), the installer exits non-zero.
//! 5. `refuse-duplicate-permission`: an app linked here declaring one permission twice with different protection levels -> the full parse refuses it (ParsingPackageUtils.java:1051-1055, INSTALL_PARSE_FAILED_MANIFEST_MALFORMED -108); the lite parse alone accepts it.
//! 6. `feature`: an app linked here requesting CAMERA only with `<required-feature android.hardware.camera>` -> on this device (no declared features, the installer gets no `--feature`) AOSP drops the request (ParsingPackageUtils.java:1464-1471): no uses-permission.
//!
//! The installer's output is frames of upstream parcels (atl-services-types installer_output); every check reads it decoded by atl-services (`installed::decode`, through atl-installer-dump).
//! 7. `alto-corrupt-length`, `alto-truncated-parcel`: check 1's output with its first frame length past the end, and with the PackageInfo parcel cut by 8 bytes -> the decode refuses each by name, decoding nothing.
//!
//! Measured: the installer's VM start, lite parse, full parse and natives times (its stderr `time-ms` line) and the process wall time of each run.
//! Breaks: `--break drop-split` (check 1's dir without its split: splits, primary ABI and libraries must turn red); `--break abi-arm64` (the device's ABI lists are arm64-v8a only: AOSP's no-match, `native error -113`, must turn check 1 red);
//! `--break feature` (check 6's installer is told the device has android.hardware.camera: the forwarder's hasFeature answers true, uses-permission must turn red).
//! Every installer run is spawned through the broker's installer profile (atl-broker `spawn_installer`, plan 4b M5): the runtime, framework-res, the api-impl dex and the package dir read-only at their own paths, one output dir read-write,
//! no network namespace, no binder, the measured installer seccomp filter, the installer's own uid. The verdict counts the seccomp denials dmesg recorded during the run (kill mode: a denial also ends the process with SIGSYS).
//! Breaks, each must turn the gate red and name the refusal: `read-outside` (alto's package dir is not in the view: ENOENT), `write-outside` (alto's output dir is inside the read-only package dir: EROFS),
//! `no-framework-res` (framework-res not in the view: AOSP's `Failed to create system AssetManager`), `net` (atl-sandbox-probe, not the installer, reaches for TCP in the profile: killed by SIGSYS at socket(AF_INET)).
//! Default: mutating (stages ART into `<out>/installer-art`, writes `<out>/installer`).

use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use atl_android_paths::{DATA, PROPERTY_AREA, PROPERTY_AREA_ENV, SYSTEM_FRAMEWORK};
use atl_broker::{Broker, INSTALLER_UID, Mount, to_log, tree_mounts};
use atl_property_service::{LinearTier, Property};

use crate::atld_run::ApkTools;
use crate::host_art::{ArtRun, HostArt};
use crate::installer_parse::{Full, compare, manifest_findings, oracle};
use crate::{binder, manifest, packages, slot, test_apks, verdict};

const CMD: &str = "installer";
/// The package that builds both loaders, and the installer's executable.
const PACKAGE: &str = "atl-app-launcher";
const INSTALLER: &str = "atl-installer";
/// The package and executable that decode the installer's output (atl-services installed.rs).
const DUMP_PACKAGE: &str = "atl-services";
const DUMP: &str = "atl-installer-dump";
/// The google-play row with a split (test-apks.toml).
const SPLIT_ROW: &str = "altos-odyssey";
/// A one-APK url row (test-apks.toml).
const SINGLE_ROW: &str = "fossify-notes";
/// A one-APK url row whose manifest declares services, receivers, providers and permissions (test-apks.toml).
const RICH_ROW: &str = "material-catalog";
/// PackageManager.INSTALL_PARSE_FAILED_MANIFEST_MALFORMED: ParsingPackageUtils' code for a malformed duplicate permission (:1051-1055).
const MANIFEST_MALFORMED: i32 = -108;
/// The device's SDK and its codename: Build.VERSION.SDK_INT / CODENAME read these; RESOURCES_SDK_INT adds one while the codename is not `REL` (Build.java).
const SDK: &str = "ro.build.version.sdk";
const CODENAME: &str = "ro.build.version.codename";
/// Check 6's feature and the permission that requires it.
const FEATURE: &str = "android.hardware.camera";
const FEATURE_PERMISSION: &str = "android.permission.CAMERA";
const BREAK_FEATURE: &str = "feature";
/// The M5 breaks (module doc): each must turn the gate red with the refusal named.
const BREAK_READ_OUTSIDE: &str = "read-outside";
const BREAK_WRITE_OUTSIDE: &str = "write-outside";
const BREAK_NO_FRAMEWORK: &str = "no-framework-res";
const BREAK_NET: &str = "net";
/// The package whose data dir the net break's probe names (atl-sandbox-probe's OWN_FILE: `DATA/user/0/<package>`, which the profile does not show).
const PROBE_DATA_PACKAGE: &str = "atl.sandbox.probe";
/// SIGSYS (31 on x86_64 and aarch64 Linux): the signal a seccomp KILL_PROCESS ends the process with.
const SIGSYS: i32 = 31;
/// How long one installer run may take: alto's measured process wall is under 2 s; a hang fails the gate, it does not stop it.
const RUN_LIMIT: Duration = Duration::from_secs(120);
/// The property area as the installer sees it: the broker maps the caller's file at bionic's default path, so the variable names that path.
const IN_VIEW_AREA: (&str, &str) = (PROPERTY_AREA_ENV, PROPERTY_AREA);
/// atl-sandbox-probe, built by cargo into `<out>/cargo` for the net break.
const PROBE_PACKAGE: &str = "atl-sandbox-probe";
/// Where test_apks keeps verified files, under the workspace: `cache/test-apks/<sha256>.apk` (xtask test_apks.rs `CACHE_DIR`, `cached_path`).
const CACHE: &str = "cache/test-apks";
/// PackageManager.INSTALL_PARSE_FAILED_BAD_MANIFEST (frameworks/base core/java/android/content/pm/PackageManager.java): the code ApkLiteParseUtils.parseClusterPackageLite gives both inconsistencies.
const BAD_MANIFEST: i32 = -101;
/// The device's ABI lists: `Build.SUPPORTED_ABIS` and its 32/64-bit variants read these properties.
const ABILIST: &str = "ro.product.cpu.abilist";
const ABILIST32: &str = "ro.product.cpu.abilist32";
const ABILIST64: &str = "ro.product.cpu.abilist64";
/// Break `abi-arm64`: the device's only ABI, so Alto's x86_64 libraries match nothing (AOSP: INSTALL_FAILED_NO_MATCHING_ABIS, -113).
const BREAK_ABI: &str = "abi-arm64";
const BROKEN_DEVICE_ABI: &str = "arm64-v8a";

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`).
    #[arg(long)]
    host: String,
    /// Java pipeline build directory (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// Native build directory (default: this checkout's slot; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Deliberate break that must turn the gate red with the refusal named (module doc): `drop-split`, `abi-arm64` (check 1: no config split in the dir; the device's ABI lists are arm64-v8a only), `feature` (check 6: the installer is told the device has android.hardware.camera),
    /// `read-outside`, `write-outside`, `no-framework-res` (check 1's view lacks its package dir / its output dir is read-only / framework-res), `net` (the probe in the profile reaches for TCP).
    #[arg(long, value_parser = ["drop-split", "abi-arm64", "feature", BREAK_READ_OUTSIDE, BREAK_WRITE_OUTSIDE, BREAK_NO_FRAMEWORK, BREAK_NET])]
    r#break: Option<String>,
    /// Print each installer command and its output.
    #[arg(long)]
    verbose: bool,
}

/// One file's identity as `aapt2 dump badging` prints it on its `package:` line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Badging {
    package: String,
    version_code: i64,
    split: Option<String>,
}

/// `aapt2 dump badging <apk>`'s first `package:` line: `name='..'`, `versionCode='..'`, and `split='..'` for a split.
fn badging(aapt2: &Path, apk: &Path) -> Result<Badging> {
    let out = crate::host_art::output(
        Command::new(aapt2).arg("dump").arg("badging").arg(apk),
        "aapt2 dump badging",
    )?;
    let line = out
        .lines()
        .find(|l| l.starts_with("package:"))
        .with_context(|| format!("no package line in aapt2's badging of {}", apk.display()))?;
    let attr = |key: &str| -> Option<String> {
        let start = line.find(&format!(" {key}='"))? + key.len() + 3;
        let len = line[start..].find('\'')?;
        Some(line[start..start + len].to_owned())
    };
    Ok(Badging {
        package: attr("name").context("badging: no name")?,
        version_code: attr("versionCode")
            .context("badging: no versionCode")?
            .parse()
            .context("badging: versionCode")?,
        split: attr("split"),
    })
}

/// What the installer returned, from its decoded output (atl-installer-dump: `package`, `version-code`, `split`*, `primary-abi`, `lib-dir`; the full parse's other lines are installer_parse.rs's) and its stderr
/// (`time-ms vm <n> lite <n> parse <n> natives <n>`).
#[derive(Debug, Default)]
struct Parsed {
    package: String,
    version_code: i64,
    splits: Vec<String>,
    primary_abi: Option<String>,
    lib_dir: Option<PathBuf>,
    vm_ms: u64,
    lite_ms: u64,
    parse_ms: u64,
    natives_ms: u64,
}

/// The installer's `none` for an absent value.
fn none_or(value: &str) -> Option<&str> {
    (value != "none").then_some(value)
}

fn parse_output(decoded: &str, stderr: &str) -> Result<Parsed> {
    let mut p = Parsed::default();
    let timing = stderr.lines().filter(|l| l.starts_with("time-ms "));
    for line in decoded.lines().chain(timing) {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "package" => p.package = value.to_owned(),
            "version-code" => p.version_code = value.parse().context("version-code")?,
            "split" => p.splits.push(value.to_owned()),
            "primary-abi" => p.primary_abi = none_or(value).map(str::to_owned),
            "lib-dir" => p.lib_dir = none_or(value).map(PathBuf::from),
            "time-ms" => {
                let f: Vec<&str> = value.split(' ').collect();
                ensure!(
                    f.len() == 8
                        && f[0] == "vm"
                        && f[2] == "lite"
                        && f[4] == "parse"
                        && f[6] == "natives",
                    "time-ms line {line:?}"
                );
                p.vm_ms = f[1].parse()?;
                p.lite_ms = f[3].parse()?;
                p.parse_ms = f[5].parse()?;
                p.natives_ms = f[7].parse()?;
            }
            _ => {}
        }
    }
    ensure!(!p.package.is_empty(), "no package line in {decoded:?}");
    Ok(p)
}

/// A package dir as PackageManager stages one: `base.apk` and `split_<name>.apk`, symlinks to `files` = (split name or None for the base, file).
fn stage_dir(dir: &Path, files: &[(Option<&str>, &Path)]) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    for (split, file) in files {
        let name = split.map_or_else(|| "base.apk".to_owned(), |s| format!("split_{s}.apk"));
        std::os::unix::fs::symlink(file, dir.join(name))
            .with_context(|| format!("linking {}", file.display()))?;
    }
    Ok(())
}

/// The verified cached file of `pin`: present (a google-play row is fetched by `xtask test-apks fetch`, never here) and of the pinned sha256.
/// Row `name`'s one file: the single-APK cases' rows are url rows.
fn one_file(root: &Path, name: &str) -> Result<PathBuf> {
    let (_, files) = test_apks::ensure(root, name)?;
    match <[_; 1]>::try_from(files) {
        Ok([(None, path)]) => Ok(path),
        _ => bail!("{name}: not a one-file row"),
    }
}

fn cached(root: &Path, pin: &test_apks::Pin) -> Result<PathBuf> {
    let path = atl_layout::Layout::of(root)?
        .workspace()
        .join(CACHE)
        .join(format!("{}.apk", pin.sha256));
    let bytes = std::fs::read(&path)
        .with_context(|| format!("{} ({}) is not cached", path.display(), pin.file))?;
    ensure!(
        test_apks::sha256_hex(&bytes) == pin.sha256,
        "{}: sha256 differs from the pin",
        path.display()
    );
    Ok(path)
}

/// A split APK of `package` / `version_code` named `split`, linked by aapt2 from a manifest only: the input of the refusal checks.
fn link_split(
    tools: &ApkTools,
    work: &Path,
    package: &str,
    version_code: i64,
    split: &str,
) -> Result<PathBuf> {
    link(
        tools,
        &work.join(format!("split-{package}-{version_code}")),
        &format!(
            "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" package=\"{package}\" android:versionCode=\"{version_code}\" split=\"{split}\">\n  <application android:hasCode=\"false\"/>\n</manifest>\n"
        ),
    )
}

/// A code-less app `org.atl.fixture.<name>` whose manifest holds `body` (elements under `<manifest>`), linked by aapt2 into `<work>/<name>`: the input of the full parse's own checks.
fn link_app(tools: &ApkTools, work: &Path, name: &str, body: &str) -> Result<PathBuf> {
    link(
        tools,
        &work.join(format!("fixture-{name}")),
        &format!(
            "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" package=\"org.atl.fixture.{name}\" android:versionCode=\"1\">\n  <uses-sdk android:minSdkVersion=\"24\" android:targetSdkVersion=\"34\"/>\n{body}  <application android:hasCode=\"false\"/>\n</manifest>\n"
        ),
    )
}

/// `manifest` linked by aapt2 against the SDK's android.jar into `<dir>/app.apk`.
fn link(tools: &ApkTools, dir: &Path, manifest_text: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let manifest = dir.join("AndroidManifest.xml");
    std::fs::write(&manifest, manifest_text)?;
    let apk = dir.join("app.apk");
    crate::host_art::exec(
        Command::new(&tools.aapt2)
            .arg("link")
            .arg("--manifest")
            .arg(&manifest)
            .arg("-I")
            .arg(&tools.android_jar)
            .arg("-o")
            .arg(&apk),
        "aapt2 link",
    )?;
    Ok(apk)
}

/// The device's property area file: atl-property-service's default set over the host's build.prop, its ABI lists replaced by `abi` when given, laid out by its area writer into `dir`.
/// Returns the set (the oracle reads the device's ABI list from it) and the file (the broker maps it at PROPERTY_AREA).
fn device_area(host: &HostArt, dir: &Path, abi: Option<&str>) -> Result<(Vec<Property>, PathBuf)> {
    let text = std::fs::read_to_string(&host.build_prop)
        .with_context(|| format!("reading {}", host.build_prop.display()))?;
    let mut props = atl_property_service::defaults(&text, LinearTier::probe())?;
    if let Some(abi) = abi {
        for (name, value) in &mut props {
            match name.as_str() {
                ABILIST | ABILIST64 => *value = abi.to_owned(),
                ABILIST32 => value.clear(),
                _ => {}
            }
        }
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join("properties");
    std::fs::write(&path, atl_property_service::area(&props)?)?;
    Ok((props, path))
}

/// The device's ABIs, preferred first: `ro.product.cpu.abilist` of the property set.
fn device_list(props: &[Property]) -> Vec<String> {
    props
        .iter()
        .find(|(name, _)| name.as_str() == ABILIST)
        .map_or_else(Vec::new, |(_, list)| {
            list.split(',')
                .filter(|a| !a.is_empty())
                .map(str::to_owned)
                .collect()
        })
}

/// The converter's framework-res below the native build dir: what Android installs at AssetManager.FRAMEWORK_APK_PATH.
const FRAMEWORK_RES: &str = "device/framework-res/framework-res.apk";

/// What an installer run's view shows of the caller's files (on top of the runtime, which always is shown). A break turns one off, for its check only.
#[derive(Clone, Copy)]
struct Show {
    /// the package dir: read-only, with the files its links name
    input: bool,
    /// framework-res.apk at `/system/framework/framework-res.apk`, the fixed path the full parse's system AssetManager opens (AssetManager.createSystemAssetsInZygoteLocked, via DefaultSplitAssetLoader.getBaseAssetManager)
    framework: bool,
    /// the output dir: read-write
    output: bool,
}

impl Show {
    const ALL: Show = Show {
        input: true,
        framework: true,
        output: true,
    };

    /// The refusal a break takes away, as the verdict names it (None: nothing is taken away).
    fn refusal(self) -> Option<&'static str> {
        if !self.input {
            Some("refused: the package dir is outside the installer's view (ENOENT)")
        } else if !self.framework {
            Some("refused: framework-res is outside the installer's view")
        } else if !self.output {
            Some("refused: the output dir is read-only in the view (EROFS)")
        } else {
            None
        }
    }
}

/// The signal that ended a sandboxed process: bwrap exits with 128 + the signal when its child is killed (the child's own signal is not its status).
fn killed_by(status: std::process::ExitStatus) -> Option<i32> {
    status.signal().or_else(|| {
        status
            .code()
            .filter(|c| (129..=192).contains(c))
            .map(|c| c - 128)
    })
}

/// The line of an installer's stderr that says what refused it, most specific first: AOSP's own failure text, an errno text, a described Java exception, then the installer's own error line
/// (its `atl-installer: ...` at the start of a line; ART's `I atl-installer:` info lines are not errors). liblog's trace-file noise and the flag storage the view does not have are not refusals.
fn refusal_line(stderr: &str) -> Option<&str> {
    const NOISE: [&str; 3] = ["cutils-trace", "FeatureFlags", "AconfigStorage"];
    let lines = || {
        stderr
            .lines()
            .map(str::trim)
            .filter(|l| !NOISE.iter().any(|n| l.contains(n)))
    };
    [
        "Failed to create system AssetManager",
        "Failed to",
        "Read-only file system",
        "Permission denied",
        "No such file or directory",
        "Exception",
    ]
    .iter()
    .find_map(|m| lines().find(|l| l.contains(m)))
    .or_else(|| lines().find(|l| l.starts_with("atl-installer:")))
}

/// The device's Build.VERSION.RESOURCES_SDK_INT: `ro.build.version.sdk`, plus one while `ro.build.version.codename` is not `REL` (Build.java: ACTIVE_CODENAMES is empty only for REL).
fn device_sdk(props: &[Property]) -> Result<i32> {
    let get = |key: &str| {
        props
            .iter()
            .find(|(n, _)| n == key)
            .map(|(_, v)| v.as_str())
    };
    let sdk: i32 = get(SDK)
        .with_context(|| format!("the property set has no {SDK}"))?
        .parse()
        .with_context(|| SDK.to_owned())?;
    Ok(sdk + i32::from(get(CODENAME).is_some_and(|c| c != "REL")))
}

/// AOSP's `extractNativeLibs` of the base APK's manifest (ApkLiteParseUtils:509-510, default true), read from `aapt2 dump xmltree`: `=false` when the `<application>` attribute says so.
fn extract_native_libs(aapt2: &Path, base: &Path) -> Result<bool> {
    let tree = crate::host_art::output(
        Command::new(aapt2)
            .arg("dump")
            .arg("xmltree")
            .arg("--file")
            .arg("AndroidManifest.xml")
            .arg(base),
        "aapt2 dump xmltree",
    )?;
    let Some(line) = tree.lines().find(|l| l.contains("extractNativeLibs(")) else {
        return Ok(true);
    };
    match line.rsplit_once('=').map(|(_, v)| v.trim()) {
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        _ => bail!(
            "extractNativeLibs of {}: unrecognised form {line:?}",
            base.display()
        ),
    }
}

/// A row's copied libraries as (name, sha256), sorted.
type LibSums = Vec<(String, String)>;

/// A library the zip carries as `lib/<abi>/<name>` (one level, the entries AOSP's copy takes): its ABI, name, and sha256 of its bytes (`unzip -p`).
struct NativeEntry {
    abi: String,
    name: String,
    sha256: String,
}

fn native_entries(apk: &Path) -> Result<Vec<NativeEntry>> {
    let listing = crate::host_art::output(Command::new("unzip").arg("-Z1").arg(apk), "unzip -Z1")?;
    let mut entries = Vec::new();
    for entry in listing.lines() {
        let parts: Vec<&str> = entry.split('/').collect();
        if parts.len() != 3 || parts[0] != "lib" || parts[2].is_empty() {
            continue;
        }
        let bytes = Command::new("unzip")
            .arg("-p")
            .arg(apk)
            .arg(entry)
            .output()
            .with_context(|| format!("unzip -p {entry}"))?;
        ensure!(
            bytes.status.success(),
            "unzip -p {entry} of {}: {}",
            apk.display(),
            bytes.status
        );
        entries.push(NativeEntry {
            abi: parts[1].to_owned(),
            name: parts[2].to_owned(),
            sha256: test_apks::sha256_hex(&bytes.stdout),
        });
    }
    Ok(entries)
}

/// AOSP's native choice over a row's files, from the zip: the first device ABI any file carries (findSupportedAbi takes the lowest index over every APK). With `extractNativeLibs` that ABI's libraries as (name, sha256), sorted; without it none (derivePackageAbi:364, shouldExtractLibs).
fn natives(
    device: &[String],
    files: &[PathBuf],
    extract: bool,
) -> Result<(Option<String>, Option<LibSums>)> {
    let mut all = Vec::new();
    for f in files {
        all.extend(native_entries(f)?);
    }
    let primary = device
        .iter()
        .find(|abi| all.iter().any(|e| e.abi == **abi))
        .cloned();
    if !extract {
        return Ok((primary, None));
    }
    let mut libs: Vec<(String, String)> = all
        .into_iter()
        .filter(|e| Some(e.abi.as_str()) == primary.as_deref())
        .map(|e| (e.name, e.sha256))
        .collect();
    libs.sort();
    Ok((primary, Some(libs)))
}

/// The files of `dir` as (name, sha256), sorted: what the installer copied, read back here.
fn dir_files(dir: &Path) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let bytes = std::fs::read(entry.path())?;
            files.push((
                entry.file_name().to_string_lossy().into_owned(),
                test_apks::sha256_hex(&bytes),
            ));
        }
    }
    files.sort();
    Ok(files)
}

/// What the oracle says a package dir must give: identity and splits (aapt2), the primary ABI and its libraries (the zip).
struct Want {
    base: Badging,
    /// The base APK, the full parse's oracle input.
    base_apk: PathBuf,
    splits: Vec<String>,
    primary: Option<String>,
    /// (name, sha256), sorted; None: nothing is copied (extractNativeLibs=false, the libraries stay in the APK).
    libs: Option<LibSums>,
}

struct Gate<'a> {
    host: &'a HostArt,
    installer: PathBuf,
    /// atl-installer-dump: the installer's frames decoded by atl-services (`installed::decode`) into lines
    dump: PathBuf,
    /// The api-impl dex `xtask native` built (the ParseCallback forwarder): the installer VM's class path.
    api_impl: PathBuf,
    aapt2: PathBuf,
    /// The device's resource SDK (`device_sdk`).
    sdk: i32,
    /// The broker the installer runs through: its installer profile (`spawn_installer`).
    broker: &'a Broker,
    /// framework-res.apk of the converter's build, shown at its Android path when `Show::framework`.
    framework_res: PathBuf,
    /// The run's files: each installer run's frames and stderr.
    work: PathBuf,
    data: PathBuf,
    fails: Vec<String>,
}

impl Gate<'_> {
    /// What the installer's view shows of the caller's files, on the broker's root: the runtime read-only at its own paths (the ART root with the links it holds, the boot classpath, the api-impl dex), framework-res at its Android path,
    /// per `show` the package dir (read-only, its links' targets too) and the output dir (read-write, created here).
    fn view(&self, dir: &Path, lib_root: &Path, show: Show) -> Result<Vec<Mount>> {
        let ro = |p: &Path| Mount {
            host: p.to_path_buf(),
            at: p.to_path_buf(),
            writable: false,
        };
        let mut mounts = tree_mounts(&self.host.art)?;
        mounts.extend(self.host.boot.iter().map(|j| ro(j)));
        mounts.push(ro(&self.api_impl));
        if show.framework {
            mounts.push(Mount {
                host: self.framework_res.clone(),
                at: Path::new(SYSTEM_FRAMEWORK).join("framework-res.apk"),
                writable: false,
            });
        }
        if show.input {
            mounts.extend(tree_mounts(dir)?);
        }
        if show.output {
            std::fs::create_dir_all(lib_root)?;
            mounts.push(Mount {
                host: lib_root.to_path_buf(),
                at: lib_root.to_path_buf(),
                writable: true,
            });
        }
        Ok(mounts)
    }

    /// The installer over `dir` with the device property file `area` and declared `features`, libraries into `lib_root`, run in the broker's installer profile: its frames, the run, and its wall time.
    fn run_with(
        &self,
        area: &Path,
        features: &[&str],
        dir: &Path,
        lib_root: &Path,
        show: Show,
    ) -> Result<(ArtRun, Vec<u8>, u128)> {
        let own: Vec<String> = features.iter().map(|f| format!("--feature={f}")).collect();
        let own: Vec<&str> = own.iter().map(String::as_str).collect();
        let mut cmd = self
            .host
            .art_command(&self.installer, &own, &self.data, IN_VIEW_AREA);
        cmd.arg(format!("-Djava.class.path={}", self.api_impl.display()))
            .arg(dir)
            .arg(lib_root);
        let mut spawn = binder::spawn_of(&cmd, INSTALLER_UID)?;
        spawn.mounts.extend(self.view(dir, lib_root, show)?);
        spawn.property_area = Some(area.to_path_buf());
        let stem = lib_root
            .file_name()
            .context("a lib root without a name")?
            .to_string_lossy()
            .into_owned();
        let frames_path = self.work.join(format!("{stem}.frames"));
        let stderr_path = self.work.join(format!("{stem}.stderr"));
        spawn.stdout = Stdio::from(File::create(&frames_path)?);
        spawn.stderr = Stdio::from(File::create(&stderr_path)?);
        let start = Instant::now();
        let mut p = self
            .broker
            .spawn_installer(spawn)
            .context("starting the installer in its profile")?;
        // the broker's share of the wall time: mounts, the ELF closure of the runtime, bwrap's set-up, before the installer runs
        println!("{stem}: profile spawn {} ms", start.elapsed().as_millis());
        let status = binder::wait_limited(&mut p.child, RUN_LIMIT)?;
        let wall = start.elapsed().as_millis();
        let run = ArtRun {
            ok: status.is_some_and(|s| s.success()),
            code: status.and_then(|s| s.code()),
            signal: status.and_then(killed_by),
            status: status.map_or_else(
                || format!("still running after {RUN_LIMIT:?}, killed"),
                |s| s.to_string(),
            ),
            stdout: String::new(),
            stderr: std::fs::read_to_string(&stderr_path).unwrap_or_default(),
        };
        Ok((run, std::fs::read(&frames_path)?, wall))
    }

    /// `run_with` on this device: no declared features (atld's PackageManager declares none).
    fn run(
        &self,
        area: &Path,
        dir: &Path,
        lib_root: &Path,
        show: Show,
    ) -> Result<(ArtRun, Vec<u8>, u128)> {
        self.run_with(area, &[], dir, lib_root, show)
    }

    /// The installer's `frames` decoded (atl-installer-dump): its lines, or its error line.
    fn decode(&self, frames: &[u8]) -> Result<std::result::Result<String, String>> {
        use std::io::Write as _;
        // the converter's libbinder_ndk and liblog, from the staged runtime's library dir as every converter binary the harness runs
        let mut child = Command::new(&self.dump)
            .env("LD_LIBRARY_PATH", &self.host.lib_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("running {}", self.dump.display()))?;
        let mut stdin = child.stdin.take().context("dump stdin")?;
        // written from a thread: the dump reads all of stdin before writing, but a large output must not wait on a full pipe either way
        let input = frames.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let out = child.wait_with_output()?;
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("dump stdin writer panicked"))??;
        let stdout = String::from_utf8(out.stdout).context("dump output")?;
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Ok(if out.status.success() {
            Ok(stdout)
        } else {
            // a failed decode prints nothing on stdout (installer_dump.rs)
            ensure!(
                stdout.is_empty(),
                "atl-installer-dump failed after printing lines: {stderr}"
            );
            Err(format!("{} ({})", stderr, out.status))
        })
    }

    /// Breaks on `frames` (a good output): a corrupt frame length, then a truncated PackageInfo parcel, must each be refused by name with nothing decoded.
    fn decode_breaks(&mut self, name: &str, frames: &[u8]) -> Result<()> {
        ensure!(
            frames.len() > 16,
            "{name}: output of {} bytes",
            frames.len()
        );
        let mut corrupt = frames.to_vec();
        // the first frame's length (bytes 4..8, little-endian) past the end
        corrupt[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let first = u32::from_le_bytes(frames[4..8].try_into().expect("4 bytes")) as usize;
        // the PackageInfo parcel without its last 8 bytes, its frame length to match, the rest of the output dropped
        let mut truncated = frames[..8 + first - 8].to_vec();
        truncated[4..8].copy_from_slice(&u32::try_from(first - 8)?.to_le_bytes());
        for (case, input, marker) in [
            ("corrupt-length", corrupt, "claims"),
            ("truncated-parcel", truncated, "refused"),
        ] {
            match self.decode(&input)? {
                Err(e) if e.contains(marker) => println!("{name}-{case}: PASS refused: {e}"),
                Err(e) => self.fail(
                    &format!("{name}-{case}"),
                    format!("refused without `{marker}`: {e}"),
                ),
                Ok(_) => self.fail(&format!("{name}-{case}"), "decoded".to_owned()),
            }
        }
        Ok(())
    }

    /// The full parse of `stdout` against the oracle of `base_apk` (failures into `bad`), and against manifest.rs (printed as findings of check `name`).
    fn full_parse(
        &self,
        name: &str,
        stdout: &str,
        base_apk: &Path,
        bad: &mut Vec<String>,
    ) -> Result<Full> {
        let full = Full::read(stdout)?;
        bad.extend(compare(&full, &oracle(&self.aapt2, base_apk, self.sdk)?));
        let m = manifest::read(&self.aapt2, base_apk)?;
        for (field, ours, theirs) in manifest_findings(&full, &m) {
            println!("{name}: finding {field}: installer {ours} != manifest.rs {theirs}");
        }
        Ok(full)
    }

    /// Break `net`: atl-sandbox-probe, run in the installer's profile, reaches for TCP. The profile's filter kills it at its socket(AF_INET), and the verdict names the audit record dmesg holds for that call.
    fn net_break(&mut self, probe: &Path) -> Result<()> {
        let name = BREAK_NET;
        let file = |n: &str| self.work.join(n);
        std::fs::write(file("net-home-file"), b"a file outside the view")?;
        let mut cmd = Command::new(probe);
        cmd.arg(file("net-home-file"))
            .arg(file("net-home-new"))
            .arg("/usr/atl-sandbox-probe")
            .arg(format!("{DATA}/user/0/{PROBE_DATA_PACKAGE}/probe.txt"))
            .arg("9")
            .arg(file("net-socket"))
            .arg(file("net-other-socket"))
            .arg(file("net-fonts.conf"))
            .arg(file("net-fonts-sibling.conf"))
            .arg("/sys/devices/system/cpu/online")
            .arg("/sys/devices/system/cpu/online");
        let mut spawn = binder::spawn_of(&cmd, INSTALLER_UID)?;
        let log = self.work.join("net-probe.log");
        (spawn.stdout, spawn.stderr) = to_log(File::create(&log)?)?;
        let before = seccomp_records();
        let mut p = self
            .broker
            .spawn_installer(spawn)
            .context("starting the probe in the installer profile")?;
        let status = binder::wait_limited(&mut p.child, RUN_LIMIT)?;
        let refused = new_records(before, seccomp_records());
        match status {
            Some(s) if killed_by(s) == Some(SIGSYS) => {
                // the first audit record of this run names the call (x86_64: 41 = socket)
                let call = refused
                    .first()
                    .and_then(|l| l.split("syscall=").nth(1))
                    .and_then(|n| n.split_whitespace().next())
                    .unwrap_or("?");
                self.fail(
                    name,
                    format!(
                        "refused: the probe was killed by SIGSYS in the installer profile at syscall {call} (41 = socket: its TCP socket)"
                    ),
                );
            }
            Some(s) if s.success() => self.fail(
                name,
                "probe completed: a TCP socket went through the installer profile".to_owned(),
            ),
            Some(s) => self.fail(
                name,
                format!(
                    "probe exited {s}: {}",
                    std::fs::read_to_string(&log).unwrap_or_default().trim()
                ),
            ),
            None => self.fail(
                name,
                format!("probe still running after {RUN_LIMIT:?}, killed"),
            ),
        }
        Ok(())
    }

    fn fail(&mut self, check: &str, msg: String) {
        println!("{check}: FAIL {msg}");
        self.fails.push(format!("{check}: {msg}"));
    }

    /// Check `name`: the installer over `dir` equals the oracle `want`: identity and splits, primary ABI, and the lib dir's files by sha256.
    fn expect(
        &mut self,
        name: &str,
        area: &Path,
        dir: &Path,
        lib_root: &Path,
        want: &Want,
        show: Show,
    ) -> Result<()> {
        let (r, frames, wall) = self.run(area, dir, lib_root, show)?;
        if !r.ok {
            let refused = show.refusal().map(|w| format!("{w}; ")).unwrap_or_default();
            let how = match r.signal {
                Some(SIGSYS) => "killed by SIGSYS (a seccomp denial)".to_owned(),
                Some(n) => format!("killed by signal {n}"),
                None => r.status.clone(),
            };
            let said = refusal_line(&r.stderr)
                .map(|l| format!(": {l}"))
                .unwrap_or_default();
            self.fail(name, format!("{refused}installer {how}{said}"));
            return Ok(());
        }
        let decoded = match self.decode(&frames)? {
            Ok(d) => d,
            Err(e) => {
                self.fail(name, format!("decode: {e}"));
                return Ok(());
            }
        };
        if name == "alto" {
            self.decode_breaks(name, &frames)?;
        }
        let p = parse_output(&decoded, &r.stderr)?;
        let mut got = p.splits.clone();
        got.sort();
        let mut splits = want.splits.clone();
        splits.sort();
        let mut bad = Vec::new();
        let full = self.full_parse(name, &decoded, &want.base_apk, &mut bad)?;
        if p.package != want.base.package {
            bad.push(format!(
                "package {} != aapt2 {}",
                p.package, want.base.package
            ));
        }
        if p.version_code != want.base.version_code {
            bad.push(format!(
                "versionCode {} != aapt2 {}",
                p.version_code, want.base.version_code
            ));
        }
        if got != splits {
            bad.push(format!("splits {got:?} != aapt2 {splits:?}"));
        }
        if p.primary_abi != want.primary {
            bad.push(format!(
                "primary-abi {} != oracle {}",
                p.primary_abi.as_deref().unwrap_or("none"),
                want.primary.as_deref().unwrap_or("none")
            ));
        }
        match (&p.lib_dir, &want.libs) {
            (None, None) => {}
            (Some(d), Some(libs)) => {
                if d.parent() != Some(lib_root) {
                    bad.push(format!(
                        "lib-dir {} is not under {}",
                        d.display(),
                        lib_root.display()
                    ));
                }
                let files = dir_files(d).unwrap_or_else(|e| {
                    bad.push(format!("{e:#}"));
                    Vec::new()
                });
                if files != *libs {
                    bad.push(format!(
                        "lib-dir files {} != zip's {}",
                        files.len(),
                        libs.len()
                    ));
                }
            }
            (got, libs) => bad.push(format!(
                "lib-dir {} != oracle {}",
                got.as_ref()
                    .map_or("none".to_owned(), |d| d.display().to_string()),
                if libs.is_some() {
                    "a copied dir"
                } else {
                    "none (extractNativeLibs=false: libraries stay in the APK)"
                }
            )),
        }
        if bad.is_empty() {
            let libs = want.libs.as_ref().map_or_else(
                || "left in the APK (extractNativeLibs=false)".to_owned(),
                |l| format!("{} copied, sha256 equal to unzip -p", l.len()),
            );
            let count = |k: &str| full.components.iter().filter(|c| c.kind == k).count();
            println!(
                "{name}: PASS package {} versionCode {} splits {got:?} target-sdk {} launcher {} activities {} receivers {} services {} providers {} uses-permission {} permission {} primary-abi {} libraries {libs}",
                p.package,
                p.version_code,
                full.value("target-sdk"),
                full.launchers().first().map_or("none", String::as_str),
                count("activity"),
                count("receiver"),
                count("service"),
                count("provider"),
                full.uses_permissions.len(),
                full.permissions.len(),
                want.primary.as_deref().unwrap_or("none"),
            );
        } else {
            self.fail(name, bad.join("; "));
        }
        println!(
            "{name}: measured installer vm start {} ms, lite parse {} ms, full parse {} ms, natives {} ms, process wall {wall} ms",
            p.vm_ms, p.lite_ms, p.parse_ms, p.natives_ms
        );
        Ok(())
    }

    /// Check `feature`: the fixture's only request needs a feature; with the device's features (`features`, none unless broken) AOSP drops it, so the installer requests nothing.
    fn feature(
        &mut self,
        area: &Path,
        features: &[&str],
        dir: &Path,
        lib_root: &Path,
    ) -> Result<()> {
        let name = "feature";
        let (r, frames, _) = self.run_with(area, features, dir, lib_root, Show::ALL)?;
        if !r.ok {
            self.fail(name, format!("installer {}: {}", r.status, r.failure()));
            return Ok(());
        }
        let decoded = match self.decode(&frames)? {
            Ok(d) => d,
            Err(e) => {
                self.fail(name, format!("decode: {e}"));
                return Ok(());
            }
        };
        let full = Full::read(&decoded)?;
        if full.uses_permissions.is_empty() {
            println!(
                "{name}: PASS uses-permission [] ({FEATURE_PERMISSION} needs {FEATURE}, which this device does not declare)"
            );
        } else {
            self.fail(
                name,
                format!(
                    "uses-permission {:?} != oracle [] ({FEATURE_PERMISSION} needs {FEATURE}, which this device does not declare)",
                    full.uses_permissions
                ),
            );
        }
        Ok(())
    }

    /// Check `name`: the installer over `dir` exits non-zero with AOSP's refusal `parse error <code>: <marker>...`.
    fn refuse(
        &mut self,
        name: &str,
        area: &Path,
        dir: &Path,
        lib_root: &Path,
        code: i32,
        marker: &str,
    ) -> Result<()> {
        let (r, _, _) = self.run(area, dir, lib_root, Show::ALL)?;
        // the message after the code may lead with the file and position (ParseTypeImpl.error with the parser's description)
        let want = format!("parse error {code}: ... {marker}");
        let line = r
            .stderr
            .lines()
            .find(|l| l.contains(&format!("parse error {code}: ")) && l.contains(marker));
        match (line, r.ok) {
            (Some(l), false) => println!("{name}: PASS {}", l.trim()),
            _ => self.fail(
                name,
                format!("no `{want}` refusal ({}): {}", r.status, r.failure()),
            ),
        }
        Ok(())
    }
}

/// The seccomp audit records dmesg holds now (`type=1326`, one per call the filter denies: kill mode ends the process too). None when dmesg cannot be read: the count is then not judged.
fn seccomp_records() -> Option<Vec<String>> {
    let out = Command::new("dmesg").output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("type=1326"))
            .map(str::to_owned)
            .collect()
    })
}

/// The records dmesg gained between two `seccomp_records` reads (dmesg only appends; a read that lost its start yields none).
fn new_records(before: Option<Vec<String>>, after: Option<Vec<String>>) -> Vec<String> {
    match (before, after) {
        (Some(b), Some(a)) => a.get(b.len()..).unwrap_or_default().to_vec(),
        _ => Vec::new(),
    }
}

/// atl-sandbox-probe built into `<out>/cargo`, as `xtask test sandbox` builds it: its path.
fn build_probe(root: &Path, out: &Path) -> Result<PathBuf> {
    let status = crate::unit_use::status(
        crate::cargo_command()
            .args(["build", "--release", "-p", PROBE_PACKAGE, "--target-dir"])
            .arg(out.join("cargo"))
            .current_dir(root),
    )?;
    ensure!(status.success(), "building {PROBE_PACKAGE} failed");
    Ok(out.join("cargo/release").join(PROBE_PACKAGE))
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    let out = crate::build_state::gate_out(root, args.out.as_deref(), &args.host, CMD)?;
    let java_out = slot::out_dir(root, args.java_out.as_deref(), "--java-out", |w| {
        w.join("out/java/build")
    })?;
    let host = HostArt::new(
        root,
        &args.host,
        Some(&java_out),
        &out,
        "installer-art",
        args.verbose,
    )?;
    let tools = ApkTools::new(root, &args.host, &out, &java_out)?;
    let work = out.join(CMD);
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    std::fs::create_dir_all(work.join("data"))?;
    let (device_props, area) = device_area(&host, &work.join("device"), None)?;
    let device = device_list(&device_props);
    ensure!(
        !device.is_empty(),
        "the default property set has no {ABILIST}"
    );
    let alto_area = if args.r#break.as_deref() == Some(BREAK_ABI) {
        device_area(&host, &work.join("device-broken"), Some(BROKEN_DEVICE_ABI))?.1
    } else {
        area.clone()
    };
    let api_impl = xtask::host_art::api_impl_dex(&java_out);
    ensure!(
        api_impl.is_file(),
        "no api-impl dex {}: `cargo xtask native` builds it with the Java goals",
        api_impl.display()
    );
    // the installer runs through the broker, as atld's processes do: binderd starts as atld starts it
    let (broker, mut binderd) = binder::start_binderd(root, &out, &work.join("binderd.log"))?;
    let seccomp_before = seccomp_records();
    let probe = if args.r#break.as_deref() == Some(BREAK_NET) {
        Some(build_probe(root, &out)?)
    } else {
        None
    };
    let mut gate = Gate {
        host: &host,
        installer: packages::bin(root, &out, PACKAGE, INSTALLER)?,
        dump: packages::bin(root, &out, DUMP_PACKAGE, DUMP)?,
        api_impl,
        aapt2: tools.aapt2.clone(),
        sdk: device_sdk(&device_props)?,
        broker: &broker,
        framework_res: out.join(FRAMEWORK_RES),
        work: work.clone(),
        data: work.join("data"),
        fails: Vec::new(),
    };

    // 1. alto: base + its split, from the cache; the oracle is the whole row
    let table = test_apks::load(root)?;
    let row = table
        .get(SPLIT_ROW)
        .with_context(|| format!("no row {SPLIT_ROW}"))?;
    let mut base = None;
    let mut splits = Vec::new();
    let mut row_files = Vec::new();
    for pin in row.pins() {
        let file = cached(root, pin)?;
        let b = badging(&tools.aapt2, &file)?;
        row_files.push(file.clone());
        match b.split.clone() {
            None => base = Some((b, file)),
            Some(s) => splits.push((s, file)),
        }
    }
    let (alto, alto_base) = base.context("altos-odyssey: no base among its files")?;
    ensure!(!splits.is_empty(), "altos-odyssey: aapt2 names no split");
    let mut staged: Vec<(Option<&str>, &Path)> = vec![(None, &alto_base)];
    if args.r#break.as_deref() != Some("drop-split") {
        staged.extend(splits.iter().map(|(s, f)| (Some(s.as_str()), f.as_path())));
    }
    let dir = work.join("alto");
    stage_dir(&dir, &staged)?;
    let extract = extract_native_libs(&tools.aapt2, &alto_base)?;
    let (primary, libs) = natives(&device, &row_files, extract)?;
    let want = Want {
        base: alto,
        base_apk: alto_base.clone(),
        splits: splits.iter().map(|(s, _)| s.clone()).collect(),
        primary,
        libs,
    };
    // a break takes one part of alto's view away, or moves its output into the read-only package dir: the verdict names what refused it
    let brk = args.r#break.as_deref();
    let show = Show {
        input: brk != Some(BREAK_READ_OUTSIDE),
        framework: brk != Some(BREAK_NO_FRAMEWORK),
        output: brk != Some(BREAK_WRITE_OUTSIDE),
    };
    let alto_lib = if brk == Some(BREAK_WRITE_OUTSIDE) {
        dir.join("elsewhere")
    } else {
        work.join("alto-lib")
    };
    gate.expect("alto", &alto_area, &dir, &alto_lib, &want, show)?;

    // 2. single
    let single_apk = one_file(root, SINGLE_ROW)?;
    let single = badging(&tools.aapt2, &single_apk)?;
    if single.split.is_some() {
        bail!("{SINGLE_ROW}: aapt2 reads a split, not a base");
    }
    let dir = work.join("single");
    stage_dir(&dir, &[(None, &single_apk)])?;
    let extract = extract_native_libs(&tools.aapt2, &single_apk)?;
    let (primary, libs) = natives(&device, std::slice::from_ref(&single_apk), extract)?;
    let want = Want {
        base: single.clone(),
        base_apk: single_apk.clone(),
        splits: Vec::new(),
        primary,
        libs,
    };
    gate.expect(
        "single",
        &area,
        &dir,
        &work.join("single-lib"),
        &want,
        Show::ALL,
    )?;

    // 3. rich: a manifest with every component kind
    let rich_apk = one_file(root, RICH_ROW)?;
    let rich = badging(&tools.aapt2, &rich_apk)?;
    let dir = work.join("rich");
    stage_dir(&dir, &[(None, &rich_apk)])?;
    let extract = extract_native_libs(&tools.aapt2, &rich_apk)?;
    let (primary, libs) = natives(&device, std::slice::from_ref(&rich_apk), extract)?;
    let want = Want {
        base: rich,
        base_apk: rich_apk.clone(),
        splits: Vec::new(),
        primary,
        libs,
    };
    gate.expect(
        "rich",
        &area,
        &dir,
        &work.join("rich-lib"),
        &want,
        Show::ALL,
    )?;

    // 4. refusals: AOSP's consistency check between base and split
    let cases = [
        (
            "refuse-version",
            single.package.clone(),
            single.version_code + 1,
            "Inconsistent version",
        ),
        (
            "refuse-package",
            format!("{}.other", single.package),
            single.version_code,
            "Inconsistent package",
        ),
    ];
    for (name, package, version_code, marker) in cases {
        let split = link_split(&tools, &work, &package, version_code, "config.x86_64")?;
        let dir = work.join(name);
        stage_dir(
            &dir,
            &[(None, &single_apk), (Some("config.x86_64"), &split)],
        )?;
        gate.refuse(
            name,
            &area,
            &dir,
            &work.join(format!("{name}-lib")),
            BAD_MANIFEST,
            marker,
        )?;
    }

    // 5. a manifest only the full parse refuses: one permission declared twice, the second with another protection level
    let name = "refuse-duplicate-permission";
    let apk = link_app(
        &tools,
        &work,
        "duplicate",
        "  <permission android:name=\"org.atl.fixture.P\" android:protectionLevel=\"normal\"/>\n  <permission android:name=\"org.atl.fixture.P\" android:protectionLevel=\"signature\"/>\n",
    )?;
    let dir = work.join(name);
    stage_dir(&dir, &[(None, &apk)])?;
    gate.refuse(
        name,
        &area,
        &dir,
        &work.join(format!("{name}-lib")),
        MANIFEST_MALFORMED,
        "Found duplicate permission",
    )?;

    // 6. feature: a request gated on a feature the device does not declare
    let apk = link_app(
        &tools,
        &work,
        "feature",
        &format!(
            "  <uses-permission android:name=\"{FEATURE_PERMISSION}\">\n    <required-feature android:name=\"{FEATURE}\"/>\n  </uses-permission>\n"
        ),
    )?;
    let dir = work.join("feature");
    stage_dir(&dir, &[(None, &apk)])?;
    let features: &[&str] = if args.r#break.as_deref() == Some(BREAK_FEATURE) {
        &[FEATURE]
    } else {
        &[]
    };
    gate.feature(&area, features, &dir, &work.join("feature-lib"))?;

    if let Some(probe) = &probe {
        gate.net_break(probe)?;
    }
    // every seccomp denial the run recorded: a denial in kill mode also ends its process, so it is a failure either way
    match (seccomp_before.is_some(), seccomp_records()) {
        (true, Some(after)) => {
            let new = new_records(seccomp_before, Some(after));
            println!(
                "installer: seccomp denials during the run: {} (kill mode; dmesg type=1326 records)",
                new.len()
            );
            for line in new.iter().take(3) {
                println!("{line}");
            }
            if !new.is_empty() {
                gate.fails
                    .push(format!("seccomp: {} denied call(s) in dmesg", new.len()));
            }
        }
        _ => println!("installer: seccomp denials: not judged (dmesg unreadable)"),
    }
    println!("{}", verdict::gate(CMD, &gate.fails));
    let ok = gate.fails.is_empty();
    drop(gate);
    drop(broker);
    binder::wait_limited(&mut binderd.child, RUN_LIMIT)?;
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::{device_list, none_or, parse_output};
    use atl_property_service::Property;

    #[test]
    fn reads_the_installer_lines() {
        let p = parse_output(
            "package a.b\nversion-code 7\nsplit x\nsplit y\nprimary-abi x86_64\nlib-dir /o/lib/x86_64\n",
            "W/art: noise\ntime-ms vm 10 lite 2 parse 3 natives 5\n",
        )
        .unwrap();
        assert_eq!(
            (
                p.package.as_str(),
                p.version_code,
                p.splits.len(),
                p.primary_abi.as_deref(),
                p.lib_dir.as_deref(),
                p.vm_ms,
                p.lite_ms,
                p.parse_ms,
                p.natives_ms
            ),
            (
                "a.b",
                7,
                2,
                Some("x86_64"),
                Some(std::path::Path::new("/o/lib/x86_64")),
                10,
                2,
                3,
                5
            )
        );
        let none = parse_output(
            "package a.b\nprimary-abi none\nlib-dir none\n",
            "time-ms vm 1 lite 1 parse 1 natives 0\n",
        )
        .unwrap();
        assert_eq!((none.primary_abi, none.lib_dir), (None, None));
        assert!(parse_output("version-code 7\n", "").is_err());
        assert_eq!(none_or("none"), None);
    }

    #[test]
    fn device_list_is_the_abilist_in_order() {
        let props: Vec<Property> = vec![
            ("ro.product.cpu.abilist32".to_owned(), "x86".to_owned()),
            (
                "ro.product.cpu.abilist".to_owned(),
                "x86_64,x86,arm64-v8a".to_owned(),
            ),
        ];
        assert_eq!(device_list(&props), ["x86_64", "x86", "arm64-v8a"]);
        assert!(device_list(&[]).is_empty());
    }
}
