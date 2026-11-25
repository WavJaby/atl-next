// SPDX-License-Identifier: MIT
//! The services' install (maintainer option I, 2026-11-13): PackageManagerService's half. The host does the file work (atld: staging, the installer run, the rename, the data dirs; installd's role) over the
//! control socket; here the installer's frames are decoded ([`crate::installed`]) into the package's record: PMS's per-install fields (uid, code and data paths) set on its PackageInfo, its instruction
//! set from `primaryCpuAbi`. Permission states are the PermissionManager's (`packages::Permissions`), made from these records.
//! All or nothing: any session that fails (installer exit, decode, commit) fails the install, and no package is registered. One exception, as PMS: a session refused as a downgrade
//! ([`change_of`]) fails alone, and the installed package is registered as it is (control `Reinstate`).

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
use crate::process_control::{ControlSocket, ScanReply};

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
            .install(session)
            .with_context(|| format!("installing session {session}"))?;
        let parsed = Parsed::read(&frames).with_context(|| format!("session {session}"))?;
        ensure!(
            !records.iter().any(|r| r.name() == parsed.name),
            "two packages named {}",
            parsed.name
        );
        // checked before the commit, so a refused session leaves the installed code as it was (InstallPackageHelper.java :2884-2920 refuses ahead of any file move)
        let version = long_version(&parsed.info);
        let (parsed, version, change) = match change_of(recorded(&parsed.name), version) {
            Ok(change) => (parsed, version, change),
            // PMS fails this install only (the installer's session result); the installed package stays as it was and runs: its code is parsed in place and committed unchanged
            Err(Downgrade { installed, .. }) => {
                let frames = control
                    .reinstate(session, &parsed.name, version, installed)
                    .with_context(|| format!("keeping installed {}", parsed.name))?;
                let kept =
                    Parsed::read(&frames).with_context(|| format!("installed {}", parsed.name))?;
                let kept_version = long_version(&kept.info);
                ensure!(
                    kept.name == parsed.name && kept_version == installed,
                    "installed {}: parses as {} versionCode {kept_version}, recorded {installed}",
                    parsed.name,
                    kept.name
                );
                (kept, installed, Change::Unchanged)
            }
        };
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
    /// The recorded version: nothing.
    Unchanged,
}

/// What an install of `version` does to a package recorded at `recorded` (None: never recorded). Compared as longVersionCode.
/// A lower version is refused, counted: PackageManagerServiceUtils.checkDowngrade :1426-1429 (message text), and isDowngradePermitted :988-992 permits one only on INSTALL_REQUEST_DOWNGRADE, which
/// nothing here sets. The refusal fails that session only ([`install`] keeps the installed package: its code and data stay and it runs, as the installed app does on a device after a refused update).
/// Gap: AOSP's equal-version baseRevisionCode / split revision checks (:1430-1455) need revision codes recorded; not modelled.
pub fn change_of(recorded: Option<i64>, version: i64) -> Result<Change, Downgrade> {
    match recorded {
        None => Ok(Change::New),
        Some(from) if version > from => Ok(Change::Updated { from }),
        Some(installed) if version < installed => {
            let d = Downgrade { version, installed };
            atl_diag::refused(DIAG, "install: INSTALL_FAILED_VERSION_DOWNGRADE");
            log(format_args!("install: {d}"));
            Err(d)
        }
        Some(_) => Ok(Change::Unchanged),
    }
}

/// A session refused as a downgrade: its `version` below the `installed` (recorded) one. Displays checkDowngrade's failure (:1426-1429).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Downgrade {
    pub version: i64,
    pub installed: i64,
}

impl std::fmt::Display for Downgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "INSTALL_FAILED_VERSION_DOWNGRADE: Update version code {} is older than current {}",
            self.version, self.installed
        )
    }
}

/// PackageInfo.getLongVersionCode (PackageInfo.java :93-94): versionCodeMajor << 32 | versionCode (the minor word unsigned).
pub fn long_version(info: &PackageInfo) -> i64 {
    (i64::from(info.version_code_major) << 32) | (i64::from(info.version_code) & 0xffff_ffff)
}

/// PMS's system scan (InitAppsHelper.scanSystemDirs, before the /data packages): every package dir atl-services-types `system_packages::scan` finds under the runtime root `root` (host path), parsed
/// where it lies; flags by location. A dir the installer refuses (its parse verdict) is skipped with a log line and counted: PMS's "Failed to parse" (InstallPackageHelper.installPackagesFromDir).
/// A dir that names a package already scanned is skipped too (PMS's addForInitLI "Failed to scan"). Any other end fails the scan: a killed or verdict-less installer, output that does not decode.
/// AOSP's "Unexpected exception occurred while parsing" fails the start in the same way, not the package. The start stops there: `main` returns the error before `services: ready`.
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
        let frames = match control.scan(&dir)? {
            ScanReply::Staged(frames) => frames,
            ScanReply::Refused(verdict) => {
                skipped += 1;
                log(format_args!(
                    "scan {dir}: skipped: failed to parse: {verdict}"
                ));
                continue;
            }
        };
        let parsed =
            Parsed::read(&frames).with_context(|| format!("scan {dir}: installer output"))?;
        if records.iter().any(|r| r.name() == parsed.name) {
            skipped += 1;
            log(format_args!(
                "scan {dir}: skipped: package {} already scanned",
                parsed.name
            ));
            continue;
        }
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
#[expect(
    clippy::disallowed_types,
    reason = "a test: the installer's end is a real child (sh kills itself or exits with a verdict); no runtime process starts"
)]
mod tests {
    use super::*;

    /// The count of the downgrade refusal (change_of's diag line).
    fn downgrades() -> u64 {
        atl_diag::counts()
            .into_iter()
            .find(|(_, api, _)| api == "IPackageManager.install: INSTALL_FAILED_VERSION_DOWNGRADE")
            .map_or(0, |(_, _, n)| n)
    }

    #[test]
    fn change_compares_longversioncode_against_the_recorded_one() {
        // PackageInfo.getLongVersionCode (PackageInfo.java :93-94): a higher major word beats any minor one
        assert_eq!(change_of(None, 5).unwrap(), Change::New);
        assert_eq!(change_of(Some(4), 5).unwrap(), Change::Updated { from: 4 });
        assert_eq!(change_of(Some(5), 5).unwrap(), Change::Unchanged);
        assert_eq!(
            change_of(Some(0xffff_ffff), 1 << 32).unwrap(),
            Change::Updated { from: 0xffff_ffff }
        );
    }

    #[test]
    fn a_lower_version_is_refused_and_counted() {
        // checkDowngrade :1426-1429: the failure code and message text; minor word 0xffff_ffff of major 0 is below major 1
        let before = downgrades();
        let e = change_of(Some(5), 4).unwrap_err().to_string();
        assert_eq!(
            e,
            "INSTALL_FAILED_VERSION_DOWNGRADE: Update version code 4 is older than current 5"
        );
        assert!(change_of(Some(1 << 32), 0xffff_ffff).is_err());
        assert_eq!(downgrades(), before + 2);
    }

    /// An installer output of `com.example.app` at `version`, its base at `dir`/base.apk.
    fn frames_file(path: &Path, version: i32, dir: &str) {
        use atl_parcelables::parts::Part;
        use atl_services_types::installer_output::{Kind, write_frame};
        use binder::binder_impl::Parcel;
        let mut app = ApplicationInfo::default();
        app.item.package_name = Some("com.example.app".into());
        app.target_sdk_version = 35;
        app.source_dir = Some(format!("{dir}/base.apk"));
        let info = PackageInfo {
            package_name: Some("com.example.app".into()),
            version_code: version,
            application_info: Some(app),
            ..Default::default()
        };
        let mut p = Parcel::new();
        info.write(&mut p.borrowed()).expect("encode");
        let mut fields = Parcel::new();
        let none: Option<Vec<Option<String>>> = Some(vec![]);
        fields.write(&none).expect("required");
        fields.write(&none).expect("optional");
        fields.write(&false).expect("forceQueryable");
        fields.write(&none).expect("query packages");
        fields.write(&none).expect("query providers");
        atl_parcelables::parts::w_typed_list::<atl_parcelables::android::content::Intent>(
            &mut fields.borrowed(),
            &Some(vec![]),
        )
        .expect("query intents");
        fields.write(&none).expect("protected");
        let mut out = Vec::new();
        write_frame(&mut out, Kind::Package, &p.marshal().expect("marshal")).expect("frame");
        write_frame(
            &mut out,
            Kind::PackageFields,
            &fields.marshal().expect("marshal"),
        )
        .expect("frame");
        std::fs::write(path, out).expect("frames");
    }

    /// A session at version 1 over the installed (recorded) 2: refused, yet the install goes on with the installed package: atld is asked to reinstate it (both versionCodes), its in-place parse
    /// is committed under its own names, and the report is version 2 Unchanged (no package broadcast). A refusal of any other kind keeps failing (installer errors stay errors).
    #[test]
    fn a_refused_downgrade_keeps_the_installed_package() {
        use atl_broker::control::{self, Request, Response};
        use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
        let tmp = std::env::temp_dir().join(format!("atl-install-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("tmp");
        let (session_frames, kept_frames) = (tmp.join("0.frames"), tmp.join("0-kept.frames"));
        frames_file(&session_frames, 1, &format!("{DATA_APP}/vmdl0.tmp"));
        frames_file(&kept_frames, 2, &format!("{DATA_APP}/com.example.app"));
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair");
        let (sf, kf) = (
            session_frames.to_string_lossy().into_owned(),
            kept_frames.to_string_lossy().into_owned(),
        );
        let server = std::thread::spawn(move || {
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let record = std::sync::Arc::clone(&seen);
            let answer = move |r: Request| {
                let answer = match &r {
                    Request::Install { session: 0, .. } => Response::Staged { frames: sf.clone() },
                    Request::Reinstate {
                        session: 0,
                        package,
                        version: 1,
                        installed: 2,
                        ..
                    } if package == "com.example.app" => Response::Staged { frames: kf.clone() },
                    Request::Commit {
                        session: 0, names, ..
                    } if names.len() == 1 && names[0].staged == "base.apk" => Response::Committed,
                    other => Response::Error {
                        message: format!("unexpected {other:?}"),
                    },
                };
                record.lock().expect("seen").push(
                    format!("{r:?}")
                        .split([' ', '{', '('])
                        .next()
                        .unwrap_or("")
                        .to_owned(),
                );
                answer
            };
            control::serve_lanes(theirs, answer.clone(), answer).expect("serve_lanes");
            std::sync::Arc::try_unwrap(seen)
                .expect("serve_lanes dropped its handlers")
                .into_inner()
                .expect("seen")
        });
        let control = ControlSocket::new(ours);
        let config = Config {
            abis: vec![SupportedAbi {
                abi: "x86_64".into(),
                isa: "x86_64".into(),
            }],
        };
        let (records, reports) = install(
            &control,
            &[0],
            &mut AppIds::new(),
            &config,
            Vec::new(),
            &|_| Some(2),
        )
        .expect("the install goes on with the installed package");
        drop(control);
        assert_eq!(
            server.join().expect("server"),
            ["Install", "Reinstate", "Commit"]
        );
        assert_eq!(records.len(), 1);
        assert_eq!(long_version(&records[0].info), 2);
        assert_eq!(
            reports,
            [Reported {
                package: "com.example.app".into(),
                uid: reports[0].uid,
                version: 2,
                change: Change::Unchanged,
            }]
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A partition root with one preinstalled package dir `product/app/<name>` (its base APK empty: the installer is the child under test, not this file).
    fn partition(tag: &str, name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("atl-scan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("product/app").join(name);
        std::fs::create_dir_all(&dir).expect("package dir");
        std::fs::write(dir.join(format!("{name}.apk")), b"").expect("base apk");
        root
    }

    /// `scan` of `root` against a fake atld whose installer is a real child running `script` under sh: the child's end goes through InstallerEnd::of and its answer
    /// (scan_reply) over the real control protocol. A parsed end answers the frames file `frames`.
    fn scan_with_installer(root: &Path, script: &str, frames: &str) -> Result<Vec<PackageRecord>> {
        use atl_broker::control::{self, Request, Response};
        use atl_broker::installer_end::InstallerEnd;
        use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair");
        let (script, frames) = (script.to_owned(), frames.to_owned());
        let server = std::thread::spawn(move || {
            let answer = move |r: Request| match r {
                Request::Scan { dir } => {
                    let out = std::process::Command::new("/bin/sh")
                        .args(["-c", &script])
                        .output()
                        .expect("sh");
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    InstallerEnd::of(&out.status, &stderr, Path::new("atl-installer"))
                        .scan_reply(&dir, &frames)
                }
                other => Response::Error {
                    message: format!("unexpected {other:?}"),
                },
            };
            control::serve_lanes(theirs, answer.clone(), answer).expect("serve_lanes");
        });
        let control = ControlSocket::new(ours);
        let config = Config {
            abis: vec![SupportedAbi {
                abi: "x86_64".into(),
                isa: "x86_64".into(),
            }],
        };
        let result = scan(&control, root, &mut AppIds::new(), &config);
        drop(control);
        server.join().expect("server");
        result
    }

    #[test]
    fn a_killed_installer_fails_the_scan_and_names_the_package() {
        let root = partition("killed", "Killed");
        // a real SIGSYS end: the signal a seccomp kill leaves
        let err = match scan_with_installer(&root, "kill -s SYS $$", "") {
            Err(e) => e,
            Ok(_) => panic!("a killed installer is not a skipped package"),
        };
        let text = format!("{err:#}");
        assert!(text.contains("Killed"), "the package is named: {text}");
        assert!(text.contains("atl-installer exited"), "{text}");
        assert!(
            text.contains("type=1326"),
            "a SIGSYS end points at its audit record: {text}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_parse_verdict_still_skips_the_package() {
        let root = partition("refused", "Refused");
        let records = scan_with_installer(
            &root,
            "echo 'atl-installer: parse error -2: not an APK' >&2; exit 1",
            "",
        )
        .expect("a parse failure is skipped and the start goes on");
        assert!(records.is_empty(), "the refused package is not registered");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn output_that_does_not_decode_fails_the_scan() {
        let root = partition("garbage", "Garbage");
        let frames = root.join("frames");
        std::fs::write(&frames, b"not an installer's frames").expect("frames");
        let err = match scan_with_installer(&root, "exit 0", &frames.to_string_lossy()) {
            Err(e) => e,
            Ok(_) => panic!("an undecodable exit-0 output is not a skipped package"),
        };
        let text = format!("{err:#}");
        assert!(text.contains("Garbage"), "the package is named: {text}");
        let _ = std::fs::remove_dir_all(&root);
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
