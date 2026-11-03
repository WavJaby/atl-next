// SPDX-License-Identifier: MIT
//! The `display` service (Context.DISPLAY_SERVICE): registered so DisplayManagerGlobal.getInstance() is not null; ActivityThread's ClientTransactionListenerController requires it before any activity transaction runs.
//! The converter's IDisplayManager stub with every method left to its default body, so each transaction lands in [`counters::unimplemented`]: counted and answered UNKNOWN_TRANSACTION (an empty reply: Java's proxy reads no exception and a null / zero result), so no display exists yet.
//! An activity launch makes these calls (measured by `xtask test app`): `getDisplayInfo` 7 times, `getPreferredWideGamutColorSpaceId` and `getOverlaySupport` once each.
//! It answers from the platform layer's display when WMS-lite and the compositor give it one.

use atl_aidl::aidl::android::hardware::display::IDisplayManager::{
    BnDisplayManager, EnforcePermission, IDisplayManager,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::{counters, permission};

/// The interface name counters use.
const INTERFACE: &str = "IDisplayManager";

struct DisplayManager;

impl Interface for DisplayManager {}

impl IDisplayManager for DisplayManager {
    /// Fail-closed as the other services' hooks (activity.rs, package.rs): the permissions the stub names are held by root / system only (permission::holds).
    fn r#enforce_permission(
        &self,
        method: &str,
        permission: EnforcePermission<'_>,
    ) -> binder::Result<()> {
        let perms: &[&str] = match &permission {
            EnforcePermission::Single(p) => std::slice::from_ref(p),
            EnforcePermission::AllOf(p) | EnforcePermission::AnyOf(p) => p,
        };
        if permission::holds(ThreadState::get_calling_uid(), perms) {
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
}

/// A new display service binder.
pub fn new_binder() -> SpIBinder {
    BnDisplayManager::new_binder(DisplayManager, BinderFeatures::default()).as_binder()
}
