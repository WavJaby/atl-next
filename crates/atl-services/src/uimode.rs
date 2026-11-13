// SPDX-License-Identifier: MIT
//! The `uimode` service (Context.UI_MODE_SERVICE): UiModeManager's constructor needs it (getServiceOrThrow), else getSystemService returns null (Angry Birds 2's vungle getDeviceBody casts it unchecked and dies).
//! Behaviour = UiModeManagerService.mService with nothing ever changing: the mode type and night mode are read from the global configuration's uiMode ([`activity::GLOBAL_UI_MODE`], the one source),
//! as AOSP's service is what writes that uiMode. Locks off (config_lockUiMode / config_lockDayNightMode default false), contrast 0 (Secure.CONTRAST_LEVEL default), force invert off.
//! Callbacks are accepted and dropped: nothing changes, so AOSP would never notify them. Car mode, night mode changes, projection: UNKNOWN_TRANSACTION (counted).

use std::sync::Arc;

use atl_aidl::aidl::android::app::IUiModeManager::{
    BnUiModeManager, EnforcePermission, IUiModeManager,
};
use atl_aidl::aidl::android::app::IUiModeManagerCallback::IUiModeManagerCallback;
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::activity::GLOBAL_UI_MODE;
use crate::packages::Permissions;

/// Configuration.UI_MODE_TYPE_MASK / UI_MODE_NIGHT_MASK / UI_MODE_NIGHT_YES (Configuration.java).
const UI_MODE_TYPE_MASK: i32 = 0x0f;
const UI_MODE_NIGHT_MASK: i32 = 0x30;
const UI_MODE_NIGHT_YES: i32 = 0x20;
/// UiModeManager.MODE_NIGHT_NO / MODE_NIGHT_YES.
const MODE_NIGHT_NO: i32 = 1;
const MODE_NIGHT_YES: i32 = 2;
/// UiModeManager.FORCE_INVERT_TYPE_OFF.
const FORCE_INVERT_TYPE_OFF: i32 = 0;

struct UiModeService {
    permissions: Arc<Permissions>,
}

impl Interface for UiModeService {}

impl IUiModeManager for UiModeService {
    /// @EnforcePermission (MODIFY_DAY_NIGHT_MODE, READ_PROJECTION_STATE methods): root / system, or a caller whose install-time grants hold the permissions (Permissions::holds, rule A).
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
    fn r#addCallback(
        &self,
        _callback: Option<&Strong<dyn IUiModeManagerCallback>>,
    ) -> binder::Result<()> {
        Ok(())
    }
    /// getCurrentModeType: mCurUiMode & UI_MODE_TYPE_MASK.
    fn r#getCurrentModeType(&self) -> binder::Result<i32> {
        Ok(GLOBAL_UI_MODE & UI_MODE_TYPE_MASK)
    }
    /// getNightMode: mNightMode, the mode that produced the configuration's night bits (a fixed yes / no, never auto).
    fn r#getNightMode(&self) -> binder::Result<i32> {
        Ok(night_mode(GLOBAL_UI_MODE))
    }
    fn r#isUiModeLocked(&self) -> binder::Result<bool> {
        Ok(false)
    }
    fn r#isNightModeLocked(&self) -> binder::Result<bool> {
        Ok(false)
    }
    fn r#getContrast(&self) -> binder::Result<f32> {
        Ok(0.0)
    }
    fn r#getForceInvertState(&self) -> binder::Result<i32> {
        Ok(FORCE_INVERT_TYPE_OFF)
    }
}

fn night_mode(ui_mode: i32) -> i32 {
    if ui_mode & UI_MODE_NIGHT_MASK == UI_MODE_NIGHT_YES {
        MODE_NIGHT_YES
    } else {
        MODE_NIGHT_NO
    }
}

/// The `uimode` binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnUiModeManager::new_binder(UiModeService { permissions }, BinderFeatures::default())
        .as_binder()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configuration.UI_MODE_NIGHT_NO (0x10) / UI_MODE_NIGHT_YES (0x20) with TYPE_NORMAL (1) -> UiModeManager.MODE_NIGHT_NO (1) / MODE_NIGHT_YES (2).
    #[test]
    fn night_mode_follows_the_night_bits() {
        assert_eq!(night_mode(0x11), 1);
        assert_eq!(night_mode(0x21), 2);
    }
}
