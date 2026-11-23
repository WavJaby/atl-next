// SPDX-License-Identifier: MIT
//! The services' install (maintainer option I, 2026-11-13): PackageManagerService's half. The host does the file work (atld: staging, the installer run, the rename, the data dirs; installd's role) over the
//! control socket; here the installer's frames are decoded ([`crate::installed`]) into the package's record: PMS's per-install fields (uid, code and data paths) set on its PackageInfo, its instruction
//! set from `primaryCpuAbi`. Permission states are the PermissionManager's (`packages::Permissions`), made from these records.
//! All or nothing: any session that fails (installer exit, decode, commit) fails the install, and no package is registered.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use atl_android_paths::{DATA_APP, DATA_USER, DATA_USER_DE};
use atl_broker::control::StagedName;
use atl_parcelables::android::content::pm::{ApplicationInfo, PackageInfo};
use atl_services_types::native_libs::{SupportedAbi, instruction_set};
use atl_services_types::system_packages;

use crate::activity::log;
use crate::app_id::AppIds;
use crate::installed::Installed;
use crate::packages::{InstallFields, PackageRecord};
use crate::process_control::ControlSocket;

/// The diag interface the install's counts go under (PackageManagerService's binder name).
const DIAG: &str = "IPackageManager";

/// What the install takes besides the session ids.
pub struct Config {
    /// The device's ABIs, preferred first: primaryCpuAbi -> instruction set.
    pub abis: Vec<SupportedAbi>,
}

/// Installs every session the host holds files for, in order (PMS's boot scan of installed packages), after the system packages `records` (the scan's): every record the services answer from. Each
/// package's uid comes from `ids` (AppIdSettingMap). A session named like a scanned package fails the start (updated system packages come with persisted /data packages). `recorded` is each package's
/// longVersionCode as the last install recorded it (package_state); the reports say what this install did, for the start-time package broadcasts (broadcasts_am.rs package_broadcasts).
pub fn install(
    control: &ControlSocket,
    sessions: &[u32],
    ids: &mut AppIds,
    c: &Config,
    mut records: Vec<PackageRecord>,
    recorded: &dyn Fn(&str) -> Option<i64>,
) -> Result<(Vec<PackageRecord>, Vec<Reported>)> {
    ensure!(!c.abis.is_empty(), "no device ABI");
    let mut reports = Vec::with_capacity(sessions.len());
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
        control
            .commit(session, &parsed.name, uid, names)
            .with_context(|| format!("committing {}", parsed.name))?;
        let version = long_version(&parsed.info);
        let change = change_of(recorded(&parsed.name), version);
        reports.push(Reported {
            package: parsed.name.clone(),
            uid,
            version,
            change,
        });
        let code_dir = format!("{DATA_APP}/{}", parsed.name);
        records.push(parsed.record(uid, &code_dir, true, (0, 0), c));
    }
    Ok((records, reports))
}

/// One session's install: the package, its uid and longVersionCode, and what the install did to it against the recorded version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub package: String,
    pub uid: u32,
    pub version: i64,
    pub change: Change,
}

/// What an install does to a package against its recorded version (see [`change_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Not recorded: PACKAGE_ADDED.
    New,
    /// A higher version than `from`: PACKAGE_ADDED (EXTRA_REPLACING), PACKAGE_REPLACED, MY_PACKAGE_REPLACED to the package (BroadcastHelper.java sendPostInstallBroadcasts :600-720).
    Updated { from: i64 },
    /// A lower version than `from`: AOSP refuses this install (INSTALL_FAILED_VERSION_DOWNGRADE); here it installs as before, with no replace broadcast (counted).
    Downgraded { from: i64 },
    /// The recorded version: nothing.
    Unchanged,
}

/// What an install of `version` does to a package recorded at `recorded` (None: never recorded). Compared as longVersionCode.
pub fn change_of(recorded: Option<i64>, version: i64) -> Change {
    match recorded {
        None => Change::New,
        Some(from) if version > from => Change::Updated { from },
        Some(from) if version < from => {
            atl_diag::answered(
                DIAG,
                "install: lower version than recorded: no replace broadcasts",
            );
            Change::Downgraded { from }
        }
        Some(_) => Change::Unchanged,
    }
}

/// PackageInfo.getLongVersionCode (PackageInfo.java :93-94): versionCodeMajor << 32 | versionCode (the minor word unsigned).
pub fn long_version(info: &PackageInfo) -> i64 {
    (i64::from(info.version_code_major) << 32) | (i64::from(info.version_code) & 0xffff_ffff)
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
    let found =
        system_packages::scan(root).with_context(|| format!("scanning {}", root.display()))?;
    let mut skipped = found.skipped.len();
    for s in &found.skipped {
        log(format_args!(
            "scan {}: skipped: not a package dir",
            s.display()
        ));
    }
    let mut records: Vec<PackageRecord> = Vec::new();
    for d in &found.dirs {
        let dir = d.android_path();
        let parsed = control
            .scan(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|frames| Parsed::read(&frames));
        let parsed = match parsed {
            Ok(p) if records.iter().any(|r| r.name() == p.name) => {
                Err(anyhow::anyhow!("package {} already scanned", p.name))
            }
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
        // a shared user's package runs under the shared user's id (ScanPackageUtils: the PackageSetting's appId is its SharedUserSetting's)
        let uid = match parsed.info.shared_user_id.as_deref() {
            Some(shared) => ids.shared_user(shared),
            None => ids.acquire(),
        }
        .context("no application id left")?;
        control
            .scan_commit(&dir, &parsed.name, uid)
            .with_context(|| format!("committing scanned {}", parsed.name))?;
        records.push(parsed.record(uid, &dir, false, (d.flags, d.private_flags), c));
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

    /// The package's record: PMS's per-package fields (uid, code and data paths, flags by location), its instruction set from `primaryCpuAbi`.
    fn record(
        self,
        uid: u32,
        code_dir: &str,
        uniform_names: bool,
        (flags, private_flags): (i32, i32),
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
        // targetSdk decides the implicit org.apache.http.legacy (installed.rs with_implicit_libraries)
        let target_sdk = info
            .application_info
            .as_ref()
            .map_or(0, |a| a.target_sdk_version);
        PackageRecord {
            info,
            intents: installed.intents,
            libraries: crate::installed::with_implicit_libraries(installed.libraries, target_sdk),
            force_queryable: installed.force_queryable,
            queries: installed.queries,
            // ScanPackageUtils.java:936-939: a non-system package cannot mark any broadcast as protected
            protected_broadcasts: if flags & crate::packages::FLAG_SYSTEM != 0 {
                installed.protected_broadcasts
            } else {
                Vec::new()
            },
            uid,
            instruction_set: isa,
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

    /// The count of the downgrade refusal (change_of's diag line).
    fn downgrades() -> u64 {
        atl_diag::counts()
            .into_iter()
            .find(|(_, api, _)| {
                api == "IPackageManager.install: lower version than recorded: no replace broadcasts"
            })
            .map_or(0, |(_, _, n)| n)
    }

    #[test]
    fn change_compares_longversioncode_against_the_recorded_one() {
        // PackageInfo.getLongVersionCode (PackageInfo.java :93-94): a higher major word beats any minor one
        assert_eq!(change_of(None, 5), Change::New);
        assert_eq!(change_of(Some(4), 5), Change::Updated { from: 4 });
        let before = downgrades();
        assert_eq!(change_of(Some(5), 4), Change::Downgraded { from: 5 });
        assert_eq!(downgrades(), before + 1);
        assert_eq!(change_of(Some(5), 5), Change::Unchanged);
        assert_eq!(
            change_of(Some(0xffff_ffff), 1 << 32),
            Change::Updated { from: 0xffff_ffff }
        );
    }

    #[test]
    fn long_version_is_major_then_unsigned_minor() {
        let info = PackageInfo {
            version_code: -1,
            version_code_major: 1,
            ..PackageInfo::default()
        };
        assert_eq!(long_version(&info), (1_i64 << 32) | 0xffff_ffff);
        let plain = PackageInfo {
            version_code: 2,
            ..PackageInfo::default()
        };
        assert_eq!(long_version(&plain), 2);
    }

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
