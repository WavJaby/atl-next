// SPDX-License-Identifier: MIT
//! IPackageInstaller, the binder IPackageManager.getPackageInstaller answers (PackageInstallerService; ApplicationPackageManager.getPackageInstaller wraps it in a PackageInstaller and NPEs on null).
//! No install session ever exists here (apps are installed by atld, not through sessions), so the session queries answer what AOSP answers with an empty session table: empty lists, null info.
//! A registered callback is never called, as AOSP's is not while no session changes; it is not kept. Every other method keeps the stub's UNKNOWN_TRANSACTION default (counted).

use std::sync::Arc;

use atl_aidl::aidl::android::content::pm::IPackageInstaller::{
    BnPackageInstaller, EnforcePermission, IPackageInstaller,
};
use atl_aidl::mangled::_7_android_7_content_2_pm_25_IPackageInstallerCallback as IPackageInstallerCallback;
use atl_parcelables::android::content::pm::{PackageInstaller_SessionInfo, ParceledListSlice};
use atl_services_types::permission;
use binder::{BinderFeatures, ExceptionCode, Interface, Status, Strong, ThreadState};

use crate::packages::PackageTable;

/// The interface name counters use.
const INTERFACE: &str = "IPackageInstaller";

struct PackageInstaller {
    packages: Arc<PackageTable>,
}

impl Interface for PackageInstaller {}

/// ComputerEngine.enforceCrossUserPermission with requireFullPermission: the caller's own user passes, as do system and root; another user needs INTERACT_ACROSS_USERS_FULL, which no app holds.
fn enforce_same_user(user: i32, what: &str) -> binder::Result<()> {
    let uid = ThreadState::get_calling_uid();
    if permission::is_system_or_root(uid)
        || i32::try_from(permission::user_id(uid)).is_ok_and(|u| u == user)
    {
        return Ok(());
    }
    Err(Status::new_exception_str(
        ExceptionCode::SECURITY,
        Some(format!(
            "Permission Denial: {what} asks to run as user {user} but is calling from uid {uid}; this requires android.permission.INTERACT_ACROSS_USERS_FULL"
        )),
    ))
}

/// No session: the empty list AOSP's loops produce over an empty mSessions.
fn no_sessions() -> Option<ParceledListSlice> {
    Some(ParceledListSlice::default())
}

#[allow(non_snake_case, reason = "AOSP's AIDL method names")]
impl IPackageInstaller for PackageInstaller {
    /// @EnforcePermission: every permission the interface enforces (INSTALL_PACKAGES, TEST_INSTALL_PERMISSION, ...) is signature or privileged, held by no app here: root and system pass.
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        if permission::is_system_or_root(ThreadState::get_calling_uid()) {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    /// PackageInstallerService.getSessionInfo:1324: null for an id with no session.
    fn r#getSessionInfo(
        &self,
        _session_id: i32,
    ) -> binder::Result<Option<PackageInstaller_SessionInfo>> {
        atl_diag::answered(INTERFACE, "getSessionInfo (no session)");
        Ok(None)
    }

    /// getStagedSessions:1337: the staged, live sessions: none.
    fn r#getStagedSessions(&self) -> binder::Result<Option<ParceledListSlice>> {
        atl_diag::answered(INTERFACE, "getStagedSessions");
        Ok(no_sessions())
    }

    /// getAllSessions:1354: the cross-user check, then the user's sessions: none.
    fn r#getAllSessions(&self, user: i32) -> binder::Result<Option<ParceledListSlice>> {
        enforce_same_user(user, "getAllSessions")?;
        atl_diag::answered(INTERFACE, "getAllSessions");
        Ok(no_sessions())
    }

    /// getMySessions:1374: the cross-user check, AppOpsManager.checkPackage (the installer package must belong to the caller: SecurityException otherwise), then the sessions it owns: none.
    fn r#getMySessions(
        &self,
        installer: Option<&str>,
        user: i32,
    ) -> binder::Result<Option<ParceledListSlice>> {
        enforce_same_user(user, "getMySessions")?;
        let uid = ThreadState::get_calling_uid();
        let packages = self.packages.snapshot();
        let owned = installer
            .is_some_and(|p| packages.visible(uid, p).is_some() && packages.is_same_app(p, uid));
        if !owned && !permission::is_system_or_root(uid) {
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!(
                    "Package {} does not belong to {uid}",
                    installer.unwrap_or("null")
                )),
            ));
        }
        atl_diag::answered(INTERFACE, "getMySessions");
        Ok(no_sessions())
    }

    /// registerCallback:1640: the cross-user check, then AOSP keeps the callback for session events; none happen here, so it is not kept.
    fn r#registerCallback(
        &self,
        _callback: Option<&Strong<dyn IPackageInstallerCallback>>,
        user: i32,
    ) -> binder::Result<()> {
        enforce_same_user(user, "registerCallback")?;
        atl_diag::answered(INTERFACE, "registerCallback");
        Ok(())
    }

    /// unregisterCallback:1655: nothing was kept.
    fn r#unregisterCallback(
        &self,
        _callback: Option<&Strong<dyn IPackageInstallerCallback>>,
    ) -> binder::Result<()> {
        atl_diag::answered(INTERFACE, "unregisterCallback");
        Ok(())
    }
}

/// The installer binder for `packages`; one per PackageManager, as AOSP's one mInstallerService.
pub(crate) fn new(packages: Arc<PackageTable>) -> Strong<dyn IPackageInstaller> {
    BnPackageInstaller::new_binder(PackageInstaller { packages }, BinderFeatures::default())
}
