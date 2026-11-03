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

#[cfg(test)]
mod tests {
    use super::*;

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
