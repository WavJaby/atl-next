// SPDX-License-Identifier: MIT
//! `xtask test installer`: the installer process (atl-installer, plan item 4b slices M1-M2) runs AOSP's `ApkLiteParseUtils.parseClusterPackageLite` and then AOSP's native-code choice (`NativeLibraryHelper`, `PackageAbiHelperImpl.derivePackageAbi`) on host ART over a package dir laid out as PackageManager stages one (`base.apk`, `split_<name>.apk`).
//! Oracles, read here independently of the installer: `aapt2 dump badging` of each file (package, versionCode, `split`); the zip itself through `unzip` (the `lib/<abi>/` entries, their names and sha256); the device's ABI list from the property set the installer maps (`ro.product.cpu.abilist`).
//!
//! Checks:
//! 1. `alto`: the altos-odyssey row (google-play: base + config.x86_64, both already in the test-APK cache and verified here by sha256) -> package, versionCode and split names equal aapt2's; primary ABI = the first device ABI the row's zip carries (x86_64); the lib dir `<root>/<isa>` holds exactly the zip's `lib/x86_64/` files, same sha256.
//! 2. `single`: the fossify-notes row (one APK, downloaded on first use) -> package, versionCode equal aapt2's, no split; its manifest says extractNativeLibs=false, so the primary ABI is the first device ABI its zip carries (x86_64) and nothing is copied (lib-dir none, the libraries stay in the APK).
//! 3. `refuse-version`, `refuse-package`: fossify-notes' base + a split linked here by aapt2 whose versionCode (base + 1) or package (base + `.other`) differs -> AOSP's own check refuses (`Inconsistent version` / `Inconsistent package`, PackageManager INSTALL_PARSE_FAILED_BAD_MANIFEST -101), the installer exits non-zero.
//!
//! Measured: the installer's VM start, parse and natives times (its `time-ms` line) and the process wall time of check 1.
//! Breaks: `--break drop-split` (check 1's dir without its split: splits, primary ABI and libraries must turn red); `--break abi-arm64` (the device's ABI lists are arm64-v8a only: AOSP's no-match, `native error -113`, must turn check 1 red).
//! Default: mutating (stages ART into `<out>/installer-art`, writes `<out>/installer`).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use atl_property_service::{LinearTier, Property};

use crate::atld_run::ApkTools;
use crate::host_art::{ArtRun, HostArt};
use crate::{packages, slot, test_apks, verdict};

const CMD: &str = "installer";
/// The package that builds both loaders, and the installer's executable.
const PACKAGE: &str = "atl-app-launcher";
const INSTALLER: &str = "atl-installer";
/// The google-play row with a split (test-apks.toml).
const SPLIT_ROW: &str = "altos-odyssey";
/// A one-APK url row (test-apks.toml).
const SINGLE_ROW: &str = "fossify-notes";
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
    /// Deliberate break that must turn check 1 red: `drop-split` (no config split in the dir), `abi-arm64` (the device's ABI lists are arm64-v8a only).
    #[arg(long, value_parser = ["drop-split", "abi-arm64"])]
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

/// What the installer printed (installer.rs in atl-app-launcher: `package`, `version-code`, `split`*, `primary-abi`, `lib-dir`, `time-ms vm <n> parse <n> natives <n>`).
#[derive(Debug, Default)]
struct Parsed {
    package: String,
    version_code: i64,
    splits: Vec<String>,
    primary_abi: Option<String>,
    lib_dir: Option<PathBuf>,
    vm_ms: u64,
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
                    f.len() == 6 && f[0] == "vm" && f[2] == "parse" && f[4] == "natives",
                    "time-ms line {line:?}"
                );
                p.vm_ms = f[1].parse()?;
                p.parse_ms = f[3].parse()?;
                p.natives_ms = f[5].parse()?;
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
    let dir = work.join(format!("split-{package}-{version_code}"));
    std::fs::create_dir_all(&dir)?;
    let manifest = dir.join("AndroidManifest.xml");
    std::fs::write(
        &manifest,
        format!(
            "<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" package=\"{package}\" android:versionCode=\"{version_code}\" split=\"{split}\">\n  <application android:hasCode=\"false\"/>\n</manifest>\n"
        ),
    )?;
    let apk = dir.join("split.apk");
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
    splits: Vec<String>,
    primary: Option<String>,
    /// (name, sha256), sorted; None: nothing is copied (extractNativeLibs=false, the libraries stay in the APK).
    libs: Option<LibSums>,
}

struct Gate<'a> {
    host: &'a HostArt,
    installer: PathBuf,
    data: PathBuf,
    fails: Vec<String>,
}

impl Gate<'_> {
    /// The installer over `dir` with the device `area`, libraries into `lib_root`.
    fn run(&self, area: &(String, String), dir: &Path, lib_root: &Path) -> Result<(ArtRun, u128)> {
        let mut cmd = self
            .host
            .art_command(&self.installer, &[], &self.data, (&area.0, &area.1));
        cmd.arg(dir).arg(lib_root);
        let start = Instant::now();
        let r = self.host.run(&mut cmd)?;
        Ok((r, start.elapsed().as_millis()))
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
            println!(
                "{name}: PASS package {} versionCode {} splits {got:?} primary-abi {} libraries {libs}",
                p.package,
                p.version_code,
                want.primary.as_deref().unwrap_or("none"),
            );
        } else {
            self.fail(name, bad.join("; "));
        }
        println!(
            "{name}: measured installer vm start {} ms, parse {} ms, natives {} ms, process wall {wall} ms",
            p.vm_ms, p.parse_ms, p.natives_ms
        );
        Ok(())
    }

    /// Check `name`: the installer over `dir` exits non-zero with AOSP's refusal `parse error -101: <marker>...`.
    fn refuse(
        &mut self,
        name: &str,
        area: &(String, String),
        dir: &Path,
        lib_root: &Path,
        marker: &str,
    ) -> Result<()> {
        let (r, _) = self.run(area, dir, lib_root)?;
        let want = format!("parse error {BAD_MANIFEST}: {marker}");
        let line = r.stderr.lines().find(|l| l.contains(&want));
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
    let mut gate = Gate {
        host: &host,
        installer: packages::bin(root, &out, PACKAGE, INSTALLER)?,
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
        splits: Vec::new(),
        primary,
        libs,
    };
    gate.expect("single", &area, &dir, &work.join("single-lib"), &want)?;

    // 3. refusals: AOSP's consistency check between base and split
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
        gate.refuse(name, &area, &dir, &work.join(format!("{name}-lib")), marker)?;
    }

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
            "package a.b\nversion-code 7\nsplit x\nsplit y\nprimary-abi x86_64\nlib-dir /o/lib/x86_64\ntime-ms vm 10 parse 3 natives 5\n",
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
                3,
                5
            )
        );
        let none = parse_output(
            "package a.b\nprimary-abi none\nlib-dir none\ntime-ms vm 1 parse 1 natives 0\n",
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
