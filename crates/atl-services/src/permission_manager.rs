// SPDX-License-Identifier: MIT
//! The `permissionmgr` service (IPermissionManager): PermissionManager's constructor fetches it with getServiceOrThrow (PermissionManager.java:279), so without it getSystemService(PermissionManager) is null and PackageManager.checkPermission(perm, pkg) (PermissionManager.checkPackageNamePermission, PermissionManager.java:2037 -> checkPackageNamePermissionUncached, :2006 -> IPermissionManager.checkPermission) throws NPE (Cut the Rope's ZSystemInfo.getNetworkType, from native code).
//! Answers from the PermissionManager state (`packages::Permissions`: install-time states, runtime states, runtime grants) as PermissionManagerServiceImpl does: checkPermission (:936, checkPermissionInternal :948, then FULLER_PERMISSION_MAP :232), getPermissionFlags, grantRuntimePermission.
//! Not modelled: package visibility (filterAppAccess), instant apps, device ids, revoke (plan runtime-permissions S6: AOSP kills the app), flag updates. Every other method stays counted.
//! The @EnforcePermission methods (IPermissionManager.aidl:84, MANAGE_ONE_TIME_PERMISSION_SESSIONS) take the caller rule every service uses (permission.rs holds).

use atl_aidl::aidl::android::permission::IPermissionManager::{
    BnPermissionManager, EnforcePermission, IPermissionManager,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};
use std::sync::Arc;

use crate::activity::log;
use crate::packages::{GrantError, Packages, Permissions};

/// PackageManager.PERMISSION_GRANTED / PERMISSION_DENIED.
const GRANTED: i32 = 0;
const DENIED: i32 = -1;
/// UserHandle.USER_SYSTEM: the only user this runtime has (user.rs).
const USER_SYSTEM: i32 = 0;
/// PermissionManagerServiceImpl.FULLER_PERMISSION_MAP: holding the value grants the key.
const FULLER: [(&str, &str); 2] = [
    (
        "android.permission.ACCESS_COARSE_LOCATION",
        "android.permission.ACCESS_FINE_LOCATION",
    ),
    (
        "android.permission.INTERACT_ACROSS_USERS",
        "android.permission.INTERACT_ACROSS_USERS_FULL",
    ),
];
/// Manifest.permission: what grantRuntimePermission enforces, and the three getPermissionFlags accepts (enforceGrantRevokeGetRuntimePermissionPermissions).
const GRANT_RUNTIME_PERMISSIONS: &str = "android.permission.GRANT_RUNTIME_PERMISSIONS";
const REVOKE_RUNTIME_PERMISSIONS: &str = "android.permission.REVOKE_RUNTIME_PERMISSIONS";
const GET_RUNTIME_PERMISSIONS: &str = "android.permission.GET_RUNTIME_PERMISSIONS";

struct PermissionManagerService {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
}

impl PermissionManagerService {
    /// Context.enforceCallingOrSelfPermission: a SecurityException naming the method unless the caller holds `perms` (`any` = one of them).
    fn enforce(&self, method: &str, perms: &[&str], any: bool) -> binder::Result<()> {
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {perms:?}")),
        ))
    }
}

impl Interface for PermissionManagerService {}

impl IPermissionManager for PermissionManagerService {
    /// @EnforcePermission: root / system, or a caller holding the permissions (Permissions::holds).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let (perms, any): (&[&str], bool) = match &permission {
            EnforcePermission::Single(p) => (std::slice::from_ref(p), false),
            EnforcePermission::AllOf(p) => (p, false),
            EnforcePermission::AnyOf(p) => (p, true),
        };
        self.enforce(method, perms, any)
    }

    fn r#checkPermission(
        &self,
        package: Option<&str>,
        permission: Option<&str>,
        _persistent_device_id: Option<&str>,
        user_id: i32,
    ) -> binder::Result<i32> {
        Ok(check(
            &self.packages,
            &self.permissions,
            package,
            permission,
            user_id,
        ))
    }

    /// getPermissionFlags (PermissionManagerServiceImpl.getPermissionFlagsInternal): the caller holds one of GRANT / REVOKE / GET_RUNTIME_PERMISSIONS; an unknown user or package, or a
    /// permission without state, has flags 0.
    fn r#getPermissionFlags(
        &self,
        package: Option<&str>,
        permission: Option<&str>,
        _persistent_device_id: Option<&str>,
        user_id: i32,
    ) -> binder::Result<i32> {
        self.enforce(
            "getPermissionFlags",
            &[
                GRANT_RUNTIME_PERMISSIONS,
                REVOKE_RUNTIME_PERMISSIONS,
                GET_RUNTIME_PERMISSIONS,
            ],
            true,
        )?;
        let (Some(package), Some(permission)) = (package, permission) else {
            return Ok(0);
        };
        if user_id != USER_SYSTEM {
            return Ok(0);
        }
        let Some(p) = self.packages.by_name(package) else {
            return Ok(0);
        };
        Ok(self.permissions.flags(p.uid, permission) as i32)
    }

    /// grantRuntimePermissionInternal: the caller holds GRANT_RUNTIME_PERMISSIONS; an unknown user or package is logged and ignored; then `Permissions::grant_runtime`'s refusals, as AOSP throws them.
    fn r#grantRuntimePermission(
        &self,
        package: Option<&str>,
        permission: Option<&str>,
        _persistent_device_id: Option<&str>,
        user_id: i32,
    ) -> binder::Result<()> {
        self.enforce(
            "grantRuntimePermission",
            &[GRANT_RUNTIME_PERMISSIONS],
            false,
        )?;
        let permission = permission.unwrap_or_default();
        let Some(p) = package.and_then(|n| self.packages.by_name(n)) else {
            log(format_args!(
                "grantRuntimePermission: unknown package {package:?}"
            ));
            return Ok(());
        };
        if user_id != USER_SYSTEM {
            log(format_args!(
                "grantRuntimePermission: no such user {user_id}"
            ));
            return Ok(());
        }
        self.permissions.grant_runtime(p, permission).map_err(|e| {
            let code = match e {
                GrantError::UnknownPermission(_) => ExceptionCode::ILLEGAL_ARGUMENT,
                GrantError::NotChangeable { .. } | GrantError::NotRequested { .. } => {
                    ExceptionCode::SECURITY
                }
            };
            Status::new_exception_str(code, Some(e.to_string()))
        })
    }
}

/// PermissionManagerServiceImpl.checkPermission: unknown user or package -> denied; else the package's uid state, then the fuller permission.
/// A null package or permission name is denied: no such package, or no such permission in its state (AIDL String arguments arrive as Option).
fn check(
    packages: &Packages,
    permissions: &Permissions,
    package: Option<&str>,
    permission: Option<&str>,
    user_id: i32,
) -> i32 {
    let (Some(package), Some(permission)) = (package, permission) else {
        return DENIED;
    };
    if user_id != USER_SYSTEM {
        return DENIED;
    }
    let Some(p) = packages.by_name(package) else {
        return DENIED;
    };
    let holds = |name: &str| permissions.granted(p.uid, name);
    let fuller = FULLER
        .iter()
        .find(|(k, _)| *k == permission)
        .is_some_and(|(_, v)| holds(v));
    if holds(permission) || fuller {
        GRANTED
    } else {
        DENIED
    }
}

/// The service's binder, for `addService`.
pub fn new_binder(packages: Arc<Packages>, permissions: Arc<Permissions>) -> SpIBinder {
    BnPermissionManager::new_binder(
        PermissionManagerService {
            packages,
            permissions,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::fixture::record;
    use atl_services_types::packages::PermissionDecl;
    use std::collections::BTreeMap;

    const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";
    const ACCESS_COARSE_LOCATION: &str = "android.permission.ACCESS_COARSE_LOCATION";
    const ACCESS_FINE_LOCATION: &str = "android.permission.ACCESS_FINE_LOCATION";

    /// checkPermission over the uid state: a requested runtime permission is DENIED (-1) until granted, then GRANTED (0); COARSE is GRANTED through a granted FINE (FULLER_PERMISSION_MAP);
    /// another user, an unknown package or a null name are DENIED.
    #[test]
    fn check_follows_runtime_grants_and_the_fuller_map() {
        let mut r = record("org.x");
        r.info.requested_permissions = Some(vec![
            Some(POST_NOTIFICATIONS.into()),
            Some(ACCESS_FINE_LOCATION.into()),
        ]);
        let platform = [
            POST_NOTIFICATIONS,
            ACCESS_COARSE_LOCATION,
            ACCESS_FINE_LOCATION,
        ]
        .map(|name| PermissionDecl {
            name: name.into(),
            protection_level: 0x1,
        });
        let packages = Arc::new(Packages::new(vec![r.clone()], BTreeMap::new()));
        let permissions = Permissions::new(&packages, &platform);
        let c = |perm: Option<&str>, pkg: Option<&str>, user| {
            check(&packages, &permissions, pkg, perm, user)
        };
        let x = Some("org.x");
        assert_eq!(c(Some(POST_NOTIFICATIONS), x, 0), -1);
        assert_eq!(c(Some(ACCESS_COARSE_LOCATION), x, 0), -1);
        permissions
            .grant_runtime(&r, POST_NOTIFICATIONS)
            .expect("grant");
        permissions
            .grant_runtime(&r, ACCESS_FINE_LOCATION)
            .expect("grant");
        assert_eq!(c(Some(POST_NOTIFICATIONS), x, 0), 0);
        assert_eq!(c(Some(ACCESS_COARSE_LOCATION), x, 0), 0);
        assert_eq!(c(Some(POST_NOTIFICATIONS), x, 10), -1);
        assert_eq!(c(Some(POST_NOTIFICATIONS), Some("org.y"), 0), -1);
        assert_eq!(c(None, x, 0), -1);
    }
}
