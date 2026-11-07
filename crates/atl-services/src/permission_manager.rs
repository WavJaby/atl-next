// SPDX-License-Identifier: MIT
//! The `permissionmgr` service (IPermissionManager): PermissionManager's constructor fetches it with getServiceOrThrow, so without it getSystemService(PermissionManager) is null and
//! PackageManager.checkPermission(perm, pkg) (ApplicationPackageManager -> PermissionManager.checkPackageNamePermission) throws NPE (Cut the Rope's ZSystemInfo.getNetworkType, from native code).
//! checkPermission answers from the install-time grants (permission.rs rule A). Not modelled: package visibility (filterAppAccess), instant apps, device ids. Every other method stays counted.

use atl_aidl::aidl::android::permission::IPermissionManager::{
    BnPermissionManager, IPermissionManager,
};
use binder::{BinderFeatures, Interface, SpIBinder, Status};
use std::sync::Arc;

use crate::counters;
use crate::packages::Packages;

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
}

impl Interface for PermissionManagerService {}

impl IPermissionManager for PermissionManagerService {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(INTERFACE, method)
    }

    /// PermissionManagerServiceImpl.checkPermission: unknown user or package -> denied; else the package's grants, then the fuller permission.
    fn r#checkPermission(
        &self,
        package: &str,
        permission: &str,
        _persistent_device_id: &str,
        user_id: i32,
    ) -> binder::Result<i32> {
        Ok(check(&self.packages, package, permission, user_id))
    }
}

fn check(packages: &Packages, package: &str, permission: &str, user_id: i32) -> i32 {
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
pub fn new_binder(packages: Arc<Packages>) -> SpIBinder {
    BnPermissionManager::new_binder(
        PermissionManagerService { packages },
        BinderFeatures::default(),
    )
    .as_binder()
}
