// SPDX-License-Identifier: MIT
//! The `input_method` service (Context.INPUT_METHOD_SERVICE, Context.java:5471): the converter's IInputMethodManager stub. InputMethodManager.createRealInstance throws without it
//! (InputMethodManager.java:1638-1641; the lookup is IInputMethodManagerGlobalInvoker.java:96). Its one call on the startup path is addClient (InputMethodManager.java:1652), which AOSP
//! answers by registering the client's controller with the IME service (InputMethodManagerService.java:1791-1805). ATL has no input method yet (the desktop IME bridge is a
//! platform-layer item), so addClient is answered and nothing is kept; every other method stays counted and answered UNKNOWN_TRANSACTION.

use atl_aidl::aidl::com::android::internal::view::IInputMethodManager::{
    BnInputMethodManager, EnforcePermission, IInputMethodManager,
};
use binder::{BinderFeatures, ExceptionCode, Interface, SpIBinder, Status, Strong, ThreadState};

use crate::{counters, permission};

/// The interface name counters use.
const INTERFACE: &str = "IInputMethodManager";

struct InputMethodManager;

impl Interface for InputMethodManager {}

impl IInputMethodManager for InputMethodManager {
    /// Fail-closed as the other services' hooks (window.rs, display.rs): the permissions the stub names are held by root / system only (permission::holds).
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

    /// InputMethodManagerService.addClient (:1791-1805), as createRealInstance calls it (InputMethodManager.java:1652): the client would be registered with the IME service's
    /// controller. There is no input method to route to yet, so the client and its connection are read and not kept. Counted, so a run shows the call.
    fn r#addClient(
        &self,
        _client: Option<&Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_18_IInputMethodClient>>,
        _input_connection: Option<&Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_22_IRemoteInputConnection>>,
        _untrusted_display_id: i32,
    ) -> binder::Result<()> {
        counters::count(INTERFACE, "addClient (not kept: no IME bridge)");
        Ok(())
    }
}

/// A new input method service binder.
pub fn new_binder() -> SpIBinder {
    BnInputMethodManager::new_binder(InputMethodManager, BinderFeatures::default()).as_binder()
}
