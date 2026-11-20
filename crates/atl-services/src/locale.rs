// SPDX-License-Identifier: MIT
//! The `locale` service (Context.LOCALE_SERVICE): LocaleManager's constructor needs it (SystemServiceRegistry getServiceOrThrow), else getSystemService(LocaleManager.class) is null (androidx AppCompat's
//! getApplicationLocales / getSystemLocales; Temple Run, Angry Birds 2 log `No service published for: locale`).
//! Behaviour = LocaleManagerService's read side with no per-app locale ever stored:
//! - getSystemLocales (:534-554) = the global configuration's locale list, read from [`activity::global_configuration`] (the one source; the host locale reaches it once, `Tasks::locale`).
//! - getApplicationLocales (:405-461): handleIncomingUser, then the caller must own the package (isPackageOwnedByCaller :363, an unknown package -> IllegalArgumentException), be its installer
//!   (isCallerInstaller :466; ATL records no installer, so never), or the current IME querying the foreground app (ATL's IME runs inside the app, so its caller is the owner already); else
//!   READ_APP_SPECIFIC_LOCALES (signature|installer). No package config is stored -> LocaleList.getEmptyLocaleList() (:450-460).
//! - getOverrideLocaleConfig (:751): handleIncomingUser, then no override file -> null.
//!
//! setApplicationLocales / setOverrideLocaleConfig answer UNKNOWN_TRANSACTION (counted): AOSP commits a per-package configuration override that changes the app's Configuration and sends
//! ACTION_LOCALE_CHANGED (:272-290); accepting and dropping it would report a change that never happens. They arrive with the per-package configuration override (plan S18).

use std::sync::Arc;

use atl_aidl::aidl::android::app::ILocaleManager::{BnLocaleManager, ILocaleManager};
use atl_parcelables::android::app::LocaleConfig;
use atl_parcelables::android::os::LocaleList;
use atl_services_types::permission;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::activity::global_configuration;
use crate::packages::{Packages, Permissions};
use crate::tasks::Tasks;

/// The name the answers are counted under (`atl_diag::answered`, the app gate's SERVICE_CALLS).
const LOCALE_MANAGER: &str = "ILocaleManager";
/// enforceReadAppSpecificLocalesPermission's permission.
const READ_APP_SPECIFIC_LOCALES: &str = "android.permission.READ_APP_SPECIFIC_LOCALES";
/// handleIncomingUser with ALLOW_NON_FULL: either permission lets a caller name another user (UserController.handleIncomingUser :2828-2844).
const INTERACT_ACROSS_USERS: &str = "android.permission.INTERACT_ACROSS_USERS";
const INTERACT_ACROSS_USERS_FULL: &str = "android.permission.INTERACT_ACROSS_USERS_FULL";
/// UserHandle.USER_CURRENT / USER_CURRENT_OR_SELF.
const USER_CURRENT: i32 = -2;
const USER_CURRENT_OR_SELF: i32 = -3;
/// UserController.mCurrentUserId: ATL runs the system user only.
const CURRENT_USER: i32 = 0;

struct LocaleService {
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    tasks: Arc<Tasks<SpIBinder>>,
}

impl Interface for LocaleService {}

fn security(message: String) -> Status {
    Status::new_exception_str(ExceptionCode::SECURITY, Some(message))
}

fn illegal_argument(message: String) -> Status {
    Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(message))
}

impl LocaleService {
    /// UserController.handleIncomingUser(allowAll = false, ALLOW_NON_FULL) :2806: the caller's own user passes; root, system and holders of INTERACT_ACROSS_USERS(_FULL) may name another;
    /// else USER_CURRENT_OR_SELF falls back to the caller's user and anything else is SecurityException. A special user left negative is IllegalArgumentException.
    fn incoming_user(&self, method: &str, uid: u32, user_id: i32) -> binder::Result<i32> {
        let own = i32::try_from(permission::user_id(uid)).unwrap_or(i32::MAX);
        if user_id == own {
            return Ok(own);
        }
        let target = match user_id {
            USER_CURRENT | USER_CURRENT_OR_SELF => CURRENT_USER,
            u => u,
        };
        let allowed = permission::is_system_or_root(uid)
            || self.permissions.holds(
                uid,
                &[INTERACT_ACROSS_USERS_FULL, INTERACT_ACROSS_USERS],
                true,
            );
        if !allowed {
            if user_id != USER_CURRENT_OR_SELF {
                return Err(security(format!(
                    "Permission Denial: {method} asks to run as user {user_id} but is calling from uid {uid}; this requires {INTERACT_ACROSS_USERS_FULL} or {INTERACT_ACROSS_USERS}"
                )));
            }
            return Ok(own);
        }
        if target < 0 {
            return Err(illegal_argument(format!(
                "Call does not support special user #{user_id}"
            )));
        }
        Ok(target)
    }

    /// getPackageUid (getPackageUidAsUser in the caller's identity, so package visibility applies): the platform package is framework-res, uid SYSTEM_UID (ATL's package list holds apps only).
    fn package_uid(&self, caller: u32, package: &str) -> Option<u32> {
        if package == permission::PLATFORM_PACKAGE {
            return Some(permission::SYSTEM_UID);
        }
        self.packages.visible(caller, package).map(|p| p.uid)
    }
}

impl ILocaleManager for LocaleService {
    fn r#getApplicationLocales(
        &self,
        package_name: Option<&str>,
        user_id: i32,
    ) -> binder::Result<Option<LocaleList>> {
        let package =
            package_name.ok_or_else(|| Status::new_exception(ExceptionCode::NULL_POINTER, None))?;
        let uid = ThreadState::get_calling_uid();
        let user = self.incoming_user("getApplicationLocales", uid, user_id)?;
        let Some(target) = self.package_uid(uid, package) else {
            return Err(illegal_argument(format!(
                "Unknown package: {package} for user {user}"
            )));
        };
        // isPackageOwnedByCaller (UserHandle.isSameApp); isCallerInstaller and the IME case never hold here (module doc)
        if permission::app_id(target) != permission::app_id(uid)
            && !self
                .permissions
                .holds(uid, &[READ_APP_SPECIFIC_LOCALES], false)
        {
            return Err(security(format!(
                "getApplicationLocales: uid {uid} lacks {READ_APP_SPECIFIC_LOCALES}"
            )));
        }
        atl_diag::answered(LOCALE_MANAGER, "getApplicationLocales (no override: empty)");
        Ok(Some(LocaleList::default()))
    }

    fn r#getSystemLocales(&self) -> binder::Result<Option<LocaleList>> {
        atl_diag::answered(LOCALE_MANAGER, "getSystemLocales");
        let config = global_configuration(&self.tasks.display(), self.tasks.locale());
        Ok(Some(LocaleList {
            tags: config.locale_list.unwrap_or_default(),
        }))
    }

    fn r#getOverrideLocaleConfig(
        &self,
        package_name: Option<&str>,
        user_id: i32,
    ) -> binder::Result<Option<LocaleConfig>> {
        package_name.ok_or_else(|| Status::new_exception(ExceptionCode::NULL_POINTER, None))?;
        self.incoming_user(
            "getOverrideLocaleConfig",
            ThreadState::get_calling_uid(),
            user_id,
        )?;
        atl_diag::answered(
            LOCALE_MANAGER,
            "getOverrideLocaleConfig (no override: null)",
        );
        Ok(None)
    }
}

/// The `locale` binder, for `addService`.
pub fn new_binder(
    packages: Arc<Packages>,
    permissions: Arc<Permissions>,
    tasks: Arc<Tasks<SpIBinder>>,
) -> SpIBinder {
    BnLocaleManager::new_binder(
        LocaleService {
            packages,
            permissions,
            tasks,
        },
        BinderFeatures::default(),
    )
    .as_binder()
}
