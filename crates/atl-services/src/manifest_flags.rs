// SPDX-License-Identifier: MIT
//! ApplicationInfo.flags and ActivityInfo.flags from the manifest's boolean attributes, as AOSP derives them: PackageInfoUtils.appInfoFlags (the application's, from ParsingPackageUtils' booleans)
//! and ParsedActivityUtils.parseActivityOrReceiver:133-158 (an activity's; its `flag(FLAG, attr, sa)` is `getBoolean(attr, false)`, ComponentParseUtils.java:148). A boolean the manifest wrote decides its bit;
//! an absent one takes AOSP's default, which may depend on targetSdkVersion or on the application's effective value of the same attribute.
//! Only the plain booleans are mirrored. Not set here: bits of other elements (`<supports-screens>`, install state such as FLAG_SYSTEM, FLAG_STOPPED), `persistent` (gated by
//! persistentWhenFeatureAvailable), the backup-agent booleans (killAfterRestore, restoreAnyVersion, fullBackupOnly), private flags, and visibleToInstantApps (it also marks the package).

use std::collections::BTreeMap;

use crate::packages::{ActivityDecl, Manifest};
use crate::size_buckets::{CONFIG_SCREEN_SIZE, CONFIG_SMALLEST_SCREEN_SIZE};

/// Build.VERSION_CODES.ICE_CREAM_SANDWICH and P: the targetSdkVersion levels two application defaults turn on at or below (ParsingPackageUtils:2428, :2430).
const ICE_CREAM_SANDWICH: i32 = 14;
const P: i32 = 28;
/// The sign bit: ApplicationInfo.FLAG_MULTIARCH and ActivityInfo.FLAG_ALLOW_EMBEDDED are `1 << 31`.
const BIT_31: i32 = i32::MIN;

/// What an absent boolean means.
#[derive(Clone, Copy)]
enum Fallback {
    On,
    Off,
    /// targetSdkVersion >= the level
    SdkAtLeast(i32),
    /// targetSdkVersion < the level
    SdkBelow(i32),
    /// the application's effective value of the same attribute (ParsedActivityUtils' `pkg.isXxx()` defaults)
    Application,
}

use Fallback::{Application, Off, On, SdkAtLeast, SdkBelow};

/// One row: a boolean attribute, the flag its `true` sets, and what it means when the manifest does not write it.
struct Row {
    attr: &'static str,
    flag: i32,
    absent: Fallback,
}

const fn row(attr: &'static str, flag: i32, absent: Fallback) -> Row {
    Row { attr, flag, absent }
}

/// ApplicationInfo.flags from `<application>`: the rows PackageInfoUtils.appInfoFlags takes from ParsingPackageUtils' booleans. Line numbers are ParsingPackageUtils.java's.
const APPLICATION: &[Row] = &[
    // FLAG_HAS_CODE, :2401
    row("hasCode", 1 << 2, On),
    // FLAG_HARDWARE_ACCELERATED, :2428
    row(
        "hardwareAccelerated",
        1 << 29,
        SdkAtLeast(ICE_CREAM_SANDWICH),
    ),
    // FLAG_ALLOW_BACKUP, :2395
    row("allowBackup", 1 << 15, On),
    // FLAG_DEBUGGABLE, :2406
    row("debuggable", 1 << 1, Off),
    // FLAG_VM_SAFE_MODE, :2420
    row("vmSafeMode", 1 << 14, Off),
    // FLAG_ALLOW_TASK_REPARENTING, :2403
    row("allowTaskReparenting", 1 << 5, Off),
    // FLAG_ALLOW_CLEAR_USER_DATA, :2396
    row("allowClearUserData", 1 << 6, On),
    // FLAG_LARGE_HEAP, :2412
    row("largeHeap", 1 << 20, Off),
    // FLAG_USES_CLEARTEXT_TRAFFIC, :2430
    row("usesCleartextTraffic", 1 << 27, SdkBelow(P)),
    // FLAG_SUPPORTS_RTL, :2416
    row("supportsRtl", 1 << 22, Off),
    // FLAG_TEST_ONLY, :2417
    row("testOnly", 1 << 8, Off),
    // FLAG_MULTIARCH, :2413
    row("multiArch", BIT_31, Off),
    // FLAG_EXTRACT_NATIVE_LIBS, :2400
    row("extractNativeLibs", 1 << 28, On),
    // FLAG_IS_GAME, :2410
    row("isGame", 1 << 25, Off),
];

/// ActivityInfo.flags from `<activity>` (ParsedActivityUtils.parseActivityOrReceiver:133-158, the activity branch; receivers' `singleUser` is not an activity's). Line numbers are ParsedActivityUtils.java's.
const ACTIVITY: &[Row] = &[
    // FLAG_ALLOW_TASK_REPARENTING, :133, default: the application's
    row("allowTaskReparenting", 0x0040, Application),
    // FLAG_ALWAYS_RETAIN_TASK_STATE, :134
    row("alwaysRetainTaskState", 0x0008, Off),
    // FLAG_CLEAR_TASK_ON_LAUNCH, :135
    row("clearTaskOnLaunch", 0x0004, Off),
    // FLAG_EXCLUDE_FROM_RECENTS, :136
    row("excludeFromRecents", 0x0020, Off),
    // FLAG_FINISH_ON_CLOSE_SYSTEM_DIALOGS, :137
    row("finishOnCloseSystemDialogs", 0x0100, Off),
    // FLAG_FINISH_ON_TASK_LAUNCH, :138
    row("finishOnTaskLaunch", 0x0002, Off),
    // FLAG_IMMERSIVE, :139
    row("immersive", 0x0800, Off),
    // FLAG_MULTIPROCESS, :140
    row("multiprocess", 0x0001, Off),
    // FLAG_NO_HISTORY, :141
    row("noHistory", 0x0080, Off),
    // FLAG_SHOW_FOR_ALL_USERS, :142
    row("showForAllUsers", 0x0400, Off),
    // FLAG_SHOW_FOR_ALL_USERS as AOSP maps showOnLockScreen too, :143
    row("showOnLockScreen", 0x0400, Off),
    // FLAG_STATE_NOT_NEEDED, :144
    row("stateNotNeeded", 0x0010, Off),
    // FLAG_SYSTEM_USER_ONLY, :145
    row("systemUserOnly", 0x2000_0000, Off),
    // FLAG_HARDWARE_ACCELERATED, :148, default: the application's
    row("hardwareAccelerated", 0x0200, Application),
    // FLAG_ALLOW_EMBEDDED, :149
    row("allowEmbedded", BIT_31, Off),
    // FLAG_ALWAYS_FOCUSABLE, :150
    row("alwaysFocusable", 0x4_0000, Off),
    // FLAG_AUTO_REMOVE_FROM_RECENTS, :151
    row("autoRemoveFromRecents", 0x2000, Off),
    // FLAG_RELINQUISH_TASK_IDENTITY, :152
    row("relinquishTaskIdentity", 0x1000, Off),
    // FLAG_RESUME_WHILE_PAUSING, :153
    row("resumeWhilePausing", 0x4000, Off),
    // FLAG_SHOW_WHEN_LOCKED, :154
    row("showWhenLocked", 0x80_0000, Off),
    // FLAG_SUPPORTS_PICTURE_IN_PICTURE, :155
    row("supportsPictureInPicture", 0x40_0000, Off),
    // FLAG_TURN_SCREEN_ON, :156
    row("turnScreenOn", 0x100_0000, Off),
    // FLAG_PREFER_MINIMAL_POST_PROCESSING, :157
    row("preferMinimalPostProcessing", 0x200_0000, Off),
    // FLAG_ALLOW_UNTRUSTED_ACTIVITY_EMBEDDING, :158
    row("allowUntrustedActivityEmbedding", 0x1000_0000, Off),
];

/// Whether `row` is set, given the booleans its element wrote (the application's or an activity's) and the manifest's SDK levels.
fn is_set(row: &Row, written: &BTreeMap<String, bool>, m: &Manifest) -> bool {
    if let Some(v) = written.get(row.attr) {
        return *v;
    }
    match row.absent {
        On => true,
        Off => false,
        SdkAtLeast(level) => m.target_sdk >= level,
        SdkBelow(level) => m.target_sdk < level,
        Application => application_is_set(m, row.attr),
    }
}

/// The application's effective value of `attr`, from its APPLICATION row.
fn application_is_set(m: &Manifest, attr: &str) -> bool {
    APPLICATION
        .iter()
        .find(|r| r.attr == attr)
        .is_some_and(|r| is_set(r, &m.application_booleans, m))
}

/// ApplicationInfo.flags of `m`'s package (PackageInfoUtils.appInfoFlags over the APPLICATION rows). The install-state bits are not set here.
pub fn application_flags(m: &Manifest) -> i32 {
    APPLICATION
        .iter()
        .filter(|r| is_set(r, &m.application_booleans, m))
        .fold(0, |flags, r| flags | r.flag)
}

/// ActivityInfo.flags of `a`, an activity of `m` (ParsedActivityUtils over the ACTIVITY rows).
pub fn activity_flags(m: &Manifest, a: &ActivityDecl) -> i32 {
    ACTIVITY
        .iter()
        .filter(|r| is_set(r, &a.booleans, m))
        .fold(0, |flags, r| flags | r.flag)
}

/// Build.VERSION_CODES.HONEYCOMB_MR2: apps below it predate the size bits, so they handle size changes.
const HONEYCOMB_MR2: i32 = 13;

/// ActivityInfo.getRealConfigChanged of `a`: what it handles itself, the bits a relaunch decision reads.
pub fn real_config_changes(m: &Manifest, a: &ActivityDecl) -> u32 {
    let declared = a.config_changes as u32;
    if m.target_sdk < HONEYCOMB_MR2 {
        declared | CONFIG_SCREEN_SIZE | CONFIG_SMALLEST_SCREEN_SIZE
    } else {
        declared
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ActivityInfo.getRealConfigChanged: an app below 13 also handles screenSize (0x400) and smallestScreenSize (0x800); 13 on, only what it declares.
    #[test]
    fn real_config_changes_add_the_size_bits_below_13() {
        let a = ActivityDecl {
            config_changes: 0x3,
            ..activity(&[])
        };
        assert_eq!(real_config_changes(&manifest(12, &[]), &a), 0xC03);
        assert_eq!(real_config_changes(&manifest(13, &[]), &a), 0x3);
    }

    fn manifest(target_sdk: i32, application: &[(&str, bool)]) -> Manifest {
        Manifest {
            name: "com.example.flags".into(),
            version_code: 1,
            version_name: None,
            min_sdk: 21,
            target_sdk,
            process_name: None,
            application_theme: 0,
            application_class: None,
            app_component_factory: None,
            application_meta_data: vec![],
            application_booleans: application
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect(),
            activities: vec![],
            receivers: vec![],
            requested_permissions: vec![],
            permissions: Vec::new(),
            libraries: vec![],
            instrumentation: vec![],
        }
    }

    fn activity(booleans: &[(&str, bool)]) -> ActivityDecl {
        ActivityDecl {
            name: "com.example.flags.Main".into(),
            launcher: true,
            theme: 0,
            config_changes: 0,
            booleans: booleans
                .iter()
                .map(|(k, v)| ((*k).to_owned(), *v))
                .collect(),
            launch_mode: 0,
            task_affinity: None,
            process: None,
            meta_data: vec![],
        }
    }

    /// No attributes, targetSdkVersion 36: the defaults ParsingPackageUtils:2395-2430 gives hasCode, allowBackup, allowClearUserData and extractNativeLibs, and hardwareAccelerated from 14 on:
    /// 0x4 | 0x8000 | 0x40 | 0x1000_0000 | 0x2000_0000.
    #[test]
    fn application_defaults() {
        assert_eq!(application_flags(&manifest(36, &[])), 0x3000_8044);
    }

    /// targetSdkVersion 21: usesCleartextTraffic is on below 28 and hardwareAccelerated on from 14. An explicit `false` clears a default, an explicit `true` sets a clear one.
    /// Expected: 0x8000 | 0x40 | 0x1000_0000 (extractNativeLibs) | 0x0800_0000 (usesCleartextTraffic) | 0x100 (testOnly).
    #[test]
    fn application_booleans_override_the_defaults() {
        let m = manifest(
            21,
            &[
                ("hardwareAccelerated", false),
                ("hasCode", false),
                ("testOnly", true),
            ],
        );
        assert_eq!(application_flags(&m), 0x1800_8140);
    }

    /// An activity with no attribute takes the application's hardwareAccelerated; its own attribute decides when written.
    #[test]
    fn activity_hardware_acceleration_follows_the_application_unless_written() {
        let on = manifest(36, &[]);
        assert_eq!(activity_flags(&on, &activity(&[])), 0x200);
        assert_eq!(
            activity_flags(&on, &activity(&[("hardwareAccelerated", false)])),
            0
        );
        let off = manifest(36, &[("hardwareAccelerated", false)]);
        assert_eq!(activity_flags(&off, &activity(&[])), 0);
        assert_eq!(
            activity_flags(&off, &activity(&[("hardwareAccelerated", true)])),
            0x200
        );
    }

    /// Each activity boolean sets its own bit; showOnLockScreen sets FLAG_SHOW_FOR_ALL_USERS as AOSP maps it, allowEmbedded is the sign bit.
    #[test]
    fn activity_booleans_set_their_bits() {
        let flags = activity_flags(
            &manifest(36, &[]),
            &activity(&[
                ("immersive", true),
                ("multiprocess", true),
                ("showOnLockScreen", true),
                ("allowEmbedded", true),
                ("turnScreenOn", true),
            ]),
        );
        // 0x800 | 0x1 | 0x400 | 0x8000_0000 | 0x100_0000, plus the application's hardwareAccelerated (0x200)
        assert_eq!(flags, 0x800 | 0x1 | 0x400 | i32::MIN | 0x100_0000 | 0x200);
    }

    /// An activity row that falls back to the application names one of the application's rows, so the fallback is never silently false.
    #[test]
    fn application_fallbacks_name_application_rows() {
        for r in ACTIVITY {
            if matches!(r.absent, Application) {
                assert!(APPLICATION.iter().any(|a| a.attr == r.attr), "{}", r.attr);
            }
        }
    }
}
