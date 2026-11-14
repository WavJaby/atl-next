// SPDX-License-Identifier: MIT
//! The services' install (owner option I, 2026-11-13): PackageManagerService's half. The host does the file work (atld: staging, the installer run, the rename, the data dirs; installd's role) over the
//! control socket; here the installer's frames are decoded ([`crate::installed`]) into the package's record: PMS's per-install fields (uid, code and data paths) set on its PackageInfo, its instruction
//! set from `primaryCpuAbi`, the GL driver from the owner table and the host's ES1 fact. Permission states are the PermissionManager's (`packages::Permissions`), made from these records.
//! All or nothing: any session that fails (installer exit, decode, commit) fails the install, and no package is registered.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use atl_android_paths::{DATA_APP, DATA_USER, DATA_USER_DE};
use atl_broker::control::StagedName;
use atl_parcelables::android::content::pm::{ApplicationInfo, PackageInfo};
use atl_services_types::native_libs::{SupportedAbi, instruction_set};
use atl_services_types::{gl_driver, system_packages};

use crate::activity::log;
use crate::app_id::AppIds;
use crate::installed::Installed;
use crate::packages::{InstallFields, PackageRecord};
use crate::process_control::ControlSocket;

/// What the install takes besides the session ids.
pub struct Config {
    /// The device's ABIs, preferred first: primaryCpuAbi -> instruction set.
    pub abis: Vec<SupportedAbi>,
    /// Test only: every package gets the native GL driver (the GL selection gate's break).
    pub no_gl_selection: bool,
}

/// Installs every session the host holds files for, in order (PMS's boot scan of installed packages), after the system packages `records` (the scan's): every record the services answer from. Each
/// package's uid comes from `ids` (AppIdSettingMap). A session named like a scanned package fails the start (updated system packages come with persisted /data packages).
pub fn install(
    control: &ControlSocket,
    sessions: &[u32],
    ids: &mut AppIds,
    c: &Config,
    mut records: Vec<PackageRecord>,
) -> Result<Vec<PackageRecord>> {
    ensure!(!c.abis.is_empty(), "no device ABI");
    let owner = gl_driver::owner_table(gl_driver::OWNER_TABLE).map_err(anyhow::Error::msg)?;
    for &session in sessions {
        let uid = ids.acquire().context("no application id left")?;
        let frames = control
            .install(session, uid)
            .with_context(|| format!("installing session {session}"))?;
        let parsed = Parsed::read(&frames).with_context(|| format!("session {session}"))?;
        ensure!(
            !records.iter().any(|r| r.name() == parsed.name),
            "two packages named {}",
            parsed.name
        );
        let names = parsed
            .info
            .application_info
            .as_ref()
            .with_context(|| format!("session {session}: no ApplicationInfo"))
            .and_then(staged_names)
            .with_context(|| format!("session {session}"))?;
        let needs_es1 = control
            .commit(session, &parsed.name, uid, names)
            .with_context(|| format!("committing {}", parsed.name))?;
        let code_dir = format!("{DATA_APP}/{}", parsed.name);
        records.push(parsed.record(uid, &code_dir, true, (0, 0), needs_es1, &owner, c));
    }
    Ok(records)
}

/// PMS's system scan (InitAppsHelper.scanSystemDirs, before the /data packages): every package dir atl-services-types `system_packages::scan` finds under the runtime root `root` (host path), parsed
/// where it lies; flags by location. A dir that fails to parse, or names a package already scanned, is skipped with a log line and counted (PMS logs and skips per package); the start goes on.
/// An appId is taken only for an accepted package, so a skipped one moves no id.
pub fn scan(
    control: &ControlSocket,
    root: &Path,
    ids: &mut AppIds,
    c: &Config,
) -> Result<Vec<PackageRecord>> {
    ensure!(!c.abis.is_empty(), "no device ABI");
    let owner = gl_driver::owner_table(gl_driver::OWNER_TABLE).map_err(anyhow::Error::msg)?;
    let found = system_packages::scan(root).with_context(|| format!("scanning {}", root.display()))?;
    let mut skipped = found.skipped.len();
    for s in &found.skipped {
        log(format_args!("scan {}: skipped: not a package dir", s.display()));
    }
    let mut records: Vec<PackageRecord> = Vec::new();
    for d in &found.dirs {
        let dir = d.android_path();
        let parsed = control
            .scan(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|frames| Parsed::read(&frames));
        let parsed = match parsed {
            Ok(p) if records.iter().any(|r| r.name() == p.name) => Err(anyhow::anyhow!(
                "package {} already scanned",
                p.name
            )),
            other => other,
        };
        let parsed = match parsed {
            Ok(p) => p,
            Err(e) => {
                skipped += 1;
                log(format_args!("scan {dir}: skipped: {e:#}"));
                continue;
            }
        };
        let uid = ids.acquire().context("no application id left")?;
        let needs_es1 = control
            .scan_commit(&dir, &parsed.name, uid)
            .with_context(|| format!("committing scanned {}", parsed.name))?;
        records.push(parsed.record(
            uid,
            &dir,
            false,
            (d.flags, d.private_flags),
            needs_es1,
            &owner,
            c,
        ));
    }
    log(format_args!(
        "scan: {} system packages, {skipped} skipped",
        records.len()
    ));
    Ok(records)
}

/// One installer run's decoded frames, and the package name they give.
struct Parsed {
    name: String,
    info: PackageInfo,
    installed: Installed,
}

impl Parsed {
    fn read(frames: &str) -> Result<Parsed> {
        let bytes = std::fs::read(frames).with_context(|| format!("reading {frames}"))?;
        let mut installed =
            crate::installed::decode(&bytes).map_err(|e| anyhow::anyhow!("{frames}: {e}"))?;
        let info = std::mem::take(&mut installed.package);
        let name = info.package_name.clone().context("no package name")?;
        Ok(Parsed {
            name,
            info,
            installed,
        })
    }

    /// The package's record: PMS's per-package fields (uid, code and data paths, flags by location), its instruction set from `primaryCpuAbi`, the GL driver.
    #[allow(clippy::too_many_arguments)]
    fn record(
        self,
        uid: u32,
        code_dir: &str,
        uniform_names: bool,
        (flags, private_flags): (i32, i32),
        needs_es1: bool,
        owner: &BTreeMap<String, gl_driver::GlDriver>,
        c: &Config,
    ) -> PackageRecord {
        let Parsed {
            name,
            mut info,
            installed,
        } = self;
        let primary = info
            .application_info
            .as_ref()
            .and_then(|a| a.primary_cpu_abi.clone());
        let gl_driver = if c.no_gl_selection {
            gl_driver::GlDriver::Native
        } else {
            gl_driver::select(owner, &name, needs_es1)
        };
        let isa = instruction_set(primary.as_deref(), &c.abis).to_owned();
        InstallFields {
            uid,
            code_dir,
            isa: &isa,
            data_dir: &format!("{DATA_USER}/0/{name}"),
            device_protected_data_dir: &format!("{DATA_USER_DE}/0/{name}"),
            uniform_names,
            flags,
            private_flags,
        }
        .apply(&mut info);
        log(format_args!(
            "install {name}: uid {uid}, code {code_dir}, flags {flags:#x} private {private_flags:#x}, primary-abi {}, isa {isa}",
            primary.as_deref().unwrap_or("none")
        ));
        PackageRecord {
            info,
            intents: installed.intents,
            libraries: installed.libraries,
            uid,
            instruction_set: isa,
            gl_driver,
        }
    }
}

/// PackageInstallerSession's uniform naming (validateApkInstallLocked -> ApkLiteParseUtils.splitNameToFileName: `base.apk`, `split_<split name>.apk`) of the files the installer parsed, read from the
/// parse's own paths: the base's `sourceDir`, split i's `splitSourceDirs[i]` with `splitNames[i]` (one order, PackageLite). A split is named by its parse, never by its session file name.
fn staged_names(app: &ApplicationInfo) -> Result<Vec<StagedName>> {
    let file = |path: &Option<String>| -> Result<String> {
        let path = path.as_deref().context("a parsed APK without a path")?;
        Ok(Path::new(path)
            .file_name()
            .with_context(|| format!("{path}: no file name"))?
            .to_string_lossy()
            .into_owned())
    };
    let mut names = vec![StagedName {
        staged: file(&app.source_dir)?,
        name: "base.apk".to_owned(),
    }];
    let splits = app.split_names.as_deref().unwrap_or_default();
    let paths = app.split_source_dirs.as_deref().unwrap_or_default();
    ensure!(
        splits.len() == paths.len(),
        "{} split names but {} split paths",
        splits.len(),
        paths.len()
    );
    for (split, path) in splits.iter().zip(paths) {
        let split = split.as_deref().context("a split without a name")?;
        names.push(StagedName {
            staged: file(path)?,
            name: format!("split_{split}.apk"),
        });
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parse of a dir staged as the pm shell names files (source names) whose split name differs from every file name: the renames follow the parse.
    #[test]
    fn names_come_from_the_parsed_split_names() {
        let staged = |f: &str| Some(format!("{DATA_APP}/vmdl0.tmp/{f}"));
        let app = ApplicationInfo {
            source_dir: staged("0d04.apk"),
            split_names: Some(vec![
                Some("config.arm64_v8a".into()),
                Some("UnityDataAssetPack".into()),
            ]),
            split_source_dirs: Some(vec![staged("1556.apk"), staged("2319.apk")]),
            ..Default::default()
        };
        let got = staged_names(&app).expect("names");
        let want = [
            ("0d04.apk", "base.apk"),
            ("1556.apk", "split_config.arm64_v8a.apk"),
            ("2319.apk", "split_UnityDataAssetPack.apk"),
        ];
        assert_eq!(got.len(), want.len());
        for (g, (staged, name)) in got.iter().zip(want) {
            assert_eq!((g.staged.as_str(), g.name.as_str()), (staged, name));
        }
    }

    /// Break: a split path list shorter than the names (a parse the services cannot pair) is refused, not half-renamed.
    #[test]
    fn unpaired_split_paths_are_refused() {
        let app = ApplicationInfo {
            source_dir: Some(format!("{DATA_APP}/vmdl0.tmp/a.apk")),
            split_names: Some(vec![Some("x".into())]),
            split_source_dirs: None,
            ..Default::default()
        };
        assert!(staged_names(&app).is_err());
    }
}
