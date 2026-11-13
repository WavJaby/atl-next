// SPDX-License-Identifier: MIT
//! The `clipboard` service (Context.CLIPBOARD_SERVICE): published so ClipboardManager's constructor (`ServiceManager.getServiceOrThrow("clipboard")`) succeeds; without it
//! SystemServiceRegistry returns null and Compose's AndroidClipboardManager.<init> fails its cast. Plan rule (decision 8c): authority + state here, the Wayland selection reached later
//! through the focused app's IUiAgent. Not built yet: the clip state, the focused-app read rule and the Wayland sync. Every method keeps the stub's default: counted by the reply hook (atl-diag),
//! answered UNKNOWN_TRANSACTION. Do not add get/has methods before the focused-app read rule exists: without it one app reads another's clip.

use atl_aidl::aidl::android::content::IClipboard::{BnClipboard, EnforcePermission, IClipboard};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, ThreadState};

use crate::packages::Permissions;
use std::sync::Arc;

struct ClipboardService {
    permissions: Arc<Permissions>,
}

impl Interface for ClipboardService {}

impl IClipboard for ClipboardService {
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
}

/// The service's binder, for `addService`.
pub fn new_binder(permissions: Arc<Permissions>) -> SpIBinder {
    BnClipboard::new_binder(ClipboardService { permissions }, BinderFeatures::default()).as_binder()
}
