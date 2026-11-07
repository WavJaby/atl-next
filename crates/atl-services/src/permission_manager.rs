// SPDX-License-Identifier: MIT
//! The `permissionmgr` service (IPermissionManager): PermissionManager's constructor fetches it with getServiceOrThrow (PermissionManager.java:279), so without it getSystemService(PermissionManager) is null and PackageManager.checkPermission(perm, pkg) (PermissionManager.checkPackageNamePermission, PermissionManager.java:2037 -> checkPackageNamePermissionUncached, :2006 -> IPermissionManager.checkPermission) throws NPE (Cut the Rope's ZSystemInfo.getNetworkType, from native code).
//! checkPermission answers as PermissionManagerServiceImpl.checkPermission does (PermissionManagerServiceImpl.java:936, checkPermissionInternal :948) for an installed package: its install-time grants (permission.rs rule A), then FULLER_PERMISSION_MAP (:232).
//! A runtime permission is denied: runtime grants are not modelled, and AOSP denies one the user has not granted (checkSinglePermissionInternalLocked :988). Not modelled: package visibility (filterAppAccess), instant apps, device ids.
//! The @EnforcePermission methods (IPermissionManager.aidl:84, MANAGE_ONE_TIME_PERMISSION_SESSIONS) take the caller rule every service uses (permission.rs holds). Every other method stays counted.

use atl_aidl::aidl::android::permission::IPermissionManager::{
    BnPermissionManager, EnforcePermission, IPermissionManager,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};
use std::sync::Arc;

use crate::counters;
use crate::packages::Packages;
use crate::permission::Permissions;

/// The interface name counters use.
const INTERFACE: &str = "IPermissionManager";
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

struct PermissionManagerService {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
}

impl Interface for PermissionManagerService {}

impl IPermissionManager for PermissionManagerService {
    /// @EnforcePermission: root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
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
        if self
            .permissions
            .holds(ThreadState::get_calling_uid(), perms, any)
        {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    fn r#checkPermission(
        &self,
        package: Option<&str>,
        permission: Option<&str>,
        _persistent_device_id: Option<&str>,
        user_id: i32,
    ) -> binder::Result<i32> {
        Ok(check(&self.packages, package, permission, user_id))
    }
}

/// PermissionManagerServiceImpl.checkPermission: unknown user or package -> denied; else the package's grants, then the fuller permission.
/// A null package or permission name is denied: no such package, or no such permission in its grants (AIDL String arguments arrive as Option).
fn check(
    packages: &Packages,
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
    let holds = |name: &str| p.granted_permissions.iter().any(|g| g == name);
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
