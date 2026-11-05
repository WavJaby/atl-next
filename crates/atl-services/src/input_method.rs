// SPDX-License-Identifier: MIT
//! The `input_method` service (Context.INPUT_METHOD_SERVICE, Context.java:5471): the converter's IInputMethodManager stub. InputMethodManager.createRealInstance throws without it
//! (InputMethodManager.java:1638-1641; the lookup is IInputMethodManagerGlobalInvoker.java:96). Its one call on the startup path is addClient (InputMethodManager.java:1652), which AOSP
//! answers by registering the client's controller with the IME service (InputMethodManagerService.java:1791-1805). ATL has no input method yet (the desktop IME bridge is a
//! platform-layer item), so addClient is answered and nothing is kept. Every window focus gain sends startInputOrWindowGainedFocusAsync (ViewRootImpl.handleWindowFocusChanged ->
//! InputMethodManager.startInputInner:3560): its EditorInfo and ImeOnBackInvokedDispatcher are decoded (atl-parcelables codecs; without them the decode failed BAD_VALUE and the
//! app died of IllegalArgumentException), answered, not kept. Every other method stays counted and answered UNKNOWN_TRANSACTION.

use atl_aidl::aidl::com::android::internal::view::IInputMethodManager::{
    BnInputMethodManager, EnforcePermission, IInputMethodManager,
};
use atl_parcelables::android::view::inputmethod::ImeTracker_Token;
use atl_parcelables::com::android::internal::inputmethod::{
    InputBindResult, InputMethodInfoSafeList,
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

    /// startInputOrWindowGainedFocusAsync (ZeroJankProxy.java:466-476, use_zero_jank_proxy on in the pinned release configs): AOSP always replies through client.onStartInputResult.
    /// No IME exists, so nothing is bound and the reply is InputBindResult.NO_IME (what InputMethodManagerService answers with no current method) for the call's sequence. Counted.
    #[allow(clippy::too_many_arguments)]
    fn r#startInputOrWindowGainedFocusAsync(
        &self,
        _start_input_reason: i32,
        client: Option<&Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_18_IInputMethodClient>>,
        _window_token: Option<&SpIBinder>,
        _start_input_flags: i32,
        _soft_input_mode: i32,
        _window_flags: i32,
        _editor_info: Option<&atl_parcelables::android::view::inputmethod::EditorInfo>,
        _input_connection: Option<&Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_22_IRemoteInputConnection>>,
        _remote_accessibility_input_connection: Option<
            &Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_35_IRemoteAccessibilityInputConnection>,
        >,
        _unverified_target_sdk_version: i32,
        _user_id: i32,
        _ime_dispatcher: Option<&atl_parcelables::android::window::ImeOnBackInvokedDispatcher>,
        _ime_requested_visible: bool,
        start_input_seq: i32,
        _use_async_show_hide_method: bool,
    ) -> binder::Result<()> {
        counters::count(
            INTERFACE,
            "startInputOrWindowGainedFocusAsync (answered NO_IME: no IME bridge)",
        );
        if let Some(client) = client {
            let res = InputBindResult::error(InputBindResult::ERROR_NO_IME);
            if let Err(e) = client.r#onStartInputResult(Some(&res), start_input_seq) {
                counters::count(INTERFACE, &format!("onStartInputResult refused: {e:?}"));
            }
        }
        Ok(())
    }

    /// showSoftInput: no IME window to show -> false (IMMS answers false when there is no current method).
    #[allow(clippy::too_many_arguments)]
    fn r#showSoftInput(
        &self,
        _client: Option<&Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_18_IInputMethodClient>>,
        _window_token: Option<&SpIBinder>,
        _stats_token: Option<&ImeTracker_Token>,
        _flags: i32,
        _last_click_tool_type: i32,
        _result_receiver: Option<&atl_parcelables::android::os::ResultReceiver>,
        _reason: i32,
        _async: bool,
    ) -> binder::Result<bool> {
        counters::count(INTERFACE, "showSoftInput (false: no IME)");
        Ok(false)
    }

    /// hideSoftInput: nothing shown -> false.
    #[allow(clippy::too_many_arguments)]
    fn r#hideSoftInput(
        &self,
        _client: Option<&Strong<dyn atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_18_IInputMethodClient>>,
        _window_token: Option<&SpIBinder>,
        _stats_token: Option<&ImeTracker_Token>,
        _flags: i32,
        _result_receiver: Option<&atl_parcelables::android::os::ResultReceiver>,
        _reason: i32,
        _async: bool,
    ) -> binder::Result<bool> {
        counters::count(INTERFACE, "hideSoftInput (false: no IME)");
        Ok(false)
    }

    /// getInputMethodList / getEnabledInputMethodList: no input methods, as InputMethodInfoSafeList.empty() (callers dereference it, so never null).
    fn r#getInputMethodList(
        &self,
        _user_id: i32,
        _direct_boot_awareness: i32,
    ) -> binder::Result<Option<InputMethodInfoSafeList>> {
        Ok(Some(InputMethodInfoSafeList))
    }

    fn r#getEnabledInputMethodList(
        &self,
        _user_id: i32,
    ) -> binder::Result<Option<InputMethodInfoSafeList>> {
        Ok(Some(InputMethodInfoSafeList))
    }
}

/// A new input method service binder.
pub fn new_binder() -> SpIBinder {
    BnInputMethodManager::new_binder(InputMethodManager, BinderFeatures::default()).as_binder()
}
