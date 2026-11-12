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
//! Measured: the installer's VM start, lite parse, full parse and natives times (its `time-ms` line) and the process wall time of each run.
//! Breaks: `--break drop-split` (check 1's dir without its split: splits, primary ABI and libraries must turn red); `--break abi-arm64` (the device's ABI lists are arm64-v8a only: AOSP's no-match, `native error -113`, must turn check 1 red);
//! `--break feature` (check 6's installer is told the device has android.hardware.camera: the forwarder's hasFeature answers true, uses-permission must turn red).
//! Default: mutating (stages ART into `<out>/installer-art`, writes `<out>/installer`).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use atl_property_service::{LinearTier, Property};

use crate::atld_run::ApkTools;
use crate::host_art::{ArtRun, HostArt};
use crate::installer_parse::{Full, compare, manifest_findings, oracle};
use crate::{manifest, packages, slot, test_apks, verdict};

const CMD: &str = "installer";
/// The package that builds both loaders, and the installer's executable.
const PACKAGE: &str = "atl-app-launcher";
const INSTALLER: &str = "atl-installer";
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
    /// Deliberate break that must turn a check red: `drop-split`, `abi-arm64` (check 1: no config split in the dir; the device's ABI lists are arm64-v8a only), `feature` (check 6: the installer is told the device has android.hardware.camera).
    #[arg(long, value_parser = ["drop-split", "abi-arm64", "feature"])]
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

/// What the installer printed (installer.rs in atl-app-launcher: `package`, `version-code`, `split`*, `primary-abi`, `lib-dir`, `time-ms vm <n> lite <n> parse <n> natives <n>`; the full parse's other lines are installer_parse.rs's).
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

fn parse_output(stdout: &str) -> Result<Parsed> {
    let mut p = Parsed::default();
    for line in stdout.lines() {
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
    ensure!(!p.package.is_empty(), "no package line in {stdout:?}");
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

/// The property area the installer maps (`ATL_PROP_AREA`): atl-property-service's default set over the host's build.prop, its ABI lists replaced by `abi` when given, laid out by its area writer into `dir`.
/// Returns the set (the oracle reads the device's ABI list from it) and (variable, path) for the process environment.
fn device_area(
    host: &HostArt,
    dir: &Path,
    abi: Option<&str>,
) -> Result<(Vec<Property>, (String, String))> {
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
    Ok((
        props,
        (
            atl_property_service::AREA_ENV.to_owned(),
            path.display().to_string(),
        ),
    ))
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

/// bwrap options for the installer's filesystem view: the host's `/` entry by entry (directories bound with their devices, symlinks re-made) on bwrap's own root, plus framework-res.apk at
/// `/system/framework/framework-res.apk`, the fixed path the full parse's system AssetManager opens (AssetManager.createSystemAssetsInZygoteLocked, via DefaultSplitAssetLoader.getBaseAssetManager).
/// Environment-side setup in place of the broker's installer sandbox (slice M5), which will bind it as app sandboxes do; the host has no `/system`, and its `/` is not writable, so it is rebuilt here.
fn system_view(framework_res: &Path) -> Result<Vec<std::ffi::OsString>> {
    let mut a = atl_bwrap::Args::new();
    a.die_with_parent();
    for e in std::fs::read_dir("/").context("reading /")? {
        let p = e?.path();
        let kind = std::fs::symlink_metadata(&p)?.file_type();
        if kind.is_symlink() {
            a.symlink(std::fs::read_link(&p)?, &p);
        } else if kind.is_dir() {
            a.dev_bind(&p, &p);
        }
    }
    a.ro_bind(
        framework_res,
        Path::new(atl_android_paths::SYSTEM_FRAMEWORK).join("framework-res.apk"),
    );
    Ok(a.end())
}

/// `cmd` run inside `view` (`system_view`): bwrap, then the program with its arguments and environment.
fn in_view(cmd: &Command, view: &[std::ffi::OsString]) -> Command {
    let mut wrapped = Command::new(atl_bwrap::PROGRAM);
    wrapped
        .args(view)
        .arg(cmd.get_program())
        .args(cmd.get_args());
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => wrapped.env(k, v),
            None => wrapped.env_remove(k),
        };
    }
    wrapped
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    wrapped
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
    /// The api-impl dex `xtask native` built (the ParseCallback forwarder): the installer VM's class path.
    api_impl: PathBuf,
    aapt2: PathBuf,
    /// The device's resource SDK (`device_sdk`).
    sdk: i32,
    /// bwrap options of the installer's filesystem view (`system_view`).
    view: Vec<std::ffi::OsString>,
    data: PathBuf,
    fails: Vec<String>,
}

impl Gate<'_> {
    /// The installer over `dir` with the device `area` and declared `features`, libraries into `lib_root`.
    fn run_with(
        &self,
        area: &(String, String),
        features: &[&str],
        dir: &Path,
        lib_root: &Path,
    ) -> Result<(ArtRun, u128)> {
        let own: Vec<String> = features.iter().map(|f| format!("--feature={f}")).collect();
        let own: Vec<&str> = own.iter().map(String::as_str).collect();
        let mut cmd = self
            .host
            .art_command(&self.installer, &own, &self.data, (&area.0, &area.1));
        cmd.arg(format!("-Djava.class.path={}", self.api_impl.display()))
            .arg(dir)
            .arg(lib_root);
        let mut cmd = in_view(&cmd, &self.view);
        let start = Instant::now();
        let r = self.host.run(&mut cmd)?;
        Ok((r, start.elapsed().as_millis()))
    }

    /// `run_with` on this device: no declared features (atld's PackageManager declares none).
    fn run(&self, area: &(String, String), dir: &Path, lib_root: &Path) -> Result<(ArtRun, u128)> {
        self.run_with(area, &[], dir, lib_root)
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

    fn fail(&mut self, check: &str, msg: String) {
        println!("{check}: FAIL {msg}");
        self.fails.push(format!("{check}: {msg}"));
    }

    /// Check `name`: the installer over `dir` equals the oracle `want`: identity and splits, primary ABI, and the lib dir's files by sha256.
    fn expect(
        &mut self,
        name: &str,
        area: &(String, String),
        dir: &Path,
        lib_root: &Path,
        want: &Want,
    ) -> Result<()> {
        let (r, wall) = self.run(area, dir, lib_root)?;
        if !r.ok {
            self.fail(name, format!("installer {}: {}", r.status, r.failure()));
            return Ok(());
        }
        let p = parse_output(&r.stdout)?;
        let mut got = p.splits.clone();
        got.sort();
        let mut splits = want.splits.clone();
        splits.sort();
        let mut bad = Vec::new();
        let full = self.full_parse(name, &r.stdout, &want.base_apk, &mut bad)?;
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
        area: &(String, String),
        features: &[&str],
        dir: &Path,
        lib_root: &Path,
    ) -> Result<()> {
        let name = "feature";
        let (r, _) = self.run_with(area, features, dir, lib_root)?;
        if !r.ok {
            self.fail(name, format!("installer {}: {}", r.status, r.failure()));
            return Ok(());
        }
        let full = Full::read(&r.stdout)?;
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
        area: &(String, String),
        dir: &Path,
        lib_root: &Path,
        code: i32,
        marker: &str,
    ) -> Result<()> {
        let (r, _) = self.run(area, dir, lib_root)?;
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
    let mut gate = Gate {
        host: &host,
        installer: packages::bin(root, &out, PACKAGE, INSTALLER)?,
        api_impl,
        aapt2: tools.aapt2.clone(),
        sdk: device_sdk(&device_props)?,
        view: system_view(&out.join(FRAMEWORK_RES))?,
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
    gate.expect("alto", &alto_area, &dir, &work.join("alto-lib"), &want)?;

    // 2. single
    let (_, single_apk) = test_apks::ensure(root, SINGLE_ROW)?;
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
    gate.expect("single", &area, &dir, &work.join("single-lib"), &want)?;

    // 3. rich: a manifest with every component kind
    let (_, rich_apk) = test_apks::ensure(root, RICH_ROW)?;
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
    gate.expect("rich", &area, &dir, &work.join("rich-lib"), &want)?;

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

    println!("{}", verdict::gate(CMD, &gate.fails));
    Ok(if gate.fails.is_empty() {
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
            "package a.b\nversion-code 7\nsplit x\nsplit y\nprimary-abi x86_64\nlib-dir /o/lib/x86_64\ntime-ms vm 10 lite 2 parse 3 natives 5\n",
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
            "package a.b\nprimary-abi none\nlib-dir none\ntime-ms vm 1 lite 1 parse 1 natives 0\n",
        )
        .unwrap();
        assert_eq!((none.primary_abi, none.lib_dir), (None, None));
        assert!(parse_output("version-code 7\n").is_err());
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
