// SPDX-License-Identifier: MIT
//! The `window` service (Context.WINDOW_SERVICE): the converter's IWindowManager stub with the three answers an activity reads before any window exists, and the session openSession gives.
//! hasNavigationBar: false. WindowManagerService.hasNavigationBar (WindowManagerService.java:6669-6677) asks the display policy, which reads config_showNavigationBar
//! (frameworks/base/core/res/res/values/config.xml:2743, false); the desktop has no software navigation bar.
//! getCurrentAnimatorScale: 1.0. WindowManagerService.getCurrentAnimatorScale (:3687-3691) returns mAnimatorDurationScaleSetting, whose default is 1.0f (:1052), or 0 with animations disabled.
//! openSession: a new session. WindowManagerService.openSession (:6004-6005) returns `new Session(this, callback)`; the session's methods are all counted until the window path implements them (slice 5).
//! Every other method keeps the stub's default body: counted (counters.rs) and answered UNKNOWN_TRANSACTION.

use atl_aidl::aidl::android::view::IWindowManager::{
    BnWindowManager, EnforcePermission, IWindowManager,
};
use atl_aidl::aidl::android::view::IWindowSession::{BnWindowSession, IWindowSession};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::{counters, permission};

/// The interface names counters use.
const WINDOW_MANAGER: &str = "IWindowManager";
const WINDOW_SESSION: &str = "IWindowSession";

struct WindowManager;

/// The session openSession hands out: no method is implemented yet.
struct Session;

impl Interface for WindowManager {}

impl Interface for Session {}

impl IWindowManager for WindowManager {
    /// Fail-closed as the other services' hooks (display.rs, activity.rs): the permissions the stub names are held by root / system only (permission::holds).
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
        counters::unimplemented(WINDOW_MANAGER, method)
    }

    /// WindowManagerService.hasNavigationBar:6669-6677, for the one display: false (see the module doc).
    fn r#hasNavigationBar(&self, _display_id: i32) -> binder::Result<bool> {
        Ok(false)
    }

    /// WindowManagerService.getCurrentAnimatorScale:3687-3691 with the default setting 1.0f (:1052) and animations enabled.
    fn r#getCurrentAnimatorScale(&self) -> binder::Result<f32> {
        Ok(1.0)
    }

    /// WindowManagerService.openSession:6004-6005: a new session for the caller's callback; the callback is not kept (no window yet).
    fn r#openSession(
        &self,
        _callback: Option<
            &Strong<dyn atl_aidl::mangled::_7_android_4_view_22_IWindowSessionCallback>,
        >,
    ) -> binder::Result<Option<Strong<dyn IWindowSession>>> {
        Ok(Some(BnWindowSession::new_binder(
            Session,
            BinderFeatures::default(),
        )))
    }
}

impl IWindowSession for Session {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        counters::unimplemented(WINDOW_SESSION, method)
    }
}

/// A new window service binder.
pub fn new_binder() -> SpIBinder {
    BnWindowManager::new_binder(WindowManager, BinderFeatures::default()).as_binder()
}
