// SPDX-License-Identifier: MIT
//! The focused field, on the UI thread: the desktop input method's events (the window's TextInput) as calls on the field's IRemoteInputConnection, the way an Android IME drives its
//! InputConnection (InputMethodService.getCurrentInputConnection -> RemoteInputConnection, core/java/android/inputmethodservice/RemoteInputConnection.java: one oneway call per edit,
//! each with an InputConnectionCommandHeader of the session id). GTK objects and this state live on the UI thread only; binder threads reach it through the UI invoker (method.rs).
//! Mapping (IME study §7): preedit -> setComposingText(text, 1); preedit end / empty preedit -> setComposingText("", 1) (the composing text goes, a following commit replaces nothing);
//! commit -> commitText(text, 1) (replaces the composing region, cursor after it); delete-surrounding -> deleteSurroundingTextInCodePoints (units.rs). newCursorPosition 1 = cursor right after the
//! inserted text (InputConnection.setComposingText docs).

use std::cell::RefCell;
use std::rc::Rc;

use atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_22_IRemoteInputConnection as IRemoteInputConnection;
use atl_parcelables::com::android::internal::inputmethod::InputConnectionCommandHeader;
use atl_parcelables::java::lang::CharSequence;
use atl_platform_ui::{PlatformWindow, TextInput, TextInputEvent, Window};
use binder::Strong;

use crate::units;

/// The editor's connection and the session id its calls carry (0 for a new connection, the invalidateInput id after one: RemoteInputConnectionImpl.java:496).
pub(crate) struct Field {
    pub connection: Strong<dyn IRemoteInputConnection>,
    pub session_id: i32,
    pub input_type: i32,
}

#[derive(Default)]
struct State {
    window: Option<Rc<PlatformWindow>>,
    field: Option<Field>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// The process's window, on the UI thread once it exists: its TextInput's events go to the focused field from now on.
pub fn attach(window: Rc<PlatformWindow>) {
    window.text_input().on_events(on_event);
    STATE.with_borrow_mut(|s| s.window = Some(window));
}

/// A field gained input (IInputMethod.startInput, IInputMethodSession.invalidateInput): the desktop input method starts composing for it.
pub(crate) fn start(field: Field) {
    STATE.with_borrow_mut(|s| {
        let (purpose, hints) = crate::content::content(field.input_type);
        tracing::info!(
            "atl-ime: start input_type=0x{:x} session={} purpose={purpose:?}",
            field.input_type,
            field.session_id
        );
        s.field = Some(field);
        if let Some(window) = &s.window {
            let input = window.text_input();
            input.set_content(purpose, hints);
            input.focus_in();
        }
    });
}

/// The field's input ended (IInputMethodSession.finishInput, IInputMethod.unbindInput): composing stops.
pub(crate) fn finish() {
    STATE.with_borrow_mut(|s| {
        if s.field.take().is_some() {
            tracing::info!("atl-ime: finish");
            if let Some(window) = &s.window {
                window.text_input().focus_out();
            }
        }
    });
}

fn on_event(event: TextInputEvent) {
    STATE.with_borrow(|s| {
        let Some(field) = &s.field else {
            tracing::debug!("atl-ime: {event:?} with no field");
            return;
        };
        let header = InputConnectionCommandHeader {
            session_id: field.session_id,
        };
        let c = &field.connection;
        let sent = match &event {
            TextInputEvent::Preedit { text, .. } => {
                c.r#setComposingText(Some(&header), Some(&CharSequence::new(text.as_str())), 1)
            }
            TextInputEvent::PreeditEnd => {
                c.r#setComposingText(Some(&header), Some(&CharSequence::new("")), 1)
            }
            TextInputEvent::Commit(text) => {
                c.r#commitText(Some(&header), Some(&CharSequence::new(text.as_str())), 1)
            }
            TextInputEvent::DeleteSurrounding {
                offset_chars,
                n_chars,
            } => match units::delete_to_code_points(*offset_chars, *n_chars) {
                Some((before, after)) => c.r#deleteSurroundingTextInCodePoints(
                    Some(&header),
                    i32::try_from(before).unwrap_or(i32::MAX),
                    i32::try_from(after).unwrap_or(i32::MAX),
                ),
                None => {
                    tracing::warn!("atl-ime: delete-surrounding off the cursor: {event:?}");
                    Ok(())
                }
            },
        };
        match sent {
            Ok(()) => tracing::info!("atl-ime: sent {event:?}"),
            Err(e) => tracing::warn!("atl-ime: {event:?} not sent: {e:?}"),
        }
    });
}
