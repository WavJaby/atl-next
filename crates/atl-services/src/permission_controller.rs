// SPDX-License-Identifier: MIT
//! The `permission` service (IPermissionController): AOSP's ActivityManagerService.PermissionController (ActivityManagerService.java:6045-6055), registered as "permission" (:1972).
//! checkPermission(permission, pid, uid) answers from `Permissions::check`, the grants permissionmgr answers from: a null name is denied (ActivityManagerService.java:6195-6196; the AIDL String arrives as Option),
//! root and system hold every permission (ActivityManager.java:5228-5231), any other uid holds what its granted state holds.
//! keystore2 is the caller that needs it: check_android_permission (system/security/keystore2/src/utils.rs:178-196) asks for READ_PRIVILEGED_PHONE_STATE of the app that attests a device identifier, and false is CANNOT_ATTEST_IDS (-66), the answer an app without the permission gets (utils.rs:143-148).
//! Not modelled: pid (AOSP's MY_PID grant, ActivityManagerService.java:6131, admits system_server's own calls, which do not come through this binder) and per-process denied permissions (:6141-6146, no producer in this runtime).
//! noteOp, getPackagesForUid, isRuntimePermission and getPackageUid stay unimplemented (UNKNOWN_TRANSACTION, counted by atl-diag): no caller here makes them.

use std::sync::Arc;

use atl_aidl::aidl::android::os::IPermissionController::{
    BnPermissionController, IPermissionController,
};
use binder::{BinderFeatures, Interface, SpIBinder};

use crate::packages::Permissions;

/// The AIDL interface name the refusal counter is keyed by (atl-diag `answered`), as user.rs names IUserManager.
const INTERFACE: &str = "IPermissionController";

struct PermissionControllerService {
    permissions: Arc<Permissions>,
}

impl Interface for PermissionControllerService {}

impl IPermissionController for PermissionControllerService {
    fn r#checkPermission(
        &self,
        permission: Option<&str>,
        _pid: i32,
        uid: i32,
    ) -> binder::Result<bool> {
        Ok(check(&self.permissions, permission, uid))
    }
}

/// PermissionController.checkPermission over the uid's grants (`Permissions::check`: a null name is not held, root / system hold all, else the granted state).
/// A negative uid is no application's (Process.INVALID_UID, -1) and is denied; every denial is counted.
fn check(permissions: &Permissions, permission: Option<&str>, uid: i32) -> bool {
    let granted = u32::try_from(uid).is_ok_and(|uid| permissions.check(uid, permission));
    if !granted {
        atl_diag::answered(INTERFACE, "checkPermission denied");
    }
    granted
}

/// The service's binder, registered as `permission` (system_services.rs).
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnPermissionController::new_binder(
        PermissionControllerService { permissions },
        BinderFeatures::default(),
    )
    .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::Packages;
    use crate::packages::fixture::record;
    use atl_services_types::packages::PermissionDecl;
    use std::collections::BTreeMap;

    const READ_PRIVILEGED_PHONE_STATE: &str = "android.permission.READ_PRIVILEGED_PHONE_STATE";
    const INTERNET: &str = "android.permission.INTERNET";
    /// PermissionInfo.PROTECTION_SIGNATURE (2) | PROTECTION_FLAG_PRIVILEGED (0x10), as framework-res declares READ_PRIVILEGED_PHONE_STATE.
    const SIGNATURE_PRIVILEGED: u32 = 0x12;
    /// PermissionInfo.PROTECTION_NORMAL: granted at install to any requester.
    const NORMAL: u32 = 0;
    /// Process.ROOT_UID, Process.SYSTEM_UID; APP_UID is record()'s uid (its package's app id).
    const ROOT_UID: i32 = 0;
    const SYSTEM_UID: i32 = 1000;
    const APP_UID: i32 = 10_001;
    /// Process.INVALID_UID.
    const INVALID_UID: i32 = -1;

    fn platform() -> [PermissionDecl; 2] {
        [
            (READ_PRIVILEGED_PHONE_STATE, SIGNATURE_PRIVILEGED),
            (INTERNET, NORMAL),
        ]
        .map(|(name, protection_level)| PermissionDecl {
            name: name.into(),
            protection_level,
        })
    }

    /// The refusal counter's current value (atl-diag counts are process-wide: a test asserts it moved by at least its own refusals).
    fn refusals() -> u64 {
        atl_diag::counts()
            .iter()
            .find(|(outcome, api, _)| {
                outcome == "answered" && api == "IPermissionController.checkPermission denied"
            })
            .map_or(0, |(_, _, n)| *n)
    }

    /// ActivityManager.checkComponentPermission (ActivityManager.java:5228-5231): root and system are granted every permission without a state.
    /// A null name is denied for them too: ActivityManagerService.checkPermission returns DENIED before the uid check (:6195-6196).
    #[test]
    fn root_and_system_hold_every_permission_but_a_null_name() {
        let packages = Packages::new(vec![record("org.x")], BTreeMap::new());
        let permissions = Permissions::new(&packages, &platform());
        assert!(check(
            &permissions,
            Some(READ_PRIVILEGED_PHONE_STATE),
            ROOT_UID
        ));
        assert!(check(
            &permissions,
            Some(READ_PRIVILEGED_PHONE_STATE),
            SYSTEM_UID
        ));
        assert!(!check(&permissions, None, ROOT_UID));
    }

    /// An app holds what its state holds: a normal permission it requested is granted at install; a signature-privileged one only to the package that defines it (initial_states), so an app
    /// requesting READ_PRIVILEGED_PHONE_STATE is refused, and the refusal is counted.
    #[test]
    fn app_holds_what_its_state_grants() {
        let mut r = record("org.x");
        r.info.requested_permissions = Some(vec![
            Some(READ_PRIVILEGED_PHONE_STATE.into()),
            Some(INTERNET.into()),
        ]);
        let packages = Packages::new(vec![r], BTreeMap::new());
        let permissions = Permissions::new(&packages, &platform());
        assert!(check(&permissions, Some(INTERNET), APP_UID));
        let before = refusals();
        assert!(!check(
            &permissions,
            Some(READ_PRIVILEGED_PHONE_STATE),
            APP_UID
        ));
        assert!(refusals() > before);
    }

    /// A negative uid and an app id with no package are refused and counted: two refusals, so the counter moves by at least two.
    #[test]
    fn negative_and_unknown_uids_are_refused_and_counted() {
        let packages = Packages::new(vec![record("org.x")], BTreeMap::new());
        let permissions = Permissions::new(&packages, &platform());
        let before = refusals();
        assert!(!check(&permissions, Some(INTERNET), INVALID_UID));
        assert!(!check(&permissions, Some(INTERNET), APP_UID + 1));
        assert!(refusals() >= before + 2);
    }
}
