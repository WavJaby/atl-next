// SPDX-License-Identifier: MIT
//! `xtask test installer`: the installer process (atl-installer, plan item 4b slice M1) runs AOSP's `ApkLiteParseUtils.parseClusterPackageLite` on host ART over a package dir laid out as PackageManager stages one (`base.apk`, `split_<name>.apk`).
//! Oracle: `aapt2 dump badging` of each file, read here independently of the installer (package, versionCode, and `split` of each split file).
//!
//! Checks:
//! 1. `alto`: the altos-odyssey row (google-play: base + config.x86_64, both already in the test-APK cache and verified here by sha256) -> package, versionCode and split names equal aapt2's.
//! 2. `single`: the fossify-notes row (one APK, downloaded on first use) -> package, versionCode equal aapt2's, no split.
//! 3. `refuse-version`, `refuse-package`: fossify-notes' base + a split linked here by aapt2 whose versionCode (base + 1) or package (base + `.other`) differs -> AOSP's own check refuses (`Inconsistent version` / `Inconsistent package`, PackageManager INSTALL_PARSE_FAILED_BAD_MANIFEST -101), the installer exits non-zero.
//!
//! Measured: the installer's VM start and parse times (its `time-ms` line) and the process wall time of check 1.
//! Break (`--break drop-split`): check 1's dir without its split; the split check must turn red.
//! Default: mutating (stages ART into `<out>/installer-art`, writes `<out>/installer`).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};

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
    /// Deliberate break that must turn its check red: `drop-split`.
    #[arg(long, value_parser = ["drop-split"])]
    r#break: Option<String>,
    /// Print each installer command and its output.
    #[arg(long)]
    verbose: bool,
}

/// One file's identity as `aapt2 dump badging` prints it on its `package:` line.
#[derive(Debug, PartialEq, Eq)]
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

/// What the installer printed (installer.rs in atl-app-launcher: `package`, `version-code`, `split`*, `time-ms vm <n> parse <n>`).
#[derive(Debug, Default)]
struct Parsed {
    package: String,
    version_code: i64,
    splits: Vec<String>,
    vm_ms: u64,
    parse_ms: u64,
}

fn parse_output(stdout: &str) -> Result<Parsed> {
    let mut p = Parsed::default();
    for line in stdout.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "package" => p.package = value.to_owned(),
            "version-code" => p.version_code = value.parse().context("version-code")?,
            "split" => p.splits.push(value.to_owned()),
            "time-ms" => {
                let f: Vec<&str> = value.split(' ').collect();
                ensure!(
                    f.len() == 4 && f[0] == "vm" && f[2] == "parse",
                    "time-ms line {line:?}"
                );
                p.vm_ms = f[1].parse()?;
                p.parse_ms = f[3].parse()?;
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

struct Gate<'a> {
    host: &'a HostArt,
    installer: PathBuf,
    data: PathBuf,
    area: (String, String),
    fails: Vec<String>,
}

impl Gate<'_> {
    fn run(&self, dir: &Path) -> Result<(ArtRun, u128)> {
        let mut cmd = self.host.art_command(
            &self.installer,
            &[],
            &self.data,
            (&self.area.0, &self.area.1),
        );
        cmd.arg(dir);
        let start = Instant::now();
        let r = self.host.run(&mut cmd)?;
        Ok((r, start.elapsed().as_millis()))
    }

    fn fail(&mut self, check: &str, msg: String) {
        println!("{check}: FAIL {msg}");
        self.fails.push(format!("{check}: {msg}"));
    }

    /// Check `name`: the installer over `dir` equals the oracle (`base`'s package and versionCode, `splits` sorted).
    fn expect(&mut self, name: &str, dir: &Path, base: &Badging, splits: &[String]) -> Result<()> {
        let (r, wall) = self.run(dir)?;
        if !r.ok {
            self.fail(name, format!("installer {}: {}", r.status, r.failure()));
            return Ok(());
        }
        let p = parse_output(&r.stdout)?;
        let mut got = p.splits.clone();
        got.sort();
        let mut want = splits.to_vec();
        want.sort();
        let mut bad = Vec::new();
        if p.package != base.package {
            bad.push(format!("package {} != aapt2 {}", p.package, base.package));
        }
        if p.version_code != base.version_code {
            bad.push(format!(
                "versionCode {} != aapt2 {}",
                p.version_code, base.version_code
            ));
        }
        if got != want {
            bad.push(format!("splits {got:?} != aapt2 {want:?}"));
        }
        if bad.is_empty() {
            println!(
                "{name}: PASS package {} versionCode {} splits {got:?}",
                p.package, p.version_code
            );
        } else {
            self.fail(name, bad.join("; "));
        }
        println!(
            "{name}: measured installer vm start {} ms, parse {} ms, process wall {wall} ms",
            p.vm_ms, p.parse_ms
        );
        Ok(())
    }

    /// Check `name`: the installer over `dir` exits non-zero with AOSP's refusal `parse error -101: <marker>...`.
    fn refuse(&mut self, name: &str, dir: &Path, marker: &str) -> Result<()> {
        let (r, _) = self.run(dir)?;
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
    let mut gate = Gate {
        host: &host,
        installer: packages::bin(root, &out, PACKAGE, INSTALLER)?,
        data: work.join("data"),
        area: host.property_area(&work.join("properties"))?,
        fails: Vec::new(),
    };

    // 1. alto: base + its split, from the cache
    let table = test_apks::load(root)?;
    let row = table
        .get(SPLIT_ROW)
        .with_context(|| format!("no row {SPLIT_ROW}"))?;
    let mut base = None;
    let mut splits = Vec::new();
    for pin in row.pins() {
        let file = cached(root, pin)?;
        let b = badging(&tools.aapt2, &file)?;
        match b.split.clone() {
            None => base = Some((b, file)),
            Some(s) => splits.push((s, file)),
        }
    }
    let (alto, alto_base) = base.context("altos-odyssey: no base among its files")?;
    ensure!(!splits.is_empty(), "altos-odyssey: aapt2 names no split");
    let mut files: Vec<(Option<&str>, &Path)> = vec![(None, &alto_base)];
    if args.r#break.as_deref() != Some("drop-split") {
        files.extend(splits.iter().map(|(s, f)| (Some(s.as_str()), f.as_path())));
    }
    let dir = work.join("alto");
    stage_dir(&dir, &files)?;
    let names: Vec<String> = splits.iter().map(|(s, _)| s.clone()).collect();
    gate.expect("alto", &dir, &alto, &names)?;

    // 2. single
    let (_, single_apk) = test_apks::ensure(root, SINGLE_ROW)?;
    let single = badging(&tools.aapt2, &single_apk)?;
    if single.split.is_some() {
        bail!("{SINGLE_ROW}: aapt2 reads a split, not a base");
    }
    let dir = work.join("single");
    stage_dir(&dir, &[(None, &single_apk)])?;
    gate.expect("single", &dir, &single, &[])?;

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
        gate.refuse(name, &dir, marker)?;
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
    use super::parse_output;

    #[test]
    fn reads_the_installer_lines() {
        let p =
            parse_output("package a.b\nversion-code 7\nsplit x\nsplit y\ntime-ms vm 10 parse 3\n")
                .unwrap();
        assert_eq!(
            (
                p.package.as_str(),
                p.version_code,
                p.splits.len(),
                p.vm_ms,
                p.parse_ms
            ),
            ("a.b", 7, 2, 10, 3)
        );
        assert!(parse_output("version-code 7\n").is_err());
    }
}
