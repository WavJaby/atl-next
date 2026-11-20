// SPDX-License-Identifier: MIT
//! The focused field, on the UI thread: the desktop input method's events (the window's TextInput) as calls on the field's IRemoteInputConnection, the way an Android IME drives its
//! InputConnection (InputMethodService.getCurrentInputConnection -> RemoteInputConnection, core/java/android/inputmethodservice/RemoteInputConnection.java: one oneway call per edit,
//! each with an InputConnectionCommandHeader of the session id). GTK objects and this state live on the UI thread only; binder threads reach it through the UI invoker (method.rs).
//! Mapping (IME study §7): preedit -> setComposingText(text, 1); preedit end / empty preedit -> setComposingText("", 1) (the composing text goes, a following commit replaces nothing);
//! commit -> commitText(text, 1) (replaces the composing region, cursor after it); delete-surrounding -> deleteSurroundingTextInCodePoints (units.rs). newCursorPosition 1 = cursor right after the
//! inserted text (InputConnection.setComposingText docs).
//! Surrounding text (plan step 6): on start and on each updateSelection, getSurroundingText with a pending AndroidFuture; its completion (binder thread) comes back here and goes to
//! TextInput.set_surrounding. Never a local copy of the editor's text: a mirror built from EditorInfo + own edits goes wrong as soon as keys or the app edit the field.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_22_IRemoteInputConnection as IRemoteInputConnection;
use atl_parcelables::android::view::inputmethod::SurroundingText;
use atl_parcelables::codecs::com::android::internal::infra::AndroidFuture::FutureState;
use atl_parcelables::com::android::internal::infra::AndroidFuture;
use atl_parcelables::com::android::internal::inputmethod::InputConnectionCommandHeader;
use atl_parcelables::java::lang::CharSequence;
use atl_platform_ui::{PlatformWindow, Surrounding, TextInput, TextInputEvent, UiInvoker, Window};
use binder::Strong;

use crate::units;

/// The editor's connection and the session id its calls carry (0 for a new connection, the invalidateInput id after one: RemoteInputConnectionImpl.java:496).
pub(crate) struct Field {
    pub connection: Strong<dyn IRemoteInputConnection>,
    pub session_id: i32,
    pub input_type: i32,
    /// For the surrounding text's answer, which arrives on a binder thread (method.rs `SurroundingFuture`).
    pub invoker: Arc<dyn UiInvoker>,
}

/// UTF-16 units of text asked for on each side of the selection. Text-input v3 caps set_surrounding_text at 4000 bytes (text-input-unstable-v3.xml); a UTF-16 unit is at most 3 UTF-8 bytes
/// (BMP 1 unit -> <= 3 bytes, a surrogate pair 2 units -> 4 bytes), so 2 x 600 units <= 3600 bytes, leaving room for a short selection. A longer one is GTK's to trim.
const SURROUNDING_UNITS: i32 = 600;

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
    // outside the mutable borrow: the in-process editor may answer (and reach `surrounding`) before the call returns
    selection_changed();
}

/// Asks the editor for the text around its selection (IRemoteInputConnection.getSurroundingText, oneway): the answer completes a pending AndroidFuture on a binder thread, and nothing waits for it.
fn request_surrounding(field: &Field) {
    let header = InputConnectionCommandHeader {
        session_id: field.session_id,
    };
    let future = crate::method::surrounding_future(Arc::clone(&field.invoker), field.session_id);
    let future = AndroidFuture::new(FutureState::Pending(Some(future)));
    if let Err(e) = field.connection.r#getSurroundingText(
        Some(&header),
        SURROUNDING_UNITS,
        SURROUNDING_UNITS,
        0,
        Some(&future),
    ) {
        tracing::warn!("atl-ime: getSurroundingText not sent: {e:?}");
    }
}

/// The editor's selection moved (IInputMethodSession.updateSelection): its surrounding text is asked for again. No copy of the editor's text is kept here; the editor is the only one.
pub(crate) fn selection_changed() {
    STATE.with_borrow(|s| {
        if let Some(field) = &s.field {
            request_surrounding(field);
        }
    });
}

/// The editor's answer for session `session_id`: the window's TextInput gets it in the toolkit's units. An answer for an older session (the field restarted meanwhile) is dropped.
pub(crate) fn surrounding(session_id: i32, text: SurroundingText) {
    STATE.with_borrow(|s| {
        let (Some(field), Some(window)) = (&s.field, &s.window) else {
            return;
        };
        if field.session_id != session_id {
            return;
        }
        let content = text.text.unwrap_or_default();
        let index = |i: i32| usize::try_from(i).ok();
        let bytes = index(text.selection_start)
            .zip(index(text.selection_end))
            .map(|(start, end)| units::selection_to_bytes(&content, start, end));
        match bytes {
            Some(Ok((cursor_byte, anchor_byte))) => {
                tracing::info!(
                    "atl-ime: surrounding {} bytes cursor {cursor_byte} anchor {anchor_byte}",
                    content.len()
                );
                window.text_input().set_surrounding(&Surrounding {
                    text: content,
                    cursor_byte,
                    anchor_byte,
                });
            }
            other => tracing::warn!(
                "atl-ime: surrounding selection {}..{} unusable in {} units: {other:?}",
                text.selection_start,
                text.selection_end,
                content.encode_utf16().count()
            ),
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
