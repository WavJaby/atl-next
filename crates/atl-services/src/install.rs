// SPDX-License-Identifier: MIT
//! The services' install (maintainer option I, 2026-11-13): PackageManagerService's half. The host does the file work (atld: staging, the installer run, the rename, the data dirs; installd's role) over the
//! control socket; here the installer's frames are decoded ([`crate::installed`]) into the package's record: PMS's per-install fields (uid, code and data paths) set on its PackageInfo, its instruction
//! set from `primaryCpuAbi`. Permission states are the PermissionManager's (`packages::Permissions`), made from these records.
//! All or nothing: any session that fails (installer exit, decode, commit) fails the install, and no package is registered. One exception, as PMS: a session refused as a downgrade
//! ([`change_of`]) fails alone, and the installed package is registered as it is (control `Reinstate`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use atl_android_paths::{DATA_APP, DATA_USER, DATA_USER_DE};
use atl_broker::control::StagedName;
use atl_parcelables::android::content::pm::{ApplicationInfo, PackageInfo};
use atl_services_types::native_libs::{SupportedAbi, instruction_set};
use atl_services_types::permission::TARGET_SDK_RUNTIME_PERMISSIONS;
use atl_services_types::system_packages;

use crate::activity::log;
use crate::app_id::AppIds;
use crate::installed::Installed as Decoded;
use crate::package::same_signers;
use crate::package_state::PackageUserStates;
use crate::packages::{InstallFields, PackageRecord, PackageTable, Packages, Permissions};
use crate::process_control::{ControlSocket, ScanReply};

/// The diag interface the install's counts go under (PackageManagerService's binder name).
const DIAG: &str = "IPackageManager";

/// What the install takes besides the session ids.
pub struct Config {
    /// The device's ABIs, preferred first: primaryCpuAbi -> instruction set.
    pub abis: Vec<SupportedAbi>,
}

/// Where a session comes from: boot's (`Boot`: a downgrade reinstates the installed code) or a running install (`Session`: a downgrade is refused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// sessions of this start: stand in for the boot scan until P7
    Boot,
    /// a session on running services
    Session,
}

/// One accepted install: code committed, not yet published.
pub struct Installed {
    pub record: PackageRecord,
    pub reported: Reported,
    /// the package's freeze, kept to the publish (dropped last there): the package starts again only with its version recorded
    pub frozen: Box<dyn Send>,
}

/// PackageManagerException: an INSTALL_* name and its message (Display `NAME: message`, PackageManager.installStatusToString :10650).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
}

/// PackageFreezer (freezePackage, :65-97): stops the package's processes and freezes the package until the returned guard drops; a frozen package starts nothing. ActivityManager implements it.
pub trait PackageStops: Send + Sync {
    fn freeze(&self, package: &str) -> Box<dyn Send>;
}

/// The one install path (PMS installPackagesTraced): boot's sessions now, a running install later.
pub struct Installer {
    control: Arc<ControlSocket>,
    config: Config,
    /// the install lock: the app id pool, held from `install_one` to the end of `publish`
    ids: Mutex<AppIds>,
    table: Arc<PackageTable>,
    permissions: Arc<Permissions>,
    state: Arc<PackageUserStates>,
    stops: Arc<dyn PackageStops>,
}

impl Installer {
    pub fn new(
        control: Arc<ControlSocket>,
        config: Config,
        ids: AppIds,
        table: Arc<PackageTable>,
        permissions: Arc<Permissions>,
        state: Arc<PackageUserStates>,
        stops: Arc<dyn PackageStops>,
    ) -> Self {
        Installer {
            control,
            config,
            ids: Mutex::new(ids),
            table,
            permissions,
            state,
            stops,
        }
    }

    /// Boot's sessions as one batch: a refusal fails the start (the boot scan's refusals always did), so nothing is published.
    pub fn boot(&self, sessions: &[u32]) -> Result<Published> {
        ensure!(!self.config.abis.is_empty(), "no device ABI");
        let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
        let mut batch = Vec::with_capacity(sessions.len());
        for &session in sessions {
            let accepted = self
                .install_one(&mut ids, session, Origin::Boot, &batch)
                .map_err(|f| anyhow::Error::msg(f.message))?;
            batch.push(accepted);
        }
        Ok(self.publish(batch))
    }

    /// One session up to its commit: control `install`, then `check` (the refusals), the freeze, control `commit`. A refusal drops the staged session (control `abandon`); a failed
    /// commit is atld's to clean up (its staging dir and entry go) and releases a fresh uid. The caller holds the install lock; nothing is published.
    fn install_one(
        &self,
        ids: &mut AppIds,
        session: u32,
        origin: Origin,
        pending: &[Installed],
    ) -> Result<Installed, Failure> {
        let frames = self
            .control
            .install(session)
            .with_context(|| format!("installing session {session}"))
            .map_err(|e| failure(INTERNAL, e))?;
        let Checked {
            parsed,
            version,
            change,
            names,
            uid,
            fresh,
        } = self
            .check(ids, session, &frames, origin, pending)
            .map_err(|f| self.abandon(session, f))?;
        let name = parsed.name.clone();
        // PackageFreezer (commitInstallPackages :1373-1386, after reconcile): frozen from here to the publish, so no start of the package crosses the swap; a failed commit unfreezes at the return
        let frozen = self.stops.freeze(&name);
        if let Err(e) = self
            .control
            .commit(session, &name, uid, names)
            .with_context(|| format!("committing {name}"))
        {
            if fresh {
                ids.release(uid);
            }
            return Err(failure(INTERNAL, e));
        }
        let code_dir = format!("{DATA_APP}/{name}");
        let record = parsed.record(uid, &code_dir, true, (0, 0), &self.config);
        Ok(Installed {
            record,
            reported: Reported {
                package: name,
                uid,
                version,
                change,
            },
            frozen,
        })
    }

    /// The refusals of one staged session, each a [`refusal`]; the session takes no uid unless it passes. A Session of an installed name is an update ([`Installer::update`]);
    /// any other name must be new (a Boot session also refuses a name the table holds: boot's batch is the only source of installed packages there).
    fn check(
        &self,
        ids: &mut AppIds,
        session: u32,
        frames: &str,
        origin: Origin,
        pending: &[Installed],
    ) -> Result<Checked, Failure> {
        let parsed = Parsed::read(frames)
            .with_context(|| format!("session {session}"))
            .map_err(|e| refusal(INVALID_APK, e))?;
        let name = parsed.name.clone();
        let snapshot = self.table.snapshot();
        let old = snapshot.by_name(&name);
        if pending.iter().any(|i| i.record.name() == name)
            || (origin == Origin::Boot && old.is_some())
        {
            return Err(refusal_text(
                DUPLICATE,
                format!("two packages named {name}"),
            ));
        }
        if let (Origin::Session, Some(old)) = (origin, old) {
            return self.update(session, parsed, old);
        }
        // checked before the commit, so a refused session leaves the installed code as it was (InstallPackageHelper.java :2884-2920 refuses ahead of any file move)
        let version = long_version(&parsed.info);
        let (parsed, version, change) = match change_of(self.state.version_code(&name), version) {
            Ok(change) => (parsed, version, change),
            Err(Downgrade { installed, .. }) if origin == Origin::Session => {
                return Err(Failure {
                    code: DOWNGRADE,
                    message: format!(
                        "Update version code {version} is older than current {installed}"
                    ),
                });
            }
            // PMS fails this install only (the installer's session result); the installed package stays as it was and runs: its code is parsed in place and committed unchanged
            Err(Downgrade { installed, .. }) => {
                let frames = self
                    .control
                    .reinstate(session, &name, version, installed)
                    .with_context(|| format!("keeping installed {name}"))
                    .map_err(|e| failure(INTERNAL, e))?;
                let kept = Parsed::read(&frames)
                    .with_context(|| format!("installed {name}"))
                    .map_err(|e| failure(INTERNAL, e))?;
                let kept_version = long_version(&kept.info);
                if kept.name != name || kept_version != installed {
                    return Err(failure(
                        INTERNAL,
                        anyhow::anyhow!(
                            "installed {name}: parses as {} versionCode {kept_version}, recorded {installed}",
                            kept.name
                        ),
                    ));
                }
                (kept, installed, Change::Unchanged)
            }
        };
        let names = renames(&parsed, session)?;
        let uid = ids
            .acquire()
            .ok_or_else(|| refusal_text(INTERNAL, "no application id left".into()))?;
        Ok(Checked {
            parsed,
            version,
            change,
            names,
            uid,
            fresh: true,
        })
    }

    /// A running install of `old`, the installed package of its name (P4c, PMS's replace): its refusals in the table's order, then the update. The uid is `old`'s: the pool gives none.
    /// An equal version is an update too (Boot's equal version stays Unchanged, in [`Installer::check`]).
    fn update(
        &self,
        session: u32,
        parsed: Parsed,
        old: &PackageRecord,
    ) -> Result<Checked, Failure> {
        let name = parsed.name.clone();
        if old.app().flags & crate::packages::FLAG_SYSTEM != 0 {
            return Err(refusal_text(
                INTERNAL,
                format!("updating system package {name} is not modelled"),
            ));
        }
        let version = long_version(&parsed.info);
        // the recorded version (P7 takes it from the code); a lower one is a Downgrade, counted by change_of
        let from = self
            .state
            .version_code(&name)
            .unwrap_or_else(|| long_version(&old.info));
        if let Err(Downgrade { installed, .. }) = change_of(Some(from), version) {
            return Err(Failure {
                code: DOWNGRADE,
                message: format!("Update version code {version} is older than current {installed}"),
            });
        }
        let was = old.app().target_sdk_version;
        let now = parsed
            .info
            .application_info
            .as_ref()
            .map_or(0, |a| a.target_sdk_version);
        if was >= TARGET_SDK_RUNTIME_PERMISSIONS && now < TARGET_SDK_RUNTIME_PERMISSIONS {
            return Err(refusal_text(
                PERMISSION_MODEL,
                format!(
                    "Package {name} new target SDK {now} doesn't support runtime permissions but the old target SDK {was} does."
                ),
            ));
        }
        // Gap (PM 2026-11-26): a rotated key is refused; AOSP takes it by its signing lineage (SigningDetails.checkCapability :625), not modelled. Counted under UPDATE_INCOMPATIBLE.
        if !same_signers(
            old.info.signing_info.as_ref(),
            parsed.info.signing_info.as_ref(),
        ) {
            return Err(refusal_text(
                UPDATE_INCOMPATIBLE,
                format!("Existing package {name} signatures do not match newer version; ignoring!"),
            ));
        }
        let names = renames(&parsed, session)?;
        Ok(Checked {
            parsed,
            version,
            change: Change::Updated { from },
            names,
            uid: old.uid,
            fresh: false,
        })
    }

    /// Refused staged `session` (`f`): atld drops the session (control `abandon`). A failed abandon is logged; the refusal stands.
    fn abandon(&self, session: u32, f: Failure) -> Failure {
        if let Err(e) = self.control.abandon(session) {
            log(format_args!(
                "install session {session}: abandon failed: {e}"
            ));
        }
        f
    }

    /// The batch becomes visible at once, under the install lock: its permission states, then the table, then each recorded version, its component overrides pruned, and the state
    /// saved. Before the table publish nothing of it was visible.
    fn publish(&self, batch: Vec<Installed>) -> Published {
        let snapshot = self.table.snapshot();
        if batch.is_empty() {
            return Published {
                reports: Vec::new(),
                audience: Audience {
                    before: Arc::clone(&snapshot),
                    after: snapshot,
                },
            };
        }
        let names: Vec<String> = batch.iter().map(|i| i.record.name().to_owned()).collect();
        let mut frozen = Vec::with_capacity(batch.len());
        let (records, reports): (Vec<PackageRecord>, Vec<Reported>) = batch
            .into_iter()
            .map(|i| {
                frozen.push(i.frozen);
                (i.record, i.reported)
            })
            .unzip();
        let next = snapshot.with_records(records);
        self.permissions.packages_added(&next, &names);
        self.table.publish(next);
        // restoreComponentSettings (IPH :590-596): an override of a component the new version no longer has goes
        let published = self.table.snapshot();
        for r in &reports {
            self.state.set_version_code(&r.package, r.version);
            if let Some(p) = published.by_name(&r.package) {
                self.state
                    .retain_components(&r.package, |c| p.has_component(c));
            }
        }
        // one save per publish (D6); a failed save is logged and the install stands (D7), as boot's save in system_services
        if let Err(e) = self.state.save() {
            log(format_args!("install: package state not saved: {e}"));
        }
        // the freezes end last: a package starts again only once its version is recorded and its state saved (D6)
        drop(frozen);
        Published {
            reports,
            audience: Audience {
                before: snapshot,
                after: published,
            },
        }
    }
}

/// INSTALL_* names (PackageManager.java) of the install path's failures; the message is the error chain.
const INTERNAL: &str = "INSTALL_FAILED_INTERNAL_ERROR";
const INVALID_APK: &str = "INSTALL_FAILED_INVALID_APK";
const DUPLICATE: &str = "INSTALL_FAILED_DUPLICATE_PACKAGE";
const DOWNGRADE: &str = "INSTALL_FAILED_VERSION_DOWNGRADE";
const PERMISSION_MODEL: &str = "INSTALL_FAILED_PERMISSION_MODEL_DOWNGRADE";
const UPDATE_INCOMPATIBLE: &str = "INSTALL_FAILED_UPDATE_INCOMPATIBLE";

/// An installer or control failure: the session fails, nothing of it is committed. Not a refusal: see [`refusal`].
fn failure(code: &'static str, e: anyhow::Error) -> Failure {
    Failure {
        code,
        message: format!("{e:#}"),
    }
}

/// A refusal of a session (PMS's checks): the session fails and takes nothing; counted under its INSTALL_* name (IPackageManager.install). `change_of` counts DOWNGRADE itself.
fn refusal(code: &'static str, e: anyhow::Error) -> Failure {
    refusal_text(code, format!("{e:#}"))
}

fn refusal_text(code: &'static str, message: String) -> Failure {
    atl_diag::refused(DIAG, &format!("install: {code}"));
    Failure { code, message }
}

/// The renames the commit takes: the parse's ApplicationInfo names each staged file (`staged_names`); a parse without one is a refusal.
fn renames(parsed: &Parsed, session: u32) -> Result<Vec<StagedName>, Failure> {
    parsed
        .info
        .application_info
        .as_ref()
        .with_context(|| format!("session {session}: no ApplicationInfo"))
        .and_then(staged_names)
        .with_context(|| format!("session {session}"))
        .map_err(|e| refusal(INVALID_APK, e))
}

/// A session past its refusals, as the commit takes it. `fresh`: `uid` came from the pool for it (a failed commit releases it); an update keeps the installed uid.
struct Checked {
    parsed: Parsed,
    version: i64,
    change: Change,
    names: Vec<StagedName>,
    uid: u32,
    fresh: bool,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// One session's install: the package, its uid and longVersionCode, and what the install did to it against the recorded version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reported {
    pub package: String,
    pub uid: u32,
    pub version: i64,
    pub change: Change,
}

/// The tables a publish's package broadcasts are judged against (BroadcastHelper's visibility allow lists, IPH :2447-2450): `before` = the table the publish
/// replaced (REMOVED, REPLACED), `after` = the table it published (ADDED, NEW). Membership is `Packages::visible` on the table, evaluated per receiver.
#[derive(Debug, Clone)]
pub struct Audience {
    pub before: Arc<Packages>,
    pub after: Arc<Packages>,
}

/// What one publish made visible: its reports, and the audience of their package broadcasts.
#[derive(Debug)]
pub struct Published {
    pub reports: Vec<Reported>,
    pub audience: Audience,
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
/// nothing here sets. The refusal fails that session only ([`Installer::install_one`] keeps the installed package: its code and data stay and it runs, as the installed app does on a device after a refused update).
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
    installed: Decoded,
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
    use crate::packages::Packages;
    use atl_broker::control::{Request, Response};
    use std::collections::BTreeMap;

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

    /// An installer output of `package` at `version`, its base at `dir`/base.apk; targetSdk 35, unsigned.
    fn frames_file(path: &Path, package: &str, version: i32, dir: &str) {
        frames_file_with(path, package, version, dir, 35, None, &[]);
    }

    /// `frames_file` with the targetSdk, one signing certificate (`cert`; None: no signing info) and the activities `classes` (`fixture::activity`).
    fn frames_file_with(
        path: &Path,
        package: &str,
        version: i32,
        dir: &str,
        target_sdk: i32,
        cert: Option<&[u8]>,
        classes: &[&str],
    ) {
        use atl_parcelables::codecs::android::content::pm::PackageInfo::{
            Signature, SigningDetails,
        };
        use atl_parcelables::parts::Part;
        use atl_services_types::installer_output::{Kind, write_frame};
        use binder::binder_impl::Parcel;
        let mut app = ApplicationInfo::default();
        app.item.package_name = Some(package.into());
        app.target_sdk_version = target_sdk;
        app.source_dir = Some(format!("{dir}/base.apk"));
        let info = PackageInfo {
            package_name: Some(package.into()),
            version_code: version,
            application_info: Some(app),
            activities: Some(
                classes
                    .iter()
                    .map(|c| Some(crate::packages::fixture::activity(package, c)))
                    .collect(),
            ),
            signing_info: cert.map(|c| SigningDetails::Known {
                signatures: Some(vec![Some(Signature {
                    bytes: Some(c.to_vec()),
                })]),
                scheme_version: 3,
                public_keys: None,
                past_signing_certificates: None,
            }),
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
        frames_file(
            &session_frames,
            "com.example.app",
            1,
            &format!("{DATA_APP}/vmdl0.tmp"),
        );
        frames_file(
            &kept_frames,
            "com.example.app",
            2,
            &format!("{DATA_APP}/com.example.app"),
        );
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
        let state = Arc::new(PackageUserStates::default());
        state.set_version_code("com.example.app", 2);
        let table = empty_table(&state);
        let control = Arc::new(ControlSocket::new(ours));
        let installer = installer_over(&control, &table, &state);
        let reports = installer
            .boot(&[0])
            .expect("the boot goes on with the installed package")
            .reports;
        drop(installer);
        drop(control);
        assert_eq!(
            server.join().expect("server"),
            ["Install", "Reinstate", "Commit"]
        );
        let installed = table.snapshot();
        assert_eq!(installed.all().len(), 1);
        assert_eq!(long_version(&installed.all()[0].info), 2);
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

    /// A fake atld on a socketpair: `answer` serves the control protocol; the services' end comes back as a ControlSocket. Drop it, then join the server.
    fn fake_atld(
        answer: impl Fn(Request) -> Response + Clone + Send + 'static,
    ) -> (Arc<ControlSocket>, std::thread::JoinHandle<()>) {
        use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair");
        let server = std::thread::spawn(move || {
            atl_broker::control::serve_lanes(theirs, answer.clone(), answer).expect("serve_lanes");
        });
        (Arc::new(ControlSocket::new(ours)), server)
    }

    /// A package stop that stops nothing: these tests run no process.
    struct NoStops;
    impl PackageStops for NoStops {
        fn freeze(&self, _package: &str) -> Box<dyn Send> {
            Box::new(())
        }
    }

    /// The services' installer over `control`, `table` and `state` (the table's user state): the device's ABI, a fresh id pool, the permissions of the table's list.
    fn installer_over(
        control: &Arc<ControlSocket>,
        table: &Arc<PackageTable>,
        state: &Arc<PackageUserStates>,
    ) -> Installer {
        installer_with(control, table, state, Arc::new(NoStops))
    }

    /// [`installer_over`] with `stops` as the package freezer.
    fn installer_with(
        control: &Arc<ControlSocket>,
        table: &Arc<PackageTable>,
        state: &Arc<PackageUserStates>,
        stops: Arc<dyn PackageStops>,
    ) -> Installer {
        let permissions = Arc::new(Permissions::new(&table.snapshot(), &[]));
        Installer::new(
            Arc::clone(control),
            test_config(),
            AppIds::new(),
            Arc::clone(table),
            permissions,
            Arc::clone(state),
            stops,
        )
    }

    /// The device's ABI for the tests: x86_64.
    fn test_config() -> Config {
        Config {
            abis: vec![SupportedAbi {
                abi: "x86_64".into(),
                isa: "x86_64".into(),
            }],
        }
    }

    /// Counts the freezes taken; the guard drops at once (these tests run no process).
    struct CountingStops(Arc<std::sync::atomic::AtomicUsize>);
    impl PackageStops for CountingStops {
        fn freeze(&self, _package: &str) -> Box<dyn Send> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::new(())
        }
    }

    /// [`fake_atld`] that also records each request's name (the first word of its Debug form); the join gives the names in arrival order.
    fn recording_atld(
        answer: impl Fn(Request) -> Response + Clone + Send + 'static,
    ) -> (Arc<ControlSocket>, std::thread::JoinHandle<Vec<String>>) {
        use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let server = std::thread::spawn(move || {
            let serve = move |r: Request| {
                let name = format!("{r:?}")
                    .split([' ', '{', '('])
                    .next()
                    .unwrap_or("")
                    .to_owned();
                record.lock().expect("seen").push(name);
                answer(r)
            };
            atl_broker::control::serve_lanes(theirs, serve.clone(), serve).expect("serve_lanes");
            let names = seen.lock().expect("seen").clone();
            names
        });
        (Arc::new(ControlSocket::new(ours)), server)
    }

    /// A table holding `record`, recorded at `version`, over `state`.
    fn table_with(
        state: &Arc<PackageUserStates>,
        record: PackageRecord,
        version: i64,
    ) -> Arc<PackageTable> {
        state.set_version_code(record.name(), version);
        Arc::new(PackageTable::new(
            Packages::new(vec![record], BTreeMap::new()).with_user_state_arc(Arc::clone(state)),
        ))
    }

    /// The record `package` at `version` has once installed: its frames parsed and recorded at uid 10_001, flags as its location gives (`flags`).
    fn installed_record(
        tag: &str,
        package: &str,
        version: i32,
        target_sdk: i32,
        cert: Option<&[u8]>,
        flags: i32,
    ) -> PackageRecord {
        let path =
            std::env::temp_dir().join(format!("atl-install-{tag}-{}.frames", std::process::id()));
        let dir = format!("{DATA_APP}/{package}");
        frames_file_with(&path, package, version, &dir, target_sdk, cert, &[]);
        let parsed = Parsed::read(&path.to_string_lossy()).expect("installed frames");
        let _ = std::fs::remove_file(&path);
        parsed.record(10_001, &dir, false, (flags, 0), &test_config())
    }

    /// What one Session install did: its report or refusal, the requests the control saw in order, and the freezes taken.
    struct Outcome {
        result: Result<Reported, Failure>,
        requests: Vec<String>,
        freezes: usize,
    }

    /// One Session install (session 0) of `package` at `version` (targetSdk `target`, signed by `cert`) over `recorded` (the installed record and its recorded version). The pool
    /// holds the installed uid, as boot leaves it. The fake atld stages, commits and abandons; any other request is an error.
    fn session_over(
        tag: &str,
        recorded: Option<(PackageRecord, i64)>,
        package: &str,
        version: i32,
        target: i32,
        cert: Option<&[u8]>,
    ) -> Outcome {
        let tmp = std::env::temp_dir().join(format!("atl-install-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("tmp");
        let f0 = tmp.join("0.frames");
        frames_file_with(
            &f0,
            package,
            version,
            &format!("{DATA_APP}/vmdl0.tmp"),
            target,
            cert,
            &[],
        );
        let f0 = f0.to_string_lossy().into_owned();
        let state = Arc::new(PackageUserStates::default());
        let mut ids = AppIds::new();
        let table = match recorded {
            Some((record, from)) => {
                ids.register(record.uid);
                table_with(&state, record, from)
            }
            None => empty_table(&state),
        };
        let (control, server) = recording_atld(move |r| match r {
            Request::Install { session: 0 } => Response::Staged { frames: f0.clone() },
            Request::Commit { .. } => Response::Committed,
            Request::Abandon { session: 0 } => Response::Abandoned,
            other => Response::Error {
                message: format!("unexpected {other:?}"),
            },
        });
        let freezes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let installer = installer_with(
            &control,
            &table,
            &state,
            Arc::new(CountingStops(Arc::clone(&freezes))),
        );
        let result = installer
            .install_one(&mut ids, 0, Origin::Session, &[])
            .map(|i| i.reported);
        drop(installer);
        drop(control);
        let requests = server.join().expect("server");
        let _ = std::fs::remove_dir_all(&tmp);
        Outcome {
            result,
            requests,
            freezes: freezes.load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// A session refused at its parse drops its staged session: control `abandon` follows its install, and no `Commit` is sent.
    #[test]
    fn a_refused_session_is_abandoned() {
        let tmp = std::env::temp_dir().join(format!("atl-install-drop-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("tmp");
        let f0 = tmp.join("0.frames");
        std::fs::write(&f0, b"").expect("empty output");
        let f0 = f0.to_string_lossy().into_owned();
        let state = Arc::new(PackageUserStates::default());
        let table = empty_table(&state);
        let (control, server) = recording_atld(move |r| match r {
            Request::Install { session: 0 } => Response::Staged { frames: f0.clone() },
            Request::Abandon { session: 0 } => Response::Abandoned,
            other => Response::Error {
                message: format!("unexpected {other:?}"),
            },
        });
        let installer = installer_over(&control, &table, &state);
        let mut ids = AppIds::new();
        let err = installer
            .install_one(&mut ids, 0, Origin::Session, &[])
            .err()
            .expect("an empty output is refused");
        assert_eq!(err.code, "INSTALL_FAILED_INVALID_APK");
        drop(installer);
        drop(control);
        assert_eq!(server.join().expect("server"), ["Install", "Abandon"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// An installed system package is not updated by a running install (D9): refused by name, the session dropped, nothing committed, no freeze.
    #[test]
    fn a_system_package_is_not_updated() {
        let record = installed_record(
            "sys",
            "com.example.app",
            2,
            35,
            None,
            crate::packages::FLAG_SYSTEM,
        );
        let out = session_over("sys", Some((record, 2)), "com.example.app", 3, 35, None);
        assert_eq!(
            out.result,
            Err(Failure {
                code: "INSTALL_FAILED_INTERNAL_ERROR",
                message: "updating system package com.example.app is not modelled".into(),
            })
        );
        assert_eq!(out.requests, ["Install", "Abandon"]);
        assert_eq!(out.freezes, 0);
    }

    /// A lower version than the installed one is refused as a downgrade (its text as checkDowngrade's); the session is dropped and nothing committed, no freeze.
    #[test]
    fn a_lower_version_of_an_installed_package_is_refused() {
        let record = installed_record("down", "com.example.app", 2, 35, None, 0);
        let out = session_over("down", Some((record, 2)), "com.example.app", 1, 35, None);
        assert_eq!(
            out.result,
            Err(Failure {
                code: "INSTALL_FAILED_VERSION_DOWNGRADE",
                message: "Update version code 1 is older than current 2".into(),
            })
        );
        assert_eq!(out.requests, ["Install", "Abandon"]);
        assert_eq!(out.freezes, 0);
    }

    /// An install at the installed version is an update: it keeps the installed uid (the pool's next id is not taken) and reports Updated from that version; the commit goes
    /// with that uid, and the package is frozen once.
    #[test]
    fn an_update_keeps_the_uid_and_reports_an_update() {
        let cert = Some(&b"cert-a"[..]);
        let record = installed_record("same", "com.example.app", 2, 35, cert, 0);
        let out = session_over("same", Some((record, 2)), "com.example.app", 2, 35, cert);
        assert_eq!(
            out.result,
            Ok(Reported {
                package: "com.example.app".into(),
                uid: 10_001,
                version: 2,
                change: Change::Updated { from: 2 },
            })
        );
        assert_eq!(out.requests, ["Install", "Commit"]);
        assert_eq!(out.freezes, 1);
    }

    /// An update may not drop the target SDK below runtime permissions: INSTALL_FAILED_PERMISSION_MODEL_DOWNGRADE, AOSP's text, nothing committed.
    #[test]
    fn a_target_sdk_that_drops_runtime_permissions_is_refused() {
        let record = installed_record("model", "com.example.app", 2, 30, None, 0);
        let out = session_over("model", Some((record, 2)), "com.example.app", 3, 22, None);
        assert_eq!(
            out.result,
            Err(Failure {
                code: "INSTALL_FAILED_PERMISSION_MODEL_DOWNGRADE",
                message: "Package com.example.app new target SDK 22 doesn't support runtime permissions but the old target SDK 30 does.".into(),
            })
        );
        assert_eq!(out.requests, ["Install", "Abandon"]);
    }

    /// An update carries the installed package's signers, equal as sets. Another signer, or none on either side, is refused (INSTALL_FAILED_UPDATE_INCOMPATIBLE).
    #[test]
    fn an_update_with_other_or_no_signers_is_refused() {
        let text =
            "Existing package com.example.app signatures do not match newer version; ignoring!";
        let (a, b) = (Some(&b"cert-a"[..]), Some(&b"cert-b"[..]));
        for (installed, new) in [(a, b), (a, None), (None, a)] {
            let record = installed_record("sign", "com.example.app", 2, 35, installed, 0);
            let out = session_over("sign", Some((record, 2)), "com.example.app", 3, 35, new);
            assert_eq!(
                out.result,
                Err(Failure {
                    code: "INSTALL_FAILED_UPDATE_INCOMPATIBLE",
                    message: text.into(),
                }),
                "installed {installed:?}, update {new:?}"
            );
            assert_eq!(out.requests, ["Install", "Abandon"]);
        }
    }

    /// A table holding no package, over `state` as its user state (system_services shares it the same way).
    fn empty_table(state: &Arc<PackageUserStates>) -> Arc<PackageTable> {
        Arc::new(PackageTable::new(
            Packages::new(vec![], BTreeMap::new()).with_user_state_arc(Arc::clone(state)),
        ))
    }

    /// Boot's two sessions: nothing is published between them (the table, read at the second session's install request, holds none) and one publish follows both; ids in session order.
    #[test]
    fn boot_publishes_once_after_every_session() {
        let tmp = std::env::temp_dir().join(format!("atl-install-once-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("tmp");
        let (f0, f1) = (tmp.join("0.frames"), tmp.join("1.frames"));
        frames_file(&f0, "com.example.one", 1, &format!("{DATA_APP}/vmdl0.tmp"));
        frames_file(&f1, "com.example.two", 1, &format!("{DATA_APP}/vmdl1.tmp"));
        let (f0, f1) = (
            f0.to_string_lossy().into_owned(),
            f1.to_string_lossy().into_owned(),
        );
        let state = Arc::new(PackageUserStates::default());
        let table = empty_table(&state);
        let seen = Arc::new(Mutex::new(None::<usize>));
        let (table_seen, seen_at) = (Arc::clone(&table), Arc::clone(&seen));
        let (control, server) = fake_atld(move |r| match r {
            Request::Install { session: 0 } => Response::Staged { frames: f0.clone() },
            Request::Install { session: 1 } => {
                *seen_at.lock().expect("seen") = Some(table_seen.snapshot().all().len());
                Response::Staged { frames: f1.clone() }
            }
            Request::Commit { .. } => Response::Committed,
            other => Response::Error {
                message: format!("unexpected {other:?}"),
            },
        });
        let before = table.snapshot();
        let installer = installer_over(&control, &table, &state);
        let reports = installer.boot(&[0, 1]).expect("boot").reports;
        drop(installer);
        drop(control);
        server.join().expect("server");
        assert_eq!(before.all().len(), 0, "a snapshot keeps the list it saw");
        assert_eq!(*seen.lock().expect("seen"), Some(0));
        assert_eq!(table.snapshot().all().len(), 2);
        assert_eq!(
            reports.iter().map(|r| r.uid).collect::<Vec<_>>(),
            [10_001, 10_002]
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A commit the control refuses fails the boot and publishes nothing; the uid taken for it is free again (`register` takes it back).
    #[test]
    fn a_failed_commit_releases_its_id_and_publishes_nothing() {
        let tmp = std::env::temp_dir().join(format!("atl-install-commit-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("tmp");
        let f0 = tmp.join("0.frames");
        frames_file(&f0, "com.example.one", 1, &format!("{DATA_APP}/vmdl0.tmp"));
        let f0 = f0.to_string_lossy().into_owned();
        let state = Arc::new(PackageUserStates::default());
        let table = empty_table(&state);
        let (control, server) = fake_atld(move |r| match r {
            Request::Install { session: 0 } => Response::Staged { frames: f0.clone() },
            Request::Commit { .. } => Response::Error {
                message: "commit refused".into(),
            },
            other => Response::Error {
                message: format!("unexpected {other:?}"),
            },
        });
        let installer = installer_over(&control, &table, &state);
        let err = installer
            .boot(&[0])
            .expect_err("a failed commit fails the start");
        assert!(format!("{err:#}").contains("commit refused"), "{err:#}");
        assert_eq!(table.snapshot().all().len(), 0);
        assert!(
            installer.ids.lock().expect("ids").register(10_001),
            "the failed commit's uid is free again"
        );
        drop(installer);
        drop(control);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A session refused at its parse (an empty installer output) takes no uid: the next session of the pool gets 10001.
    #[test]
    fn a_refused_first_session_takes_no_id() {
        let tmp = std::env::temp_dir().join(format!("atl-install-refused-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("tmp");
        let (f0, f1) = (tmp.join("0.frames"), tmp.join("1.frames"));
        std::fs::write(&f0, b"").expect("empty output");
        frames_file(&f1, "com.example.two", 1, &format!("{DATA_APP}/vmdl1.tmp"));
        let (f0, f1) = (
            f0.to_string_lossy().into_owned(),
            f1.to_string_lossy().into_owned(),
        );
        let state = Arc::new(PackageUserStates::default());
        let table = empty_table(&state);
        let (control, server) = fake_atld(move |r| match r {
            Request::Install { session: 0 } => Response::Staged { frames: f0.clone() },
            Request::Install { session: 1 } => Response::Staged { frames: f1.clone() },
            Request::Commit { .. } => Response::Committed,
            other => Response::Error {
                message: format!("unexpected {other:?}"),
            },
        });
        let installer = installer_over(&control, &table, &state);
        let mut ids = AppIds::new();
        assert!(
            installer
                .install_one(&mut ids, 0, Origin::Session, &[])
                .is_err()
        );
        let second = installer
            .install_one(&mut ids, 1, Origin::Session, &[])
            .expect("the second session installs");
        assert_eq!(second.reported.uid, 10_001);
        drop(installer);
        drop(control);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A running install's publish prunes the component overrides its new version no longer has (PackageSetting.restoreComponentSettings, IPH :590-596) and saves the state: a reload
    /// holds the kept override only.
    #[test]
    fn components_gone_from_the_new_version_lose_their_override() {
        use crate::package_state::{
            COMPONENT_ENABLED_STATE_DEFAULT, COMPONENT_ENABLED_STATE_DISABLED,
        };
        let tmp = std::env::temp_dir().join(format!("atl-install-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("tmp");
        let (v1, v2) = (tmp.join("v1.frames"), tmp.join("0.frames"));
        let dir = format!("{DATA_APP}/com.example.app");
        let cert = Some(&b"cert-a"[..]);
        frames_file_with(
            &v1,
            "com.example.app",
            1,
            &dir,
            35,
            cert,
            &[".Main", ".Gone"],
        );
        frames_file_with(
            &v2,
            "com.example.app",
            2,
            &format!("{DATA_APP}/vmdl0.tmp"),
            35,
            cert,
            &[".Main"],
        );
        let record = Parsed::read(&v1.to_string_lossy())
            .expect("v1 frames")
            .record(10_001, &dir, false, (0, 0), &test_config());
        let state = Arc::new(PackageUserStates::load(&tmp));
        let table = table_with(&state, record, 1);
        state.set_component("com.example.app", ".Main", COMPONENT_ENABLED_STATE_DISABLED);
        state.set_component("com.example.app", ".Gone", COMPONENT_ENABLED_STATE_DISABLED);
        let v2 = v2.to_string_lossy().into_owned();
        let (control, server) = recording_atld(move |r| match r {
            Request::Install { session: 0 } => Response::Staged { frames: v2.clone() },
            Request::Commit { .. } => Response::Committed,
            other => Response::Error {
                message: format!("unexpected {other:?}"),
            },
        });
        let installer = installer_over(&control, &table, &state);
        let mut ids = AppIds::new();
        ids.register(10_001);
        let accepted = installer
            .install_one(&mut ids, 0, Origin::Session, &[])
            .expect("the update is accepted");
        let reports = installer.publish(vec![accepted]).reports;
        drop(installer);
        drop(control);
        server.join().expect("server");
        assert_eq!(reports.len(), 1);
        assert_eq!(
            state.component("com.example.app", ".Gone"),
            COMPONENT_ENABLED_STATE_DEFAULT
        );
        assert_eq!(
            state.component("com.example.app", ".Main"),
            COMPONENT_ENABLED_STATE_DISABLED
        );
        let saved = PackageUserStates::load(&tmp);
        assert_eq!(
            saved.component("com.example.app", ".Main"),
            COMPONENT_ENABLED_STATE_DISABLED,
            "the publish saved the state"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A save the disk refuses is logged; the publish still answers and the install stands (D7: nothing after Commit's reply fails the install). A guard before the save: it passes
    /// with or without the save, and a `save().expect` breaks it.
    #[test]
    fn a_failed_save_does_not_fail_the_install() {
        let tmp = std::env::temp_dir().join(format!("atl-install-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::write(tmp.join("blocker"), b"").expect("a file in the way");
        let state = Arc::new(PackageUserStates::load(&tmp.join("blocker").join("state")));
        let table = empty_table(&state);
        let f0 = tmp.join("0.frames");
        frames_file(&f0, "com.example.one", 1, &format!("{DATA_APP}/vmdl0.tmp"));
        let f0 = f0.to_string_lossy().into_owned();
        let (control, server) = recording_atld(move |r| match r {
            Request::Install { session: 0 } => Response::Staged { frames: f0.clone() },
            Request::Commit { .. } => Response::Committed,
            other => Response::Error {
                message: format!("unexpected {other:?}"),
            },
        });
        let installer = installer_over(&control, &table, &state);
        let mut ids = AppIds::new();
        let accepted = installer
            .install_one(&mut ids, 0, Origin::Session, &[])
            .expect("the install is accepted");
        let reports = installer.publish(vec![accepted]).reports;
        drop(installer);
        drop(control);
        server.join().expect("server");
        assert_eq!(reports.len(), 1);
        assert_eq!(table.snapshot().all().len(), 1, "the install stands");
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
