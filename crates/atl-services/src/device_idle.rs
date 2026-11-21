// SPDX-License-Identifier: MIT
//! The `deviceidle` service (DeviceIdleController.BinderService, apex/jobscheduler/service/java/com/android/server/DeviceIdleController.java :2130): PowerExemptionManager's constructor (:529-531) and
//! DeviceIdleManager wrap it, so without it PowerManager.isIgnoringBatteryOptimizations (:2701-2702) NPEs. SystemServer publishes it on every device (:2696 publishBinderService).
//! Allowlists: the system ones come from SystemConfig `allow-in-power-save(-except-idle)` (services/core/java/com/android/server/SystemConfig.java :1069-1096), the user one from Settings'
//! battery-optimisation screen. No sysconfig file of this runtime declares either tag (its only sysconfig input is atld's feature file) and nothing adds to the user set (every mutator needs
//! DEVICE_POWER, signature), so every list is empty and every package reads "not exempt": AOSP's answer for a third-party app on a fresh device. ATL never dozes apps, but AOSP answers the
//! allowlist state whether or not idle restrictions are active, so the answer stays (behaviour decision 3 does not change what an app observes here).
//! - isPowerSaveWhitelistApp / ...ExceptIdleApp (:2245-2259): filterAppAccess (invisible package -> false), then the sets (:3153-3165) -> false either way.
//! - get*Whitelist* (:3055-3190): the (empty) sets, filtered by visibility.
//! - add/remove/restore allowlist (:2138-2200): DEVICE_POWER; addPowerSaveTempWhitelistApp* / whitelistAppTemporarily (addPowerSaveTempAllowlistAppChecked :3203-3207):
//!   CHANGE_DEVICE_IDLE_TEMP_WHITELIST (signature|privileged); exitIdle: @EnforcePermission DEVICE_POWER. An app holds neither -> SecurityException as ContextImpl.enforce words it (:2430-2440).
//!   A holder (none exists among apps) is refused and counted: there is no allowlist store to change.
//!
//! ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS opens Settings' RequestIgnoreBatteryOptimizations activity; ATL ships no Settings app, so that intent resolves to nothing (the caller's ActivityNotFoundException path), as on a device without Settings.

use std::sync::Arc;

use atl_aidl::aidl::android::os::IDeviceIdleController::{
    BnDeviceIdleController, EnforcePermission, IDeviceIdleController,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::Permissions;

/// The name the answers are counted under (`atl_diag::answered`, the app gate's SERVICE_CALLS).
const DEVICE_IDLE: &str = "IDeviceIdleController";
const DEVICE_POWER: &str = "android.permission.DEVICE_POWER";
const CHANGE_TEMP_WHITELIST: &str = "android.permission.CHANGE_DEVICE_IDLE_TEMP_WHITELIST";
/// addPowerSaveTempAllowlistAppChecked's enforce message (:3206-3207).
const TEMP_MESSAGE: &str = "No permission to change device idle whitelist";

struct DeviceIdle {
    permissions: Arc<Permissions>,
}

impl Interface for DeviceIdle {}

impl DeviceIdle {
    /// Context.enforceCallingOrSelfPermission(permission, message) (ContextImpl.enforce :2430-2440); a holder gets the counted refusal `what` (no allowlist store).
    fn enforce(&self, permission: &str, message: Option<&str>, what: &str) -> binder::Result<()> {
        let uid = ThreadState::get_calling_uid();
        if !self.permissions.holds(uid, &[permission], false) {
            let prefix = message.map(|m| format!("{m}: ")).unwrap_or_default();
            return Err(Status::new_exception_str(
                ExceptionCode::SECURITY,
                Some(format!(
                    "{prefix}Neither user {uid} nor current process has {permission}."
                )),
            ));
        }
        atl_diag::refused(DEVICE_IDLE, &format!("{what} (no allowlist store)"));
        Err(Status::new_exception(
            ExceptionCode::UNSUPPORTED_OPERATION,
            None,
        ))
    }

    fn empty_names(&self, what: &str) -> binder::Result<Option<Vec<Option<String>>>> {
        atl_diag::answered(DEVICE_IDLE, &format!("{what} (empty)"));
        Ok(Some(Vec::new()))
    }

    fn empty_ids(&self, what: &str) -> binder::Result<Option<Vec<i32>>> {
        atl_diag::answered(DEVICE_IDLE, &format!("{what} (empty)"));
        Ok(Some(Vec::new()))
    }
}

impl IDeviceIdleController for DeviceIdle {
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
        let uid = ThreadState::get_calling_uid();
        if self.permissions.holds(uid, perms, any) {
            return Ok(());
        }
        Err(Status::new_exception_str(
            ExceptionCode::SECURITY,
            Some(format!("{method}: caller lacks {permission:?}")),
        ))
    }

    fn r#addPowerSaveWhitelistApp(&self, _name: Option<&str>) -> binder::Result<()> {
        self.enforce(DEVICE_POWER, None, "addPowerSaveWhitelistApp")
    }

    fn r#addPowerSaveWhitelistApps(
        &self,
        _names: Option<&[Option<String>]>,
    ) -> binder::Result<i32> {
        self.enforce(DEVICE_POWER, None, "addPowerSaveWhitelistApps")?;
        Ok(0)
    }

    fn r#removePowerSaveWhitelistApp(&self, _name: Option<&str>) -> binder::Result<()> {
        self.enforce(DEVICE_POWER, None, "removePowerSaveWhitelistApp")
    }

    fn r#removeSystemPowerWhitelistApp(&self, _name: Option<&str>) -> binder::Result<()> {
        self.enforce(DEVICE_POWER, None, "removeSystemPowerWhitelistApp")
    }

    fn r#restoreSystemPowerWhitelistApp(&self, _name: Option<&str>) -> binder::Result<()> {
        self.enforce(DEVICE_POWER, None, "restoreSystemPowerWhitelistApp")
    }

    fn r#getRemovedSystemPowerWhitelistApps(&self) -> binder::Result<Option<Vec<Option<String>>>> {
        self.empty_names("getRemovedSystemPowerWhitelistApps")
    }

    fn r#getSystemPowerWhitelistExceptIdle(&self) -> binder::Result<Option<Vec<Option<String>>>> {
        self.empty_names("getSystemPowerWhitelistExceptIdle")
    }

    fn r#getSystemPowerWhitelist(&self) -> binder::Result<Option<Vec<Option<String>>>> {
        self.empty_names("getSystemPowerWhitelist")
    }

    fn r#getUserPowerWhitelist(&self) -> binder::Result<Option<Vec<Option<String>>>> {
        self.empty_names("getUserPowerWhitelist")
    }

    fn r#getFullPowerWhitelistExceptIdle(&self) -> binder::Result<Option<Vec<Option<String>>>> {
        self.empty_names("getFullPowerWhitelistExceptIdle")
    }

    fn r#getFullPowerWhitelist(&self) -> binder::Result<Option<Vec<Option<String>>>> {
        self.empty_names("getFullPowerWhitelist")
    }

    fn r#getAppIdWhitelistExceptIdle(&self) -> binder::Result<Option<Vec<i32>>> {
        self.empty_ids("getAppIdWhitelistExceptIdle")
    }

    fn r#getAppIdWhitelist(&self) -> binder::Result<Option<Vec<i32>>> {
        self.empty_ids("getAppIdWhitelist")
    }

    fn r#getAppIdUserWhitelist(&self) -> binder::Result<Option<Vec<i32>>> {
        self.empty_ids("getAppIdUserWhitelist")
    }

    fn r#getAppIdTempWhitelist(&self) -> binder::Result<Option<Vec<i32>>> {
        self.empty_ids("getAppIdTempWhitelist")
    }

    fn r#isPowerSaveWhitelistExceptIdleApp(&self, _name: Option<&str>) -> binder::Result<bool> {
        atl_diag::answered(
            DEVICE_IDLE,
            "isPowerSaveWhitelistExceptIdleApp (empty allowlist)",
        );
        Ok(false)
    }

    fn r#isPowerSaveWhitelistApp(&self, _name: Option<&str>) -> binder::Result<bool> {
        atl_diag::answered(DEVICE_IDLE, "isPowerSaveWhitelistApp (empty allowlist)");
        Ok(false)
    }

    fn r#addPowerSaveTempWhitelistApp(
        &self,
        _name: Option<&str>,
        _duration: i64,
        _user_id: i32,
        _reason_code: i32,
        _reason: Option<&str>,
    ) -> binder::Result<()> {
        self.enforce(
            CHANGE_TEMP_WHITELIST,
            Some(TEMP_MESSAGE),
            "addPowerSaveTempWhitelistApp",
        )
    }

    fn r#addPowerSaveTempWhitelistAppForMms(
        &self,
        _name: Option<&str>,
        _user_id: i32,
        _reason_code: i32,
        _reason: Option<&str>,
    ) -> binder::Result<i64> {
        self.enforce(
            CHANGE_TEMP_WHITELIST,
            Some(TEMP_MESSAGE),
            "addPowerSaveTempWhitelistAppForMms",
        )?;
        Ok(0)
    }

    fn r#addPowerSaveTempWhitelistAppForSms(
        &self,
        _name: Option<&str>,
        _user_id: i32,
        _reason_code: i32,
        _reason: Option<&str>,
    ) -> binder::Result<i64> {
        self.enforce(
            CHANGE_TEMP_WHITELIST,
            Some(TEMP_MESSAGE),
            "addPowerSaveTempWhitelistAppForSms",
        )?;
        Ok(0)
    }

    fn r#whitelistAppTemporarily(
        &self,
        _name: Option<&str>,
        _user_id: i32,
        _reason_code: i32,
        _reason: Option<&str>,
    ) -> binder::Result<i64> {
        self.enforce(
            CHANGE_TEMP_WHITELIST,
            Some(TEMP_MESSAGE),
            "whitelistAppTemporarily",
        )?;
        Ok(0)
    }

    /// Reached only past the @EnforcePermission hook, i.e. by a DEVICE_POWER holder: no idle state to leave.
    fn r#exitIdle(&self, _reason: Option<&str>) -> binder::Result<()> {
        atl_diag::answered(DEVICE_IDLE, "exitIdle (never idle)");
        Ok(())
    }
}

/// The `deviceidle` binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnDeviceIdleController::new_binder(DeviceIdle { permissions }, BinderFeatures::default())
        .as_binder()
}
