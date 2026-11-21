// SPDX-License-Identifier: MIT
//! The per-user package state PMS keeps beside the install (AOSP PackageUserState): today the component enabled overrides of user 0, set by IPackageManager.setComponentEnabledSetting and read by
//! every resolution through [`is_enabled`] (Packages::enabled).
//! Persisted as one file the services own, `package-state.json` in the services' state dir (atld `--system-dir`), in [`state_file`]'s envelope (versioned, temp-then-rename, missing/unreadable =
//! default). Entries of packages no longer installed are dropped at load (the package was removed: PMS forgets its settings).
//! The app-level setting (setApplicationEnabledSetting) is not stored: no surveyed app calls it, so the app state is always DEFAULT and the manifest's `<application android:enabled>` decides.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::state_file;

/// PackageManager.COMPONENT_ENABLED_STATE_* (PackageManager.java).
pub const COMPONENT_ENABLED_STATE_DEFAULT: i32 = 0;
pub const COMPONENT_ENABLED_STATE_ENABLED: i32 = 1;
pub const COMPONENT_ENABLED_STATE_DISABLED: i32 = 2;
pub const COMPONENT_ENABLED_STATE_DISABLED_USER: i32 = 3;
pub const COMPONENT_ENABLED_STATE_DISABLED_UNTIL_USED: i32 = 4;
/// PackageManager.MATCH_DISABLED_COMPONENTS: isEnabled answers true whatever the state.
pub const MATCH_DISABLED_COMPONENTS: i64 = 0x200;
/// The file in the state dir, and the format version this build reads and writes.
const FILE: &str = "package-state.json";
const VERSION: u32 = 1;
/// The diag interface the store's counts go under.
const DIAG: &str = "package-state";

/// PackageUserStateUtils.isEnabled (pm/pkg/PackageUserStateUtils.java:146-185) with the app state DEFAULT (not stored, module doc) and no quarantine: MATCH_DISABLED_COMPONENTS matches all; a disabled
/// `<application>` disables every component; then the component's override (`state`, ENABLED / DISABLED / DEFAULT), else its manifest `android:enabled`.
pub fn is_enabled(package_enabled: bool, component_enabled: bool, state: i32, flags: i64) -> bool {
    if flags & MATCH_DISABLED_COMPONENTS != 0 {
        return true;
    }
    if !package_enabled {
        return false;
    }
    match state {
        COMPONENT_ENABLED_STATE_ENABLED => true,
        COMPONENT_ENABLED_STATE_DISABLED => false,
        _ => component_enabled,
    }
}

/// One package's overrides (PackageUserStateImpl mEnabledComponents / mDisabledComponents): a class is in at most one set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Overrides {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    enabled: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    disabled: BTreeSet<String>,
}

/// The file's shape inside [`state_file`]'s envelope.
#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    packages: BTreeMap<String, Overrides>,
}

/// User 0's package state, and where it is saved (None: kept in memory only, as for a services instance given no state dir).
#[derive(Debug, Default)]
pub struct PackageUserStates {
    file: Option<PathBuf>,
    packages: Mutex<BTreeMap<String, Overrides>>,
}

impl PackageUserStates {
    /// The state saved in `dir`, entries of packages `installed` denies dropped. A missing file is the empty state; an unreadable one (I/O, JSON, another version) is the empty state, counted and logged
    /// (the next save replaces it).
    pub fn load(dir: &Path, installed: impl Fn(&str) -> bool) -> Self {
        let file = dir.join(FILE);
        let packages = state_file::load::<Stored>(&file, VERSION, DIAG)
            .map_or_else(BTreeMap::new, |s| s.packages);
        let (kept, dropped): (BTreeMap<_, _>, BTreeMap<_, _>) =
            packages.into_iter().partition(|(p, _)| installed(p));
        for p in dropped.keys() {
            atl_diag::answered(DIAG, "load: entry of a package not installed dropped");
            log(&format!(
                "{}: {p} is not installed: its component states dropped",
                file.display()
            ));
        }
        PackageUserStates {
            file: Some(file),
            packages: Mutex::new(kept),
        }
    }

    fn packages(&self) -> MutexGuard<'_, BTreeMap<String, Overrides>> {
        self.packages.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The number of overrides held (the start log's count).
    pub fn len(&self) -> usize {
        self.packages()
            .values()
            .map(|o| o.enabled.len() + o.disabled.len())
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// PackageSetting.getCurrentEnabledStateLPr: ENABLED or DISABLED when overridden, else DEFAULT.
    pub fn component(&self, package: &str, class: &str) -> i32 {
        match self.packages().get(package) {
            Some(o) if o.enabled.contains(class) => COMPONENT_ENABLED_STATE_ENABLED,
            Some(o) if o.disabled.contains(class) => COMPONENT_ENABLED_STATE_DISABLED,
            _ => COMPONENT_ENABLED_STATE_DEFAULT,
        }
    }

    /// setEnabledSettingInternalLocked for a component (PackageManagerService.java:4164-4180 -> PackageSetting enable/disable/restoreComponentLPw :1212-1244): ENABLED moves `class` to the enabled set,
    /// DISABLED to the disabled set, DEFAULT removes it from both. Answers whether anything changed; any other state changes nothing (AOSP logs it and answers false).
    pub fn set_component(&self, package: &str, class: &str, state: i32) -> bool {
        let mut packages = self.packages();
        let o = packages.entry(package.to_owned()).or_default();
        let changed = match state {
            COMPONENT_ENABLED_STATE_ENABLED => {
                o.disabled.remove(class) | o.enabled.insert(class.to_owned())
            }
            COMPONENT_ENABLED_STATE_DISABLED => {
                o.enabled.remove(class) | o.disabled.insert(class.to_owned())
            }
            COMPONENT_ENABLED_STATE_DEFAULT => o.enabled.remove(class) | o.disabled.remove(class),
            _ => false,
        };
        if o.enabled.is_empty() && o.disabled.is_empty() {
            packages.remove(package);
        }
        changed
    }

    /// Writes the state (Settings.writePackageRestrictionsLPr's role) through [`state_file::save`]. AOSP defers the write unless SYNCHRONOUS; here every change is written before the call returns (one
    /// small file), which keeps SYNCHRONOUS's guarantee for every caller. In memory only (no file): nothing to do.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        state_file::save(
            file,
            VERSION,
            &Stored {
                packages: self.packages().clone(),
            },
        )
    }
}

fn log(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "services: package state: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("atl-package-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    /// PackageUserStateUtils.isEnabled's table (app state DEFAULT): MATCH_DISABLED_COMPONENTS -> true for every row; `<application enabled=false>` -> false for every state; else ENABLED -> true,
    /// DISABLED -> false, DEFAULT (and the component states setEnabledSettingInternalLocked never stores) -> the manifest's value.
    #[test]
    fn is_enabled_table() {
        let states = [
            COMPONENT_ENABLED_STATE_DEFAULT,
            COMPONENT_ENABLED_STATE_ENABLED,
            COMPONENT_ENABLED_STATE_DISABLED,
            COMPONENT_ENABLED_STATE_DISABLED_USER,
            COMPONENT_ENABLED_STATE_DISABLED_UNTIL_USED,
        ];
        for state in states {
            for manifest in [false, true] {
                assert!(is_enabled(
                    false,
                    manifest,
                    state,
                    MATCH_DISABLED_COMPONENTS
                ));
                assert!(
                    !is_enabled(false, manifest, state, 0),
                    "disabled application, state {state}"
                );
                let want = match state {
                    COMPONENT_ENABLED_STATE_ENABLED => true,
                    COMPONENT_ENABLED_STATE_DISABLED => false,
                    _ => manifest,
                };
                assert_eq!(
                    is_enabled(true, manifest, state, 0),
                    want,
                    "state {state} manifest {manifest}"
                );
            }
        }
    }

    /// enable/disable/restoreComponentLPw: a change answers true, a repeat false; DEFAULT clears either override; DISABLED_USER on a component changes nothing (setEnabledSettingInternalLocked's default case).
    #[test]
    fn set_component_transitions() {
        let s = PackageUserStates::default();
        assert_eq!(s.component("p", "C"), COMPONENT_ENABLED_STATE_DEFAULT);
        assert!(s.set_component("p", "C", COMPONENT_ENABLED_STATE_DISABLED));
        assert!(!s.set_component("p", "C", COMPONENT_ENABLED_STATE_DISABLED));
        assert_eq!(s.component("p", "C"), COMPONENT_ENABLED_STATE_DISABLED);
        assert!(s.set_component("p", "C", COMPONENT_ENABLED_STATE_ENABLED));
        assert_eq!(s.component("p", "C"), COMPONENT_ENABLED_STATE_ENABLED);
        assert!(s.set_component("p", "C", COMPONENT_ENABLED_STATE_DEFAULT));
        assert!(!s.set_component("p", "C", COMPONENT_ENABLED_STATE_DEFAULT));
        assert_eq!(s.component("p", "C"), COMPONENT_ENABLED_STATE_DEFAULT);
        assert!(!s.set_component("p", "C", COMPONENT_ENABLED_STATE_DISABLED_USER));
        assert!(s.is_empty());
    }

    /// save -> load reads back each override; a package `installed` denies is dropped; the old file is replaced whole (no temp file left).
    #[test]
    fn save_load_round_trip() {
        let dir = scratch("round-trip");
        let s = PackageUserStates::load(&dir, |_| true);
        assert!(s.is_empty());
        s.set_component("a", "A1", COMPONENT_ENABLED_STATE_DISABLED);
        s.set_component("a", "A2", COMPONENT_ENABLED_STATE_ENABLED);
        s.set_component("gone", "G", COMPONENT_ENABLED_STATE_DISABLED);
        s.save().expect("save");
        assert!(!dir.join("package-state.json.tmp").exists());
        let back = PackageUserStates::load(&dir, |p| p != "gone");
        assert_eq!(back.component("a", "A1"), COMPONENT_ENABLED_STATE_DISABLED);
        assert_eq!(back.component("a", "A2"), COMPONENT_ENABLED_STATE_ENABLED);
        assert_eq!(back.component("gone", "G"), COMPONENT_ENABLED_STATE_DEFAULT);
        assert_eq!(back.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt file and a file of another version both load as the default state (counted), and the next save replaces them with a readable one.
    #[test]
    fn unreadable_file_is_default_state() {
        let dir = scratch("unreadable");
        std::fs::write(dir.join(FILE), b"{ not json").expect("plant");
        let refusals = || {
            atl_diag::counts()
                .into_iter()
                .find(|(_, api, _)| api == "package-state.load: unreadable file, default state")
                .map_or(0, |(_, _, n)| n)
        };
        let before = refusals();
        let s = PackageUserStates::load(&dir, |_| true);
        assert!(s.is_empty());
        assert_eq!(refusals(), before + 1);
        std::fs::write(
            dir.join(FILE),
            br#"{"version":99,"packages":{"a":{"disabled":["A"]}}}"#,
        )
        .expect("plant");
        let s = PackageUserStates::load(&dir, |_| true);
        assert_eq!(s.component("a", "A"), COMPONENT_ENABLED_STATE_DEFAULT);
        assert_eq!(refusals(), before + 2);
        s.set_component("a", "A", COMPONENT_ENABLED_STATE_DISABLED);
        s.save().expect("save");
        assert_eq!(
            PackageUserStates::load(&dir, |_| true).component("a", "A"),
            COMPONENT_ENABLED_STATE_DISABLED
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
