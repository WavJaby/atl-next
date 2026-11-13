// SPDX-License-Identifier: MIT
//! Caller identity and the `@EnforcePermission` policy, by caller uid (no SELinux, no PermissionController yet).
//! Uid layout: system/core/libcutils/include/private/android_filesystem_config.h (AID_ROOT 0, AID_SYSTEM 1000, AID_APP_START 10000) and UserHandle.PER_USER_RANGE 100000.

/// UserHandle.PER_USER_RANGE: uid = user * range + app id.
pub const PER_USER_RANGE: u32 = 100_000;
/// AID_ROOT / AID_SYSTEM.
pub const ROOT_UID: u32 = 0;
pub const SYSTEM_UID: u32 = 1000;

/// UserHandle.getAppId.
pub fn app_id(uid: u32) -> u32 {
    uid % PER_USER_RANGE
}

/// UserHandle.getUserId.
pub fn user_id(uid: u32) -> u32 {
    uid / PER_USER_RANGE
}

/// Root or the system uid, in any user: what ActivityManager.checkComponentPermission (frameworks/base core/java/android/app/ActivityManager.java) grants every permission without asking,
/// and PackageManagerServiceUtils.isSystemOrRoot accepts.
pub fn is_system_or_root(uid: u32) -> bool {
    matches!(app_id(uid), ROOT_UID | SYSTEM_UID)
}

/// Process.FIRST_APPLICATION_UID (AID_APP_START).
pub const FIRST_APPLICATION_UID: u32 = 10_000;
/// AID_SHELL: the uid `adb shell` (and `am`) runs as.
pub const SHELL_UID: u32 = 2000;
/// Process.INVALID_UID: the wire's "no uid" (signed, as the AIDL int fields carry it).
pub const INVALID_UID: i32 = -1;

/// A uid that is not an application's (root, system, shell, ...): AppsFilterImpl.shouldFilterApplication filters nothing for `callingAppId < Process.FIRST_APPLICATION_UID`.
pub fn sees_all_packages(uid: u32) -> bool {
    app_id(uid) < FIRST_APPLICATION_UID
}

/// An `@EnforcePermission` check (the generated stub's Single / AllOf / AnyOf; Single = a one-element AllOf): root / system hold everything (checkComponentPermission); any other uid
/// holds a permission only when `granted` says so -- its granted permission states (`initial_states`, then runtime grants). `any` = AnyOf. No permission at all is held by nobody but root / system.
pub fn holds(uid: u32, permissions: &[&str], any: bool, granted: impl Fn(&str) -> bool) -> bool {
    if is_system_or_root(uid) {
        return true;
    }
    if permissions.is_empty() {
        return false;
    }
    if any {
        permissions.iter().any(|p| granted(p))
    } else {
        permissions.iter().all(|p| granted(p))
    }
}

/// PermissionInfo.PROTECTION_MASK_BASE and its base types (PermissionInfo.java): DANGEROUS is what Permission.isRuntime tests.
const PROTECTION_MASK_BASE: u32 = 0xf;
const PROTECTION_NORMAL: u32 = 0;
const PROTECTION_DANGEROUS: u32 = 1;
const PROTECTION_SIGNATURE: u32 = 2;

/// PackageManager.FLAG_PERMISSION_REVOKED_COMPAT (= FLAG_PERMISSION_REVOKE_ON_UPGRADE, 1 << 3) and FLAG_PERMISSION_REVIEW_REQUIRED (1 << 6).
pub const FLAG_PERMISSION_REVOKED_COMPAT: u32 = 1 << 3;
pub const FLAG_PERMISSION_REVIEW_REQUIRED: u32 = 1 << 6;
/// Build.VERSION_CODES.M: the first targetSdk that supports runtime permissions.
pub const TARGET_SDK_RUNTIME_PERMISSIONS: i32 = 23;
/// PermissionManagerServiceImpl.PLATFORM_PACKAGE_NAME: the defining package of framework-res's permissions.
pub const PLATFORM_PACKAGE: &str = "android";

/// The registry's view of one permission (PermissionRegistry / Permission): who defines it and its base protection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub owner: String,
    base: u32,
}

impl Definition {
    /// Permission.isRuntime: base protection DANGEROUS.
    pub fn is_runtime(&self) -> bool {
        self.base == PROTECTION_DANGEROUS
    }
}

/// The permission registry by name.
pub type Registry = std::collections::BTreeMap<String, Definition>;

/// The permission registry: `definitions` = the platform's (framework-res) then each installed package's, as (defining package, name, protectionLevel); the first definition wins (PMS keeps the first).
pub fn registry<'a>(definitions: impl IntoIterator<Item = (&'a str, &'a str, u32)>) -> Registry {
    let mut defined = Registry::new();
    for (owner, name, level) in definitions {
        defined.entry(name.to_owned()).or_insert(Definition {
            owner: owner.to_owned(),
            base: level & PROTECTION_MASK_BASE,
        });
    }
    defined
}

/// One permission of a uid's state (UidPermissionState's PermissionState): granted or not, its PackageManager.FLAG_PERMISSION_* flags, and whether it is a runtime one (changeable by grant/revoke).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionState {
    pub name: String,
    pub runtime: bool,
    pub granted: bool,
    pub flags: u32,
}

/// The first permission state of `package` after its install (PermissionManagerServiceImpl.restorePermissionState with no previous state; install-time half: owner decision 2026-11-05, interim rule A).
/// - Install permission: granted when NORMAL, or SIGNATURE and defined by `package` itself (same package -> same certificate); a signature permission of another signer gets no state.
/// - Runtime (DANGEROUS) permission: targetSdk >= M -> not granted, flags 0 (the user grants it through PermissionController); legacy targetSdk < M -> granted, and a platform-defined one carries
///   REVIEW_REQUIRED | REVOKED_COMPAT (the review the user confirms once).
/// - An undefined permission gets no state. Not modelled: restricted permissions (hard/soft restriction flags), privileged / role / development permissions, split permissions.
pub fn initial_states<'a>(
    package: &str,
    target_sdk: i32,
    requested: impl IntoIterator<Item = &'a str>,
    registry: &Registry,
) -> Vec<PermissionState> {
    let legacy = target_sdk < TARGET_SDK_RUNTIME_PERMISSIONS;
    requested
        .into_iter()
        .filter_map(|p| {
            let d = registry.get(p)?;
            let state = |runtime, granted, flags| PermissionState {
                name: p.to_owned(),
                runtime,
                granted,
                flags,
            };
            match d.base {
                PROTECTION_NORMAL => Some(state(false, true, 0)),
                PROTECTION_SIGNATURE if d.owner == package => Some(state(false, true, 0)),
                PROTECTION_DANGEROUS if !legacy => Some(state(true, false, 0)),
                PROTECTION_DANGEROUS if d.owner == PLATFORM_PACKAGE => Some(state(
                    true,
                    true,
                    FLAG_PERMISSION_REVIEW_REQUIRED | FLAG_PERMISSION_REVOKED_COMPAT,
                )),
                PROTECTION_DANGEROUS => Some(state(true, true, 0)),
                _ => None,
            }
        })
        .collect()
}

/// Manifest.permission.BROADCAST_STICKY: protectionLevel `normal` (core/res/AndroidManifest.xml:4917-4920) -> install-granted on request (`initial_states`).
pub const BROADCAST_STICKY: &str = "android.permission.BROADCAST_STICKY";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::PermissionDecl;

    fn decl(name: &str, level: u32) -> PermissionDecl {
        PermissionDecl {
            name: name.into(),
            protection_level: level,
        }
    }

    /// Levels as aapt2 prints framework-res's: INTERNET normal (0x0), POST_NOTIFICATIONS dangerous (0x1), WRITE_SECURE_SETTINGS signature|privileged (0x12); an app's own signature permission
    /// (0x2), another app's signature permission, an app-defined dangerous one.
    fn platform_and_apps() -> [(&'static str, Vec<PermissionDecl>); 3] {
        [
            (
                "android",
                vec![
                    decl("android.permission.INTERNET", 0x0),
                    decl("android.permission.POST_NOTIFICATIONS", 0x1),
                    decl("android.permission.WRITE_SECURE_SETTINGS", 0x12),
                ],
            ),
            (
                "org.x",
                vec![
                    decl("org.x.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION", 0x2),
                    decl("org.x.DANGER", 0x1),
                ],
            ),
            ("org.y", vec![decl("org.y.SECRET", 0x2)]),
        ]
    }

    const REQUESTED: [&str; 7] = [
        "android.permission.INTERNET",
        "android.permission.POST_NOTIFICATIONS",
        "android.permission.WRITE_SECURE_SETTINGS",
        "org.x.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION",
        "org.x.DANGER",
        "org.y.SECRET",
        "org.z.UNDEFINED",
    ];

    /// (name, runtime, granted, flags) of `initial_states("org.x", target_sdk, REQUESTED)`.
    fn states(target_sdk: i32) -> Vec<(String, bool, bool, u32)> {
        let defs = platform_and_apps();
        let registry = registry(defs.iter().flat_map(|(owner, list)| {
            list.iter()
                .map(move |d| (*owner, d.name.as_str(), d.protection_level))
        }));
        initial_states("org.x", target_sdk, REQUESTED, &registry)
            .into_iter()
            .map(|s| (s.name, s.runtime, s.granted, s.flags))
            .collect()
    }

    fn row(name: &str, runtime: bool, granted: bool, flags: u32) -> (String, bool, bool, u32) {
        (name.to_owned(), runtime, granted, flags)
    }

    /// restorePermissionState, targetSdk >= M (36, AB2's): normal and own-signature granted at install; dangerous ones runtime, not granted, flags 0 (appSupportsRuntimePermissions branch keeps
    /// the absent previous state); another signer's signature, privileged-signature (no allowlist modelled) and undefined permissions get no state.
    #[test]
    fn modern_app_runtime_permissions_start_denied() {
        assert_eq!(
            states(36),
            [
                row("android.permission.INTERNET", false, true, 0),
                row("android.permission.POST_NOTIFICATIONS", true, false, 0),
                row(
                    "org.x.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION",
                    false,
                    true,
                    0
                ),
                row("org.x.DANGER", true, false, 0),
            ]
        );
    }

    /// restorePermissionState, targetSdk < M (22): every runtime permission granted; a new platform-defined one carries REVIEW_REQUIRED | REVOKED_COMPAT = 0x40 | 0x8 (PackageManager.java:5300, :5330,
    /// :5423); an app-defined one no flags. M itself (23) is the first non-legacy target.
    #[test]
    fn legacy_app_runtime_permissions_are_granted_for_review() {
        assert_eq!(
            states(22),
            [
                row("android.permission.INTERNET", false, true, 0),
                row("android.permission.POST_NOTIFICATIONS", true, true, 0x48),
                row(
                    "org.x.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION",
                    false,
                    true,
                    0
                ),
                row("org.x.DANGER", true, true, 0),
            ]
        );
        assert_eq!(states(23), states(36));
    }

    #[test]
    fn app_holds_its_grants_all_or_any() {
        let granted = |p: &str| p == "A";
        assert!(holds(10_000, &["A"], false, granted));
        assert!(!holds(10_000, &["A", "B"], false, granted));
        assert!(holds(10_000, &["B", "A"], true, granted));
        assert!(!holds(10_000, &["B"], true, granted));
        assert!(!holds(10_000, &[], false, granted));
    }

    #[test]
    fn system_and_root_in_any_user_hold_everything() {
        // uids: AID_ROOT, AID_SYSTEM, AID_SYSTEM in user 10 (10 * 100000 + 1000), AID_SHELL 2000, the first app uid, an app in user 10
        let none = |_: &str| false;
        assert!(holds(0, &["A"], false, none));
        assert!(holds(1000, &["A", "B"], false, none));
        assert!(holds(1_001_000, &["A"], true, none));
        assert!(!holds(2000, &["A"], false, none));
        assert!(!holds(10_000, &["A"], false, none));
        assert!(!holds(1_010_123, &[], false, none));
        assert_eq!((app_id(1_010_123), user_id(1_010_123)), (10_123, 10));
    }

    #[test]
    fn packages_are_filtered_for_applications_only() {
        assert!(
            sees_all_packages(ROOT_UID)
                && sees_all_packages(SYSTEM_UID)
                && sees_all_packages(SHELL_UID)
        );
        assert!(!sees_all_packages(FIRST_APPLICATION_UID) && !sees_all_packages(1_010_123));
    }
}
