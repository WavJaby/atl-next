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

/// A uid that is not an application's (root, system, shell, ...): AppsFilterImpl.shouldFilterApplication filters nothing for `callingAppId < Process.FIRST_APPLICATION_UID`.
pub fn sees_all_packages(uid: u32) -> bool {
    app_id(uid) < FIRST_APPLICATION_UID
}

/// Whether `uid` holds `permissions` (all of them): granted to root / system; denied to every other uid. No M2 method carries @EnforcePermission (aidl lists none on the M2 subset of
/// IActivityManager, IPackageManager, IActivityClientController), so no app permission is needed yet; an app-held permission (manifest + grants via PermissionController, plan Q5) comes with the first method that needs one.
pub fn holds(uid: u32, permissions: &[&str]) -> bool {
    let _ = permissions;
    is_system_or_root(uid)
}

/// PermissionInfo.PROTECTION_MASK_BASE and the two base types the install-time rule grants (PermissionInfo.java).
const PROTECTION_MASK_BASE: u32 = 0xf;
const PROTECTION_NORMAL: u32 = 0;
const PROTECTION_SIGNATURE: u32 = 2;

/// The install-time grants of `package` (owner decision 2026-11-05, interim rule A; PermissionManagerServiceImpl.restorePermissionState): a requested permission is granted when its definition
/// is NORMAL, or SIGNATURE and defined by `package` itself (same package -> same certificate). Every other requested permission (dangerous: runtime grant via PermissionController, not built;
/// signature of another signer; undefined) is not. `definitions` = the platform's (framework-res) then each installed package's, as (defining package, declaration); the first definition wins (PMS keeps the first).
pub fn install_grants<'a>(
    package: &str,
    requested: &[String],
    definitions: impl IntoIterator<Item = (&'a str, &'a crate::packages::PermissionDecl)>,
) -> Vec<String> {
    let mut defined: std::collections::BTreeMap<&str, (&str, u32)> = Default::default();
    for (owner, d) in definitions {
        defined
            .entry(d.name.as_str())
            .or_insert((owner, d.protection_level & PROTECTION_MASK_BASE));
    }
    requested
        .iter()
        .filter(|p| match defined.get(p.as_str()) {
            Some((_, PROTECTION_NORMAL)) => true,
            Some((owner, PROTECTION_SIGNATURE)) => *owner == package,
            _ => false,
        })
        .cloned()
        .collect()
}

/// ActivityManager.checkComponentPermission for an exported, uid-less component (ActivityManagerService.checkPermissionForDevice:6193 -> checkComponentPermission): no permission -> denied;
/// root / system -> granted; else granted only when `uid`'s install-time grants hold it.
pub fn check(uid: u32, permission: Option<&str>, grants: &[String]) -> bool {
    let Some(permission) = permission else {
        return false;
    };
    is_system_or_root(uid) || grants.iter().any(|g| g == permission)
}

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

    /// Levels as aapt2 prints framework-res's: INTERNET normal (0x0), READ_CONTACTS dangerous (0x1), WRITE_SECURE_SETTINGS signature|privileged (0x12); an app's own signature permission (0x2).
    #[test]
    fn install_grants_normal_and_own_signature_only() {
        let platform = [
            decl("android.permission.INTERNET", 0x0),
            decl("android.permission.READ_CONTACTS", 0x1),
            decl("android.permission.WRITE_SECURE_SETTINGS", 0x12),
        ];
        let own = [decl("org.x.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION", 0x2)];
        let other = [decl("org.y.SECRET", 0x2)];
        let requested: Vec<String> = [
            "android.permission.INTERNET",
            "android.permission.READ_CONTACTS",
            "android.permission.WRITE_SECURE_SETTINGS",
            "org.x.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION",
            "org.y.SECRET",
            "org.z.UNDEFINED",
        ]
        .map(String::from)
        .into();
        let defs = platform
            .iter()
            .map(|d| ("android", d))
            .chain(own.iter().map(|d| ("org.x", d)))
            .chain(other.iter().map(|d| ("org.y", d)));
        assert_eq!(
            install_grants("org.x", &requested, defs),
            [
                "android.permission.INTERNET",
                "org.x.DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION"
            ]
        );
    }

    #[test]
    fn check_denies_null_and_ungranted() {
        let grants = vec!["android.permission.INTERNET".to_owned()];
        assert!(!check(10_000, None, &grants));
        assert!(!check(1000, None, &grants));
        assert!(check(1000, Some("android.permission.READ_CONTACTS"), &[]));
        assert!(check(10_000, Some("android.permission.INTERNET"), &grants));
        assert!(!check(
            10_000,
            Some("android.permission.READ_CONTACTS"),
            &grants
        ));
    }

    #[test]
    fn system_and_root_in_any_user_only() {
        // uids: AID_ROOT, AID_SYSTEM, AID_SYSTEM in user 10 (10 * 100000 + 1000), AID_SHELL 2000, the first app uid, an app in user 10
        assert!(holds(0, &["A"]));
        assert!(holds(1000, &["A", "B"]));
        assert!(holds(1_001_000, &["A"]));
        assert!(!holds(2000, &["A"]));
        assert!(!holds(10_000, &["A"]));
        assert!(!holds(1_010_123, &[]));
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
