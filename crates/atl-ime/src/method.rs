// SPDX-License-Identifier: MIT
//! The in-app IInputMethod and IInputMethodSession (IME plan route R1, step 5): what IMMS-lite binds (atl-services ime_bind.rs) and the app's InputMethodManager talks to, served in the app
//! process. Reference, the IME side of AOSP: core/java/android/inputmethodservice/IInputMethodWrapper.java (createSession -> callback.sessionCreated, startInput, unbindInput) and
//! IInputMethodSessionWrapper.java (finishInput, invalidateInput). Calls arrive on binder threads; every one that touches the field goes to the UI thread through the invoker (field.rs).
//! Not served (answered UNKNOWN_TRANSACTION, logged): handwriting, inline suggestions, subtypes, extracted text, completions; show/hide are accepted and do nothing (no soft keyboard: the desktop input method shows its own UI).

use std::sync::Arc;

use atl_aidl::aidl::com::android::internal::inputmethod::IInputMethod::{
    BnInputMethod, IInputMethod,
};
use atl_aidl::aidl::com::android::internal::inputmethod::IInputMethodSession::{
    BnInputMethodSession, IInputMethodSession,
};
use atl_aidl::mangled::{
    _3_com_7_android_8_internal_11_inputmethod_12_IInputMethod_10_InitParams as InitParams,
    _3_com_7_android_8_internal_11_inputmethod_12_IInputMethod_16_StartInputParams as StartInputParams,
    _3_com_7_android_8_internal_11_inputmethod_22_IRemoteInputConnection as IRemoteInputConnection,
    _3_com_7_android_8_internal_11_inputmethod_27_IInputMethodSessionCallback as IInputMethodSessionCallback,
};
use atl_parcelables::android::os::ResultReceiver;
use atl_parcelables::android::view::InputChannel;
use atl_parcelables::android::view::inputmethod::{EditorInfo, ImeTracker_Token, InputBinding};
use atl_platform_ui::UiInvoker;
use binder::{BinderFeatures, Interface, SpIBinder, Status, StatusCode, Strong};

use crate::field::{self, Field};

type Invoker = Arc<dyn UiInvoker>;

fn unserved(method: &'static str) -> Status {
    tracing::warn!("atl-ime: {method} not served");
    StatusCode::UNKNOWN_TRANSACTION.into()
}

/// Queues a field start on the UI thread.
fn start_field(
    invoker: &Invoker,
    connection: Option<&Strong<dyn IRemoteInputConnection>>,
    editor: Option<&EditorInfo>,
    session_id: i32,
) {
    let Some(connection) = connection.cloned() else {
        tracing::warn!("atl-ime: start without a connection");
        return;
    };
    let input_type = editor.map_or(0, |e| e.input_type);
    invoker.invoke(Box::new(move || {
        field::start(Field {
            connection,
            session_id,
            input_type,
        })
    }));
}

struct InputMethod {
    invoker: Invoker,
}

impl Interface for InputMethod {}

impl IInputMethod for InputMethod {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        unserved(method)
    }

    /// IInputMethodWrapper.initializeInternal: the IME's window token and privileged operations; this IME has no window, so nothing is kept.
    fn r#initializeInternal(&self, _params: Option<&InitParams>) -> binder::Result<()> {
        tracing::info!("atl-ime: initializeInternal");
        Ok(())
    }

    fn r#bindInput(&self, binding: Option<&InputBinding>) -> binder::Result<()> {
        tracing::info!("atl-ime: bindInput uid={:?}", binding.map(|b| b.uid));
        Ok(())
    }

    fn r#unbindInput(&self) -> binder::Result<()> {
        self.invoker.invoke(Box::new(field::finish));
        Ok(())
    }

    /// IInputMethodWrapper.startInput -> InputMethodService.dispatchStartInput: a new connection, session id 0 (RemoteInputConnectionImpl starts at 0).
    fn r#startInput(&self, params: Option<&StartInputParams>) -> binder::Result<()> {
        let Some(params) = params else {
            return Ok(());
        };
        start_field(
            &self.invoker,
            params.remoteInputConnection.as_ref(),
            params.editorInfo.as_ref(),
            0,
        );
        Ok(())
    }

    /// IInputMethodWrapper.createSession: the session goes back through the callback. The channel (key events for the IME, plan step 7) is not used yet.
    fn r#createSession(
        &self,
        _channel: Option<&InputChannel>,
        callback: Option<&Strong<dyn IInputMethodSessionCallback>>,
    ) -> binder::Result<()> {
        let Some(callback) = callback else {
            return Ok(());
        };
        let session = BnInputMethodSession::new_binder(
            Session {
                invoker: Arc::clone(&self.invoker),
            },
            BinderFeatures::default(),
        );
        tracing::info!("atl-ime: createSession");
        callback.r#sessionCreated(Some(&session))
    }

    fn r#setSessionEnabled(
        &self,
        _session: Option<&Strong<dyn IInputMethodSession>>,
        enabled: bool,
    ) -> binder::Result<()> {
        tracing::debug!("atl-ime: setSessionEnabled {enabled}");
        Ok(())
    }

    fn r#showSoftInput(
        &self,
        _token: Option<&SpIBinder>,
        _stats: Option<&ImeTracker_Token>,
        _flags: i32,
        _receiver: Option<&ResultReceiver>,
    ) -> binder::Result<()> {
        Ok(())
    }

    fn r#hideSoftInput(
        &self,
        _token: Option<&SpIBinder>,
        _stats: Option<&ImeTracker_Token>,
        _flags: i32,
        _receiver: Option<&ResultReceiver>,
    ) -> binder::Result<()> {
        Ok(())
    }
}

struct Session {
    invoker: Invoker,
}

impl Interface for Session {}

impl IInputMethodSession for Session {
    fn r#unimplemented(&self, method: &'static str) -> Status {
        unserved(method)
    }

    /// The editor's selection moved; the desktop side follows in plan step 6.
    fn r#updateSelection(
        &self,
        _old_start: i32,
        _old_end: i32,
        new_start: i32,
        new_end: i32,
        candidates_start: i32,
        candidates_end: i32,
    ) -> binder::Result<()> {
        tracing::debug!(
            "atl-ime: updateSelection {new_start}..{new_end} composing {candidates_start}..{candidates_end}"
        );
        Ok(())
    }

    fn r#viewClicked(&self, _focus_changed: bool) -> binder::Result<()> {
        Ok(())
    }

    fn r#finishSession(&self) -> binder::Result<()> {
        self.invoker.invoke(Box::new(field::finish));
        Ok(())
    }

    fn r#finishInput(&self) -> binder::Result<()> {
        self.invoker.invoke(Box::new(field::finish));
        Ok(())
    }

    /// IInputMethodSessionWrapper.invalidateInput: the same editor with a new connection state and session id.
    fn r#invalidateInput(
        &self,
        editor: Option<&EditorInfo>,
        connection: Option<&Strong<dyn IRemoteInputConnection>>,
        session_id: i32,
    ) -> binder::Result<()> {
        start_field(&self.invoker, connection, editor, session_id);
        Ok(())
    }

    fn r#removeImeSurface(&self) -> binder::Result<()> {
        Ok(())
    }
}

/// The process's input method binder, its calls reaching the UI thread through `invoker` (the toolkit's).
pub fn new_binder(invoker: Arc<dyn UiInvoker>) -> SpIBinder {
    BnInputMethod::new_binder(InputMethod { invoker }, BinderFeatures::default()).as_binder()
}
