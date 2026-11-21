// SPDX-License-Identifier: MIT
//! The `shortcut` service (Context.SHORTCUT_SERVICE, IShortcutService): ShortcutManager's constructor needs it (SystemServiceRegistry getServiceOrThrow), else getSystemService(ShortcutManager.class)
//! is null (LINE's SplashActivity died on removeAllDynamicShortcuts). SystemServer starts ShortcutService on every device.
//! Behaviour = ShortcutService (services/core/java/com/android/server/pm/ShortcutService.java + ShortcutPackage.java) for the caller's own dynamic shortcuts, on a device with no launcher that pins or caches
//! shortcuts: nothing is ever FLAG_PINNED or FLAG_CACHED_*, so every removal or disable deletes the shortcut outright (ShortcutPackage.deleteOrDisableWithId :590-627) and getShortcuts' PINNED / CACHED
//! matches are empty; isRequestPinItemSupported false, requestPinShortcut false (ShortcutRequestPinProcessor without a supporting launcher).
//! - Limits = ShortcutService's defaults (:176-197; no device config override here): 15 per activity, 10 calls per 24 h interval, icon 96 dp (not a low-RAM device) in px at the display density.
//! - Rate limit (ShortcutPackage.getApiCallCount :695-728): reset whenever the uid is foreground (procState <= BOUND_FOREGROUND_SERVICE, isUidForegroundLocked :632). ATL reports every app process
//!   PROCESS_STATE_TOP (running_processes.rs), so a caller is always foreground: its count is reset before each check and it is never limited, as an app in front is on a device.
//! - Order: ShortcutPackage keeps an ArrayMap by id, so lists come out by the ids' String.hashCode ascending, equal hashes in insertion order; ranks per activity by adjustRanks (:1558).
//! - Icons (saveIconAndFixUpShortcutLocked :1536): resource -> iconResId + FLAG_HAS_ICON_RES (own package only), uri -> iconUri + FLAG_HAS_ICON_URI, bitmap -> FLAG_HAS_ICON_FILE; the Icon is dropped.
//!   AOSP writes the bitmap to a file only a launcher reads (LauncherApps.getShortcutIconFd); there is none, so the pixels are not kept.
//! - Not modelled, counted: manifest shortcuts (`android.app.shortcuts` meta-data, ShortcutParser: the installer does not read the XML resource), resource-string resolution of res-id labels
//!   (fixUpShortcutResourceNamesAndValues needs the app's resources), usage reports to UsageStats (none here), share targets (MANAGE_APP_PREDICTIONS callers only).
//!
//! State: per package `<state dir>/shortcut_service/packages/<package>.json` (AOSP's per-package split under system_ce/<user>/shortcut_service, :5124) and the service-wide reset time in
//! `shortcut_service/shortcut_service.json` (FILENAME_BASE_STATE :203), through [`crate::state_file`]; each ShortcutInfo is kept as its marshalled parcel after fix-up. Written on every change (AOSP
//! defers by 3 s, DEFAULT_SAVE_DELAY_MS; one small file each here). pm clear (`--clear-data`) removes the package's file (handlePackageDataCleared :3824 -> cleanUpPackageLocked).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use atl_aidl::aidl::android::content::pm::IShortcutService::{BnShortcutService, IShortcutService};
use atl_parcelables::android::content::pm::{ParceledListSlice, ShortcutInfo};
use atl_parcelables::android::content::{Intent, IntentSender};
use atl_parcelables::codecs::android::content::pm::ShortcutInfo::{IconKind, ShortcutDetails};
use atl_parcelables::java::lang::CharSequence;
use atl_parcelables::parts::{ComponentName, Part};
use atl_services_types::permission;
use binder::binder_impl::Parcel;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};
use serde::{Deserialize, Serialize};

use crate::packages::{Packages, Permissions};
use crate::running_processes::PROCESS_STATE_TOP;
use crate::tasks::Tasks;

/// The name the answers are counted under (`atl_diag::answered`, the app gate's SERVICE_CALLS).
const SHORTCUT: &str = "IShortcutService";
/// ShortcutService defaults (ShortcutService.java:176-188).
const RESET_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_UPDATES_PER_INTERVAL: i32 = 10;
pub const MAX_SHORTCUTS_PER_ACTIVITY: i32 = 15;
const MAX_ICON_DIMENSION_DP: f32 = 96.0;
/// DisplayMetrics.DENSITY_DEFAULT: density = densityDpi / 160 (DisplayMetrics.density).
const DENSITY_DEFAULT: f32 = 160.0;
/// ActivityManager.PROCESS_STATE_BOUND_FOREGROUND_SERVICE: ShortcutService's PROCESS_STATE_FOREGROUND_THRESHOLD (:451).
const PROCESS_STATE_BOUND_FOREGROUND_SERVICE: i32 = 3;
/// android.permission.UNLIMITED_SHORTCUTS_API_CALLS (signature|role): injectHasUnlimitedShortcutsApiCallsPermission (:2823).
const UNLIMITED_SHORTCUTS_API_CALLS: &str = "android.permission.UNLIMITED_SHORTCUTS_API_CALLS";
/// ShortcutInfo flags and constants (content/pm/ShortcutInfo.java:77-156, :377).
const FLAG_DYNAMIC: i32 = 1 << 0;
const FLAG_PINNED: i32 = 1 << 1;
const FLAG_HAS_ICON_RES: i32 = 1 << 2;
const FLAG_HAS_ICON_FILE: i32 = 1 << 3;
const FLAG_MANIFEST: i32 = 1 << 5;
const FLAG_DISABLED: i32 = 1 << 6;
const FLAG_ADAPTIVE_BITMAP: i32 = 1 << 9;
const FLAG_RETURNED_BY_SERVICE: i32 = 1 << 10;
const FLAG_LONG_LIVED: i32 = 1 << 13;
const FLAG_HAS_ICON_URI: i32 = 1 << 15;
const FLAG_CACHED_ALL: i32 = (1 << 14) | (1 << 29) | (1 << 30);
const RANK_NOT_SET: i32 = i32::MAX;
const RANK_CHANGED_BIT: i32 = i32::MIN;
const SURFACE_LAUNCHER: i32 = 1 << 0;
/// ShortcutManager.FLAG_MATCH_* (content/pm/ShortcutManager.java).
const FLAG_MATCH_MANIFEST: i32 = 1 << 0;
const FLAG_MATCH_DYNAMIC: i32 = 1 << 1;
const FLAG_MATCH_PINNED: i32 = 1 << 2;
const FLAG_MATCH_CACHED: i32 = 1 << 3;
/// The state files' format version and diag name.
const VERSION: u32 = 1;
const DIAG: &str = "shortcut-store";
const DIR: &str = "shortcut_service";
const PACKAGES_DIR: &str = "packages";
const BASE_FILE: &str = "shortcut_service.json";

/// `package`'s shortcut file under the services' state dir; atld-run's pm clear (`--clear-data`) removes it.
pub fn package_file(state_dir: &Path, package: &str) -> PathBuf {
    state_dir
        .join(DIR)
        .join(PACKAGES_DIR)
        .join(format!("{package}.json"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// String.hashCode: the key ArrayMap orders by.
fn java_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(i32::from(c)))
}

fn exception(code: ExceptionCode, message: &str) -> Status {
    Status::new_exception_str(code, Some(message))
}
fn illegal_argument(message: &str) -> Status {
    exception(ExceptionCode::ILLEGAL_ARGUMENT, message)
}
fn illegal_state(message: &str) -> Status {
    exception(ExceptionCode::ILLEGAL_STATE, message)
}
fn null_pointer(message: &str) -> Status {
    exception(ExceptionCode::NULL_POINTER, message)
}
fn security(message: &str) -> Status {
    exception(ExceptionCode::SECURITY, message)
}

/// One stored shortcut and its server-side implicit rank (ShortcutInfo.mImplicitRank, RANK_CHANGED_BIT included: never parcelled).
#[derive(Debug, Clone)]
struct Entry {
    info: ShortcutInfo,
    implicit: i32,
}

impl Entry {
    fn id(&self) -> &str {
        self.info.id.as_deref().unwrap_or_default()
    }
    fn rank(&self) -> i32 {
        self.info.details.as_ref().map_or(0, |d| d.rank)
    }
    fn set_rank(&mut self, rank: i32) {
        if let Some(d) = &mut self.info.details {
            d.rank = rank;
        }
    }
    fn is_dynamic(&self) -> bool {
        self.info.flags & FLAG_DYNAMIC != 0
    }
}

/// ShortcutPackage: one package's shortcuts in ArrayMap insertion order (sorted by hash on output) and its rate-limit counters (call-count, last-reset).
#[derive(Debug, Default)]
struct PackageShortcuts {
    shortcuts: Vec<Entry>,
    api_calls: i32,
    last_reset: i64,
}

impl PackageShortcuts {
    fn find(&self, id: &str) -> Option<&Entry> {
        self.shortcuts.iter().find(|e| e.id() == id)
    }
    fn remove(&mut self, id: &str) -> Option<Entry> {
        let i = self.shortcuts.iter().position(|e| e.id() == id)?;
        Some(self.shortcuts.remove(i))
    }
    /// forceReplaceShortcutInner: the old one out, the new one in at the end (ArrayMap.put after remove).
    fn replace(&mut self, e: Entry) {
        self.remove(e.id());
        self.shortcuts.push(e);
    }
    /// The shortcuts in ArrayMap order: hash ascending, equal hashes in insertion order.
    fn ordered(&self) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self.shortcuts.iter().collect();
        v.sort_by_key(|e| java_hash(e.id()));
        v
    }
    /// clearAllImplicitRanks: implicit rank and RANK_CHANGED_BIT cleared.
    fn clear_implicit_ranks(&mut self) {
        self.shortcuts.iter_mut().for_each(|e| e.implicit = 0);
    }
    /// adjustRanks (:1558-1600): per activity, sorted by (rank, rank changed first, implicit rank, id), dynamic shortcuts get ranks 0..n; a changed rank stamps the time.
    fn adjust_ranks(&mut self, now: i64) {
        let mut activities: Vec<Option<ComponentName>> = Vec::new();
        for e in &self.shortcuts {
            if !activities.contains(&e.info.activity) {
                activities.push(e.info.activity.clone());
            }
        }
        for a in activities {
            let mut idx: Vec<usize> = (0..self.shortcuts.len())
                .filter(|&i| self.shortcuts[i].info.activity == a)
                .collect();
            idx.sort_by(|&x, &y| {
                let (a, b) = (&self.shortcuts[x], &self.shortcuts[y]);
                a.rank()
                    .cmp(&b.rank())
                    .then_with(|| {
                        (b.implicit & RANK_CHANGED_BIT != 0)
                            .cmp(&(a.implicit & RANK_CHANGED_BIT != 0))
                    })
                    .then_with(|| {
                        (a.implicit & !RANK_CHANGED_BIT).cmp(&(b.implicit & !RANK_CHANGED_BIT))
                    })
                    .then_with(|| a.id().cmp(b.id()))
            });
            let mut rank = 0;
            for i in idx {
                let e = &mut self.shortcuts[i];
                if !e.is_dynamic() {
                    continue;
                }
                if e.rank() != rank {
                    e.info.last_changed_timestamp = now;
                    e.set_rank(rank);
                }
                rank += 1;
            }
        }
    }
    /// getApiCallCount (:695-728): a foreground (or unlimited) caller resets the count; a service-wide reset after this package's last one zeroes it.
    fn api_call_count(&mut self, foreground: bool, service_last_reset: i64, now: i64) -> i32 {
        if foreground {
            self.api_calls = 0;
        }
        if self.last_reset > now {
            self.last_reset = now;
            self.api_calls = 0;
        } else if self.last_reset < service_last_reset {
            self.api_calls = 0;
            self.last_reset = service_last_reset;
        }
        self.api_calls
    }
}

/// A package file's contents: the shortcuts as hex parcels, then the counters.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredPackage {
    shortcuts: Vec<String>,
    call_count: i32,
    last_reset: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredBase {
    last_reset: i64,
}

fn to_hex(info: &ShortcutInfo) -> Result<String, binder::StatusCode> {
    let mut p = Parcel::new();
    info.write(&mut p.borrowed())?;
    let bytes = p.marshal()?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn from_hex(s: &str) -> Option<ShortcutInfo> {
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect::<Option<_>>()?;
    let p = Parcel::unmarshal(&bytes).ok()?;
    ShortcutInfo::read(p.borrowed_ref()).ok()
}

/// ShortcutUser for user 0, and where it is saved (None: in memory only).
struct Store {
    dir: Option<PathBuf>,
    packages: BTreeMap<String, PackageShortcuts>,
    /// mRawLastResetTime (service-wide; 0 = not yet set).
    last_reset: i64,
}

impl Store {
    /// The state under `dir`, files of packages `installed` denies skipped (an uninstalled package's shortcuts are gone, cleanUpPackageLocked with appStillExists false).
    fn load(dir: Option<PathBuf>, installed: impl Fn(&str) -> bool) -> Store {
        let mut store = Store {
            dir,
            packages: BTreeMap::new(),
            last_reset: 0,
        };
        let Some(dir) = store.dir.clone() else {
            return store;
        };
        store.last_reset =
            crate::state_file::load::<StoredBase>(&dir.join(DIR).join(BASE_FILE), VERSION, DIAG)
                .map_or(0, |b| b.last_reset);
        let Ok(files) = std::fs::read_dir(dir.join(DIR).join(PACKAGES_DIR)) else {
            return store;
        };
        for f in files.flatten() {
            let path = f.path();
            let Some(package) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .map(str::to_owned)
            else {
                continue;
            };
            if !installed(&package) {
                atl_diag::answered(DIAG, "load: file of a package not installed skipped");
                continue;
            }
            let Some(s) = crate::state_file::load::<StoredPackage>(&path, VERSION, DIAG) else {
                continue;
            };
            let mut shortcuts = Vec::new();
            for h in &s.shortcuts {
                match from_hex(h) {
                    Some(info) => shortcuts.push(Entry { info, implicit: 0 }),
                    None => {
                        atl_diag::refused(DIAG, "load: undecodable shortcut dropped");
                    }
                }
            }
            store.packages.insert(
                package,
                PackageShortcuts {
                    shortcuts,
                    api_calls: s.call_count,
                    last_reset: s.last_reset,
                },
            );
        }
        store
    }

    fn package(&mut self, package: &str) -> &mut PackageShortcuts {
        self.packages.entry(package.to_owned()).or_default()
    }

    /// updateTimesLocked (:1301-1320): the first use sets it to now; a rewound valid clock to now; past the interval it advances by whole intervals, keeping its offset.
    fn update_times(&mut self, now: i64) -> i64 {
        /// isClockValid (:1296): after 2015-01-01 (the constant is in seconds, compared to millis as AOSP does).
        const CLOCK_VALID: i64 = 1_420_070_400;
        let prev = self.last_reset;
        if self.last_reset == 0 || (now < self.last_reset && now >= CLOCK_VALID) {
            self.last_reset = now;
        } else if self.last_reset + RESET_INTERVAL_MS <= now {
            let offset = self.last_reset % RESET_INTERVAL_MS;
            self.last_reset = (now / RESET_INTERVAL_MS) * RESET_INTERVAL_MS + offset;
        }
        if prev != self.last_reset {
            self.save_base();
        }
        self.last_reset
    }

    fn save_base(&self) {
        if let Some(dir) = &self.dir
            && let Err(e) = crate::state_file::save(
                &dir.join(DIR).join(BASE_FILE),
                VERSION,
                &StoredBase {
                    last_reset: self.last_reset,
                },
            )
        {
            save_failed(&e.to_string());
        }
    }

    /// scheduleSave of `package`'s item: its file rewritten.
    fn save(&self, package: &str) {
        let Some(dir) = &self.dir else {
            return;
        };
        let file = package_file(dir, package);
        let Some(ps) = self.packages.get(package) else {
            return;
        };
        let shortcuts: Result<Vec<String>, _> =
            ps.shortcuts.iter().map(|e| to_hex(&e.info)).collect();
        match shortcuts {
            Ok(shortcuts) => {
                let stored = StoredPackage {
                    shortcuts,
                    call_count: ps.api_calls,
                    last_reset: ps.last_reset,
                };
                if let Err(e) = crate::state_file::save(&file, VERSION, &stored) {
                    save_failed(&e.to_string());
                }
            }
            Err(e) => save_failed(&format!("encode: {e:?}")),
        }
    }
}

fn save_failed(why: &str) {
    use std::io::Write;
    atl_diag::refused(DIAG, "save failed: kept in memory only");
    let _ = writeln!(
        std::io::stderr(),
        "services: {DIAG}: save failed ({why}): kept in memory only"
    );
}

struct ShortcutService {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    tasks: Arc<Tasks<SpIBinder>>,
    store: Mutex<Store>,
}

impl Interface for ShortcutService {}

/// Which operation enforceShortcutCountsBeforeOperation counts for (OPERATION_SET / ADD / UPDATE, :454-456).
#[derive(PartialEq, Clone, Copy)]
enum Operation {
    Set,
    Add,
    Update,
}

impl ShortcutService {
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// isCallerSystem: UserHandle.isSameApp(uid, SYSTEM_UID).
    fn is_system(uid: u32) -> bool {
        permission::app_id(uid) == permission::SYSTEM_UID
    }

    /// verifyCaller (:1740-1758): non-empty package; the caller's own user; the package's uid must be the caller's.
    fn verify_caller(&self, package: Option<&str>, user_id: i32) -> binder::Result<(String, u32)> {
        let package = package
            .filter(|p| !p.is_empty())
            .ok_or_else(|| illegal_argument("packageName"))?;
        let uid = ThreadState::get_calling_uid();
        if Self::is_system(uid) {
            return Ok((package.to_owned(), uid));
        }
        if i64::from(permission::user_id(uid)) != i64::from(user_id) {
            return Err(security("Invalid userId"));
        }
        if self.packages.by_name(package).map(|p| p.uid) != Some(uid) {
            return Err(security("Calling package name mismatch"));
        }
        Ok((package.to_owned(), uid))
    }

    /// verifyShortcutInfoPackage (:1760-1773).
    fn verify_shortcut_package(package: &str, uid: u32, si: &ShortcutInfo) -> binder::Result<()> {
        if si.package.as_deref() != Some(package) {
            return Err(security("Shortcut package name mismatch"));
        }
        if i64::from(permission::user_id(uid)) != i64::from(si.user_id) {
            return Err(security("UserId in shortcut doesn't match the caller"));
        }
        Ok(())
    }

    /// isUidForegroundLocked: SYSTEM_UID, or a procState at or below the threshold; every ATL app process is PROCESS_STATE_TOP (module doc).
    fn foreground(uid: u32) -> bool {
        Self::is_system(uid) || PROCESS_STATE_TOP <= PROCESS_STATE_BOUND_FOREGROUND_SERVICE
    }

    fn unlimited(&self, uid: u32) -> bool {
        self.permissions
            .holds(uid, &[UNLIMITED_SHORTCUTS_API_CALLS], false)
    }

    /// fillInDefaultActivity (:1945-1960): a shortcut without an activity gets the package's launcher activity.
    fn fill_in_default_activity(&self, list: &mut [ShortcutInfo]) -> binder::Result<()> {
        for si in list.iter_mut().filter(|s| s.activity.is_none()) {
            let package = si.package.clone().unwrap_or_default();
            let launcher = self
                .packages
                .by_name(&package)
                .and_then(|p| p.launcher())
                .and_then(|a| a.component.item.name.clone());
            let Some(class) = launcher else {
                return Err(illegal_state(&format!(
                    "Launcher activity not found for package {package}"
                )));
            };
            si.activity = Some(ComponentName { package, class });
        }
        Ok(())
    }

    /// fixUpIncomingShortcutInfo (:1888-1920) + validateIcon: the activity must be the package's main activity; unless updating, the mandatory fields; flags reduced to FLAG_LONG_LIVED.
    fn fix_up(&self, si: &mut ShortcutInfo, for_update: bool) -> binder::Result<()> {
        let package = si.package.clone().unwrap_or_default();
        if let Some(a) = &si.activity {
            let name = format!("ComponentInfo{{{}/{}}}", a.package, a.class);
            if a.package != package {
                return Err(illegal_state(&format!(
                    "Cannot publish shortcut: activity {name} does not belong to package {package}"
                )));
            }
            if !self
                .packages
                .by_name(&package)
                .is_some_and(|p| p.is_launcher(&a.class))
            {
                return Err(illegal_state(&format!(
                    "Cannot publish shortcut: activity {name} is not main activity"
                )));
            }
        }
        let d = si.details.get_or_insert_with(ShortcutDetails::default);
        if !for_update {
            // enforceMandatoryFields (ShortcutInfo.java)
            if si.id.as_deref().is_none_or(str::is_empty) {
                return Err(illegal_argument("Shortcut ID must be provided"));
            }
            if si.activity.is_none() {
                return Err(null_pointer("Activity must be provided"));
            }
            if d.title.is_none() && d.title_res_id == 0 {
                return Err(illegal_argument("Short label must be provided"));
            }
            match &d.intents {
                None => return Err(null_pointer("Shortcut Intent must be provided")),
                Some(i) if i.is_empty() => {
                    return Err(illegal_argument("Shortcut Intent must be provided"));
                }
                Some(_) => {}
            }
        }
        if d.title.is_none() && d.title_res_id != 0 {
            atl_diag::answered(SHORTCUT, "res-id label not resolved (no app resources)");
        }
        si.flags &= FLAG_LONG_LIVED;
        Ok(())
    }

    /// saveIconAndFixUpShortcutLocked (:1536-1590): the icon becomes flags + a res id or uri; the Icon itself is dropped.
    fn fix_up_icon(si: &mut ShortcutInfo) -> binder::Result<()> {
        let package = si.package.clone();
        let Some(d) = &mut si.details else {
            return Ok(());
        };
        let Some(icon) = d.icon.take() else {
            return Ok(());
        };
        match icon.kind {
            IconKind::Resource {
                package: res_package,
                res_id,
                ..
            } => {
                if res_package != package {
                    return Err(illegal_argument(
                        "Icon resource must reside in shortcut owner package",
                    ));
                }
                si.icon_res_id = res_id;
                si.flags |= FLAG_HAS_ICON_RES;
            }
            IconKind::Uri { adaptive, uri } => {
                d.icon_uri = uri;
                si.flags |= FLAG_HAS_ICON_URI | if adaptive { FLAG_ADAPTIVE_BITMAP } else { 0 };
            }
            IconKind::Bitmap { adaptive, .. } => {
                si.flags |= FLAG_HAS_ICON_FILE | if adaptive { FLAG_ADAPTIVE_BITMAP } else { 0 }
            }
        }
        Ok(())
    }

    /// enforceShortcutCountsBeforeOperation (ShortcutPackage :1439-1480) + enforceMaxActivityShortcuts (:1805): per activity, the dynamic shortcuts after the operation may not exceed 15.
    fn enforce_counts(
        ps: &PackageShortcuts,
        list: &[ShortcutInfo],
        op: Operation,
    ) -> binder::Result<()> {
        let mut counts: Vec<(Option<ComponentName>, i32)> = Vec::new();
        let mut add =
            |a: &Option<ComponentName>, n: i32| match counts.iter_mut().find(|(k, _)| k == a) {
                Some((_, c)) => *c += n,
                None => counts.push((a.clone(), n)),
            };
        for e in &ps.shortcuts {
            if e.info.flags & FLAG_MANIFEST != 0 || (e.is_dynamic() && op != Operation::Set) {
                add(&e.info.activity, 1);
            }
        }
        for si in list {
            if si.activity.is_none() {
                continue;
            }
            match ps.find(si.id.as_deref().unwrap_or_default()) {
                None if op == Operation::Update => {}
                None => add(&si.activity, 1),
                Some(orig) => {
                    // isFloating: pinned or cached and neither dynamic nor manifest (never here)
                    let floating = orig.info.flags & (FLAG_PINNED | FLAG_CACHED_ALL) != 0
                        && orig.info.flags & (FLAG_DYNAMIC | FLAG_MANIFEST) == 0;
                    if floating && op == Operation::Update {
                        continue;
                    }
                    if op != Operation::Set && !floating {
                        add(&orig.info.activity, -1);
                    }
                    add(&si.activity, 1);
                }
            }
        }
        if counts.iter().any(|(_, c)| *c > MAX_SHORTCUTS_PER_ACTIVITY) {
            return Err(illegal_argument("Max number of dynamic shortcuts exceeded"));
        }
        Ok(())
    }

    fn list_of(list: Option<&ParceledListSlice>) -> binder::Result<Vec<ShortcutInfo>> {
        let list = list.ok_or_else(|| null_pointer("shortcutInfoList"))?;
        Ok(list.of::<ShortcutInfo>().cloned().collect())
    }

    /// setDynamicShortcuts (:2027) and addDynamicShortcuts (:2197): the shared path; false when rate-limited.
    fn publish(
        &self,
        method: &str,
        package: Option<&str>,
        list: Option<&ParceledListSlice>,
        user_id: i32,
        op: Operation,
    ) -> binder::Result<bool> {
        let (package, uid) = self.verify_caller(package, user_id)?;
        let reset = Self::foreground(uid) || self.unlimited(uid);
        let mut list = Self::list_of(list)?;
        list.iter()
            .try_for_each(|si| Self::verify_shortcut_package(&package, uid, si))?;
        let now = now_ms();
        let mut store = self.store();
        let service_reset = store.update_times(now);
        self.fill_in_default_activity(&mut list)?;
        let ps = store.package(&package);
        Self::enforce_counts(ps, &list, op)?;
        let mut implicit: Vec<i32> = (0..list.len())
            .map(|i| i32::try_from(i).unwrap_or(i32::MAX))
            .collect();
        if op == Operation::Add {
            implicit.iter_mut().for_each(|r| *r |= RANK_CHANGED_BIT);
        }
        if op == Operation::Set && !Self::try_api_call(ps, reset, service_reset, now) {
            return Ok(false);
        }
        ps.clear_implicit_ranks();
        if op == Operation::Add && !Self::try_api_call(ps, reset, service_reset, now) {
            return Ok(false);
        }
        for si in &mut list {
            self.fix_up(si, false)?;
        }
        if op == Operation::Set {
            // deleteAllDynamicShortcuts: nothing is pinned or cached, so every dynamic shortcut goes (removeOrphans)
            ps.shortcuts.retain(|e| !e.is_dynamic());
        }
        let result = (|| {
            for (mut si, implicit) in list.into_iter().zip(implicit) {
                // addOrReplaceDynamicShortcut (ShortcutPackage :358-375)
                if si.flags & FLAG_DISABLED != 0 {
                    return Err(illegal_argument(
                        "add/setDynamicShortcuts() cannot publish disabled shortcuts",
                    ));
                }
                si.flags |= FLAG_DYNAMIC;
                if si
                    .details
                    .as_ref()
                    .is_some_and(|d| d.excluded_surfaces & SURFACE_LAUNCHER != 0)
                {
                    continue;
                }
                Self::fix_up_icon(&mut si)?;
                ps.replace(Entry { info: si, implicit });
            }
            Ok(())
        })();
        ps.adjust_ranks(now);
        store.save(&package);
        result?;
        atl_diag::answered(SHORTCUT, method);
        Ok(true)
    }

    /// tryApiCall (ShortcutPackage :744-752).
    /// `reset`: the caller is foreground or holds UNLIMITED_SHORTCUTS_API_CALLS.
    fn try_api_call(ps: &mut PackageShortcuts, reset: bool, service_reset: i64, now: i64) -> bool {
        if ps.api_call_count(reset, service_reset, now) >= MAX_UPDATES_PER_INTERVAL {
            return false;
        }
        ps.api_calls += 1;
        true
    }

    /// The ids-based mutators' shared check: the list must be given (Objects.requireNonNull "shortcutIds must be provided"), each id non-empty.
    fn ids(ids: Option<&[Option<String>]>) -> binder::Result<Vec<String>> {
        let ids = ids.ok_or_else(|| null_pointer("shortcutIds must be provided"))?;
        ids.iter()
            .rev()
            .map(|i| {
                i.clone()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| illegal_argument(""))
            })
            .collect()
    }

    /// removeDynamicShortcuts / disableShortcuts / removeLongLivedShortcuts: with nothing pinned or cached each existing id is deleted (deleteOrDisableWithId's forceDelete branch).
    fn delete_ids(
        &self,
        method: &str,
        package: Option<&str>,
        ids: Option<&[Option<String>]>,
        user_id: i32,
    ) -> binder::Result<()> {
        let (package, _) = self.verify_caller(package, user_id)?;
        let ids = Self::ids(ids)?;
        let mut store = self.store();
        let ps = store.package(&package);
        for id in &ids {
            ps.remove(id);
        }
        ps.adjust_ranks(now_ms());
        store.save(&package);
        atl_diag::answered(SHORTCUT, method);
        Ok(())
    }
}

impl IShortcutService for ShortcutService {
    fn r#setDynamicShortcuts(
        &self,
        package: Option<&str>,
        list: Option<&ParceledListSlice>,
        user_id: i32,
    ) -> binder::Result<bool> {
        self.publish(
            "setDynamicShortcuts",
            package,
            list,
            user_id,
            Operation::Set,
        )
    }

    fn r#addDynamicShortcuts(
        &self,
        package: Option<&str>,
        list: Option<&ParceledListSlice>,
        user_id: i32,
    ) -> binder::Result<bool> {
        self.publish(
            "addDynamicShortcuts",
            package,
            list,
            user_id,
            Operation::Add,
        )
    }

    /// pushDynamicShortcut (:2259-2300 + ShortcutPackage :391-445): no rate limit; at 15 for its activity the highest-ranked dynamic one goes first.
    fn r#pushDynamicShortcut(
        &self,
        package: Option<&str>,
        shortcut: Option<&ShortcutInfo>,
        user_id: i32,
    ) -> binder::Result<()> {
        let (package, uid) = self.verify_caller(package, user_id)?;
        let mut si = shortcut.cloned().ok_or_else(|| null_pointer("shortcut"))?;
        Self::verify_shortcut_package(&package, uid, &si)?;
        let now = now_ms();
        let mut store = self.store();
        self.fill_in_default_activity(std::slice::from_mut(&mut si))?;
        let d = si.details.get_or_insert_with(ShortcutDetails::default);
        if d.rank == RANK_NOT_SET {
            d.rank = 0;
        }
        let ps = store.package(&package);
        ps.clear_implicit_ranks();
        self.fix_up(&mut si, false)?;
        if si.flags & FLAG_DISABLED != 0 {
            return Err(illegal_argument(
                "pushDynamicShortcuts() cannot publish disabled shortcuts",
            ));
        }
        si.flags |= FLAG_DYNAMIC;
        let id = si.id.clone().unwrap_or_default();
        if !ps.find(&id).is_some_and(Entry::is_dynamic) {
            let mut same: Vec<&Entry> = ps
                .shortcuts
                .iter()
                .filter(|e| e.info.activity == si.activity)
                .collect();
            if same.len() == usize::try_from(MAX_SHORTCUTS_PER_ACTIVITY).unwrap_or(usize::MAX) {
                same.sort_by_key(|e| e.rank());
                let last = same.last().map(|e| e.id().to_owned()).unwrap_or_default();
                ps.remove(&last);
            }
        }
        if !si
            .details
            .as_ref()
            .is_some_and(|d| d.excluded_surfaces & SURFACE_LAUNCHER != 0)
        {
            Self::fix_up_icon(&mut si)?;
            ps.replace(Entry {
                info: si,
                implicit: RANK_CHANGED_BIT,
            });
        }
        ps.adjust_ranks(now);
        store.save(&package);
        atl_diag::answered(SHORTCUT, "pushDynamicShortcut");
        Ok(())
    }

    /// updateShortcuts (:2097-2170): existing ids take the source's non-null fields (ShortcutInfo.copyNonNullFieldsFrom); unknown ids are skipped.
    fn r#updateShortcuts(
        &self,
        package: Option<&str>,
        list: Option<&ParceledListSlice>,
        user_id: i32,
    ) -> binder::Result<bool> {
        let (package, uid) = self.verify_caller(package, user_id)?;
        let reset = Self::foreground(uid) || self.unlimited(uid);
        let list = Self::list_of(list)?;
        list.iter()
            .try_for_each(|si| Self::verify_shortcut_package(&package, uid, si))?;
        if let Some(si) = list.iter().find(|si| {
            si.details
                .as_ref()
                .is_some_and(|d| d.excluded_surfaces & SURFACE_LAUNCHER != 0)
        }) {
            return Err(illegal_argument(&format!(
                "Shortcut ID={} is hidden from launcher and may not be manipulated via APIs",
                si.id.as_deref().unwrap_or_default()
            )));
        }
        let now = now_ms();
        let mut store = self.store();
        let service_reset = store.update_times(now);
        let ps = store.package(&package);
        Self::enforce_counts(ps, &list, Operation::Update)?;
        if !Self::try_api_call(ps, reset, service_reset, now) {
            return Ok(false);
        }
        ps.clear_implicit_ranks();
        for (i, mut source) in list.into_iter().enumerate() {
            self.fix_up(&mut source, true)?;
            let id = source.id.clone().unwrap_or_default();
            let Some(pos) = ps.shortcuts.iter().position(|e| e.id() == id) else {
                continue;
            };
            let target = &mut ps.shortcuts[pos];
            let s = source.details.unwrap_or_default();
            if s.rank != RANK_NOT_SET {
                target.implicit = RANK_CHANGED_BIT | i32::try_from(i).unwrap_or(i32::MAX);
            }
            if let Some(a) = source.activity {
                target.info.activity = Some(a);
            }
            let t = target
                .info
                .details
                .get_or_insert_with(ShortcutDetails::default);
            if s.icon.is_some() {
                target.info.icon_res_id = 0;
                target.info.flags &= !(FLAG_HAS_ICON_RES
                    | FLAG_HAS_ICON_FILE
                    | FLAG_HAS_ICON_URI
                    | FLAG_ADAPTIVE_BITMAP);
                t.icon = s.icon;
                t.icon_res_name = None;
                t.bitmap_path = None;
                t.icon_uri = None;
            }
            let text = |v: &mut Option<String>, id: &mut i32, sv: Option<String>, sid: i32| {
                if sv.is_some() {
                    *v = sv;
                    *id = 0;
                } else if sid != 0 {
                    *v = None;
                    *id = sid;
                }
            };
            text(&mut t.title, &mut t.title_res_id, s.title, s.title_res_id);
            text(&mut t.text, &mut t.text_res_id, s.text, s.text_res_id);
            text(
                &mut t.disabled_message,
                &mut t.disabled_message_res_id,
                s.disabled_message,
                s.disabled_message_res_id,
            );
            if s.categories.is_some() {
                t.categories = s.categories;
            }
            if s.persons.is_some() {
                t.persons = s.persons;
            }
            if s.intents.is_some() {
                t.intents = s.intents;
                t.intent_persistable_extras = s.intent_persistable_extras;
            }
            if s.rank != RANK_NOT_SET {
                t.rank = s.rank;
            }
            if s.extras.is_some() {
                t.extras = s.extras;
            }
            if s.locus_id.is_some() {
                t.locus_id = s.locus_id;
            }
            if s.starting_theme_res_name
                .as_deref()
                .is_some_and(|n| !n.is_empty())
            {
                t.starting_theme_res_name = s.starting_theme_res_name;
            }
            target.info.last_changed_timestamp = now;
            Self::fix_up_icon(&mut target.info)?;
        }
        ps.adjust_ranks(now);
        store.save(&package);
        atl_diag::answered(SHORTCUT, "updateShortcuts");
        Ok(true)
    }

    fn r#removeDynamicShortcuts(
        &self,
        package: Option<&str>,
        ids: Option<&[Option<String>]>,
        user_id: i32,
    ) -> binder::Result<()> {
        self.delete_ids("removeDynamicShortcuts", package, ids, user_id)
    }

    fn r#removeLongLivedShortcuts(
        &self,
        package: Option<&str>,
        ids: Option<&[Option<String>]>,
        user_id: i32,
    ) -> binder::Result<()> {
        self.delete_ids("removeLongLivedShortcuts", package, ids, user_id)
    }

    fn r#disableShortcuts(
        &self,
        package: Option<&str>,
        ids: Option<&[Option<String>]>,
        _message: Option<&CharSequence>,
        _message_res_id: i32,
        user_id: i32,
    ) -> binder::Result<()> {
        self.delete_ids("disableShortcuts", package, ids, user_id)
    }

    /// enableShortcuts (:2441): clears FLAG_DISABLED of existing ids; nothing stored is ever disabled here (a disabled dynamic shortcut is deleted), so only the checks remain.
    fn r#enableShortcuts(
        &self,
        package: Option<&str>,
        ids: Option<&[Option<String>]>,
        user_id: i32,
    ) -> binder::Result<()> {
        self.verify_caller(package, user_id)?;
        Self::ids(ids)?;
        atl_diag::answered(SHORTCUT, "enableShortcuts");
        Ok(())
    }

    /// removeAllDynamicShortcuts (:2508): every dynamic shortcut goes (none is pinned or cached).
    fn r#removeAllDynamicShortcuts(
        &self,
        package: Option<&str>,
        user_id: i32,
    ) -> binder::Result<()> {
        let (package, _) = self.verify_caller(package, user_id)?;
        let mut store = self.store();
        let ps = store.package(&package);
        ps.shortcuts.retain(|e| !e.is_dynamic());
        store.save(&package);
        atl_diag::answered(SHORTCUT, "removeAllDynamicShortcuts");
        Ok(())
    }

    /// getShortcuts (:2568-2590): the flag filter, cloned CLONE_REMOVE_FOR_CREATOR (icon, bitmap path, icon uri, res names dropped), FLAG_RETURNED_BY_SERVICE.
    fn r#getShortcuts(
        &self,
        package: Option<&str>,
        match_flags: i32,
        user_id: i32,
    ) -> binder::Result<Option<ParceledListSlice>> {
        let (package, _) = self.verify_caller(package, user_id)?;
        let mut flags = 0;
        for (m, f) in [
            (FLAG_MATCH_DYNAMIC, FLAG_DYNAMIC),
            (FLAG_MATCH_PINNED, FLAG_PINNED),
            (FLAG_MATCH_MANIFEST, FLAG_MANIFEST),
            (FLAG_MATCH_CACHED, FLAG_CACHED_ALL),
        ] {
            if match_flags & m != 0 {
                flags |= f;
            }
        }
        if match_flags & FLAG_MATCH_MANIFEST != 0 {
            atl_diag::answered(
                SHORTCUT,
                "getShortcuts MATCH_MANIFEST (manifest shortcuts not read)",
            );
        }
        let mut store = self.store();
        let ps = store.package(&package);
        let list: Vec<ShortcutInfo> = ps
            .ordered()
            .into_iter()
            .filter(|e| e.info.flags & flags != 0)
            .map(|e| {
                let mut si = e.info.clone();
                if let Some(d) = &mut si.details {
                    d.icon = None;
                    d.bitmap_path = None;
                    d.icon_uri = None;
                    d.icon_res_name = None;
                    d.title_res_name = None;
                    d.text_res_name = None;
                    d.disabled_message_res_name = None;
                }
                si.flags |= FLAG_RETURNED_BY_SERVICE;
                si
            })
            .collect();
        atl_diag::answered(SHORTCUT, "getShortcuts");
        Ok(Some(ParceledListSlice::new(list)))
    }

    fn r#getMaxShortcutCountPerActivity(
        &self,
        package: Option<&str>,
        user_id: i32,
    ) -> binder::Result<i32> {
        self.verify_caller(package, user_id)?;
        atl_diag::answered(SHORTCUT, "getMaxShortcutCountPerActivity");
        Ok(MAX_SHORTCUTS_PER_ACTIVITY)
    }

    fn r#getRemainingCallCount(&self, package: Option<&str>, user_id: i32) -> binder::Result<i32> {
        let (package, uid) = self.verify_caller(package, user_id)?;
        let reset = Self::foreground(uid) || self.unlimited(uid);
        let now = now_ms();
        let mut store = self.store();
        let service_reset = store.update_times(now);
        let count = store
            .package(&package)
            .api_call_count(reset, service_reset, now);
        atl_diag::answered(SHORTCUT, "getRemainingCallCount");
        Ok(MAX_UPDATES_PER_INTERVAL - count)
    }

    /// getNextResetTimeLocked (:1288): the service-wide last reset + the interval.
    fn r#getRateLimitResetTime(&self, package: Option<&str>, user_id: i32) -> binder::Result<i64> {
        self.verify_caller(package, user_id)?;
        let last = self.store().update_times(now_ms());
        atl_diag::answered(SHORTCUT, "getRateLimitResetTime");
        Ok(last + RESET_INTERVAL_MS)
    }

    /// mMaxIconDimension = injectDipToPixel(96) (:828, :848): TypedValue.applyDimension(DIP) = dp * densityDpi / 160 in float, truncated.
    fn r#getIconMaxDimensions(&self, package: Option<&str>, user_id: i32) -> binder::Result<i32> {
        self.verify_caller(package, user_id)?;
        atl_diag::answered(SHORTCUT, "getIconMaxDimensions");
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        Ok((MAX_ICON_DIMENSION_DP * (self.tasks.display().dpi as f32 / DENSITY_DEFAULT)) as i32)
    }

    /// reportShortcutUsed (:2706): checks only; there is no UsageStats to report to.
    fn r#reportShortcutUsed(
        &self,
        package: Option<&str>,
        shortcut_id: Option<&str>,
        user_id: i32,
    ) -> binder::Result<()> {
        self.verify_caller(package, user_id)?;
        shortcut_id.ok_or_else(|| null_pointer("shortcutId"))?;
        atl_diag::answered(SHORTCUT, "reportShortcutUsed (no usage stats)");
        Ok(())
    }

    /// isRequestPinItemSupported (:2727): verifyCallerUserId, then no launcher supports pinning.
    fn r#isRequestPinItemSupported(&self, user: i32, _request_type: i32) -> binder::Result<bool> {
        let uid = ThreadState::get_calling_uid();
        if !Self::is_system(uid) && i64::from(permission::user_id(uid)) != i64::from(user) {
            return Err(security("Invalid userId"));
        }
        atl_diag::answered(SHORTCUT, "isRequestPinItemSupported (no launcher)");
        Ok(false)
    }

    /// requestPinShortcut (:2314) -> requestPinItem: the argument checks, then no launcher to ask: false.
    fn r#requestPinShortcut(
        &self,
        package: Option<&str>,
        shortcut: Option<&ShortcutInfo>,
        _result: Option<&IntentSender>,
        user_id: i32,
    ) -> binder::Result<bool> {
        let si = shortcut.ok_or_else(|| null_pointer(""))?;
        if si.flags & FLAG_DISABLED != 0 {
            return Err(illegal_argument("Shortcut must be enabled"));
        }
        if si
            .details
            .as_ref()
            .is_some_and(|d| d.excluded_surfaces & SURFACE_LAUNCHER != 0)
        {
            return Err(illegal_argument(
                "Shortcut excluded from launcher cannot be pinned",
            ));
        }
        let (package, uid) = self.verify_caller(package, user_id)?;
        Self::verify_shortcut_package(&package, uid, si)?;
        atl_diag::answered(SHORTCUT, "requestPinShortcut (no launcher: false)");
        Ok(false)
    }

    /// createShortcutResultIntent (:2326): the checks, then no launcher to make the intent for: null.
    fn r#createShortcutResultIntent(
        &self,
        package: Option<&str>,
        shortcut: Option<&ShortcutInfo>,
        user_id: i32,
    ) -> binder::Result<Option<Intent>> {
        let si = shortcut.ok_or_else(|| null_pointer(""))?;
        if si.flags & FLAG_DISABLED != 0 {
            return Err(illegal_argument("Shortcut must be enabled"));
        }
        let (package, uid) = self.verify_caller(package, user_id)?;
        Self::verify_shortcut_package(&package, uid, si)?;
        atl_diag::answered(SHORTCUT, "createShortcutResultIntent (no launcher: null)");
        Ok(None)
    }
}

/// The `shortcut` binder, for `addService`; `state_dir` the services' state dir (None: in memory only). Files of packages not installed are skipped.
pub fn new_binder(
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    tasks: Arc<Tasks<SpIBinder>>,
    state_dir: Option<PathBuf>,
) -> SpIBinder {
    let store = Store::load(state_dir, |p| packages.by_name(p).is_some());
    {
        use std::io::Write;
        let n: usize = store.packages.values().map(|p| p.shortcuts.len()).sum();
        let _ = writeln!(
            std::io::stderr(),
            "services: shortcut: {n} shortcuts of {} packages loaded",
            store.packages.len()
        );
    }
    BnShortcutService::new_binder(
        ShortcutService {
            packages,
            permissions,
            tasks,
            store: Mutex::new(store),
        },
        BinderFeatures::default(),
    )
    .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, rank: i32, implicit: i32) -> Entry {
        let info = ShortcutInfo {
            id: Some(id.into()),
            flags: FLAG_DYNAMIC,
            activity: Some(ComponentName {
                package: "p".into(),
                class: "A".into(),
            }),
            details: Some(ShortcutDetails {
                rank,
                ..Default::default()
            }),
            ..Default::default()
        };
        Entry { info, implicit }
    }

    #[test]
    fn java_hash_matches_string_hashcode() {
        // "a".hashCode() = 97; "ab" = 97*31+98 = 3105; "" = 0
        assert_eq!(java_hash("a"), 97);
        assert_eq!(java_hash("ab"), 3105);
        assert_eq!(java_hash(""), 0);
    }

    #[test]
    fn adjust_ranks_orders_set_then_added() {
        // setDynamicShortcuts([a, b]) without ranks: implicit 0, 1 -> ranks 0, 1; then add [c] (RANK_NOT_SET, rank changed) after clearing implicit ranks -> c last
        let mut ps = PackageShortcuts {
            shortcuts: vec![entry("b", RANK_NOT_SET, 1), entry("a", RANK_NOT_SET, 0)],
            ..Default::default()
        };
        ps.adjust_ranks(5);
        ps.clear_implicit_ranks();
        ps.replace(entry("c", RANK_NOT_SET, RANK_CHANGED_BIT));
        ps.adjust_ranks(6);
        let ranks: Vec<(String, i32)> = ps
            .ordered()
            .iter()
            .map(|e| (e.id().to_owned(), e.rank()))
            .collect();
        assert_eq!(
            ranks,
            [
                ("a".to_owned(), 0),
                ("b".to_owned(), 1),
                ("c".to_owned(), 2)
            ]
        );
    }

    #[test]
    fn counts_reject_sixteen() {
        let ps = PackageShortcuts::default();
        let list: Vec<ShortcutInfo> = (0..16)
            .map(|i| entry(&format!("s{i}"), RANK_NOT_SET, 0).info)
            .collect();
        assert!(ShortcutService::enforce_counts(&ps, &list[..15], Operation::Set).is_ok());
        assert!(ShortcutService::enforce_counts(&ps, &list, Operation::Set).is_err());
    }

    #[test]
    fn rate_limit_resets_in_foreground() {
        let mut ps = PackageShortcuts {
            api_calls: 9,
            last_reset: 100,
            ..Default::default()
        };
        assert_eq!(ps.api_call_count(false, 100, 200), 9);
        assert_eq!(ps.api_call_count(true, 100, 200), 0);
    }

    #[test]
    fn hex_round_trip() {
        let e = entry("x", 3, 0);
        let h = to_hex(&e.info).expect("encode");
        let back = from_hex(&h).expect("decode");
        assert_eq!(
            (
                back.id.as_deref(),
                back.flags,
                back.activity,
                back.details.map(|d| d.rank)
            ),
            (Some("x"), FLAG_DYNAMIC, e.info.activity, Some(3))
        );
        assert_eq!(to_hex(&from_hex(&h).expect("decode")).expect("encode"), h);
    }
}
