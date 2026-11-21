// SPDX-License-Identifier: MIT
//! The focused field, on the display thread: the desktop input method's events (the window's TextInput) as calls on the field's IRemoteInputConnection, the way an Android IME drives its
//! InputConnection (InputMethodService.getCurrentInputConnection -> RemoteInputConnection, core/java/android/inputmethodservice/RemoteInputConnection.java: one oneway call per edit,
//! each with an InputConnectionCommandHeader of the session id). GTK objects and this state live on the display thread only; binder threads reach it through the display invoker (method.rs).
//! Mapping (IME study §7): preedit -> setComposingText(text, 1); preedit end / empty preedit -> setComposingText("", 1) (the composing text goes, a following commit replaces nothing);
//! commit -> commitText(text, 1) (replaces the composing region, cursor after it); delete-surrounding -> deleteSurroundingTextInCodePoints (units.rs). newCursorPosition 1 = cursor right after the
//! inserted text (InputConnection.setComposingText docs).
//! Surrounding text: the field's [`EditorText`] copy answers the toolkit synchronously (editor_text.rs: GTK reads it inside the same `done` that delivered the edit, before any editor answer
//! could arrive over the outbox). Each edit is applied to the copy before it is sent; EditorInfo's initial surrounding text and each getSurroundingText answer (asked for on every updateSelection)
//! replace it. An answer to a request older than the last edit is stale and dropped; an adopted answer that disagrees with a predicted copy is a divergence, logged and counted
//! (`atl-ime: editor text diverged n=`), an agreeing one `confirmed n=`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use atl_aidl::mangled::_3_com_7_android_8_internal_11_inputmethod_22_IRemoteInputConnection as IRemoteInputConnection;
use atl_parcelables::android::view::inputmethod::SurroundingText;
use atl_parcelables::codecs::com::android::internal::infra::AndroidFuture::FutureState;
use atl_parcelables::com::android::internal::infra::AndroidFuture;
use atl_parcelables::com::android::internal::inputmethod::InputConnectionCommandHeader;
use atl_parcelables::java::lang::CharSequence;
use atl_platform_ui::{
    DisplayInvoker, LooperPoster, PlatformWindow, TextInput, TextInputEvent, Window,
};
use binder::Strong;

use crate::editor_text::EditorText;
use crate::units;

/// The editor's connection and the session id its calls carry (0 for a new connection, the invalidateInput id after one: RemoteInputConnectionImpl.java:496).
pub(crate) struct Field {
    /// Some until drop, which hands it to the outbox.
    connection: Option<Strong<dyn IRemoteInputConnection>>,
    session_id: i32,
    input_type: i32,
    /// For the surrounding text's answer, which arrives on a binder thread (method.rs `SurroundingFuture`).
    invoker: Arc<dyn DisplayInvoker>,
    /// Where every call on `connection` is sent from, and where it is released: an attached thread, in post order (display-thread rule B1: an in-process Java editor runs a call, and its
    /// JavaBBinder's destructor, on the caller's thread, which needs a JNIEnv the display thread never has).
    outbox: Arc<dyn LooperPoster>,
    /// The copy of the editor's text; `None` until the editor first reports it (a password field's EditorInfo carries none).
    text: Option<EditorText>,
    /// Edits sent so far: a getSurroundingText answer is current only when no edit was sent after its request.
    edits: u64,
    /// The copy holds edits the editor has not reported back yet.
    predicted: bool,
}

impl Field {
    pub(crate) fn new(
        connection: Strong<dyn IRemoteInputConnection>,
        session_id: i32,
        input_type: i32,
        invoker: Arc<dyn DisplayInvoker>,
        outbox: Arc<dyn LooperPoster>,
    ) -> Self {
        Field {
            connection: Some(connection),
            session_id,
            input_type,
            invoker,
            outbox,
            text: None,
            edits: 0,
            predicted: false,
        }
    }
}

/// The display thread's reference goes to the outbox, after every send posted before it: the outbox's release is then the last one, never the display thread's.
impl Drop for Field {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.outbox.post(Box::new(move || drop(connection)));
        }
    }
}

/// Sends `call` on `field`'s connection from the outbox, after every earlier send; `what` names it in the `atl-ime: sent` line, with the thread it ran on.
fn send(
    field: &Field,
    what: String,
    call: impl FnOnce(&dyn IRemoteInputConnection, &InputConnectionCommandHeader) -> binder::Result<()>
    + Send
    + 'static,
) {
    let Some(connection) = field.connection.clone() else {
        return;
    };
    let header = InputConnectionCommandHeader {
        session_id: field.session_id,
    };
    field.outbox.post(Box::new(move || {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("?");
        match call(&*connection, &header) {
            Ok(()) => tracing::info!("atl-ime: sent {what} thread={thread}"),
            Err(e) => tracing::warn!("atl-ime: {what} not sent: {e:?} thread={thread}"),
        }
    }));
}

/// UTF-16 units of text asked for on each side of the selection. Text-input v3 caps set_surrounding_text at 4000 bytes (text-input-unstable-v3.xml); a UTF-16 unit is at most 3 UTF-8 bytes
/// (BMP 1 unit -> <= 3 bytes, a surrogate pair 2 units -> 4 bytes), so 2 x 600 units <= 3600 bytes, leaving room for a short selection. A longer one is GTK's to trim.
const SURROUNDING_UNITS: i32 = 600;

#[derive(Default)]
struct State {
    window: Option<Rc<PlatformWindow>>,
    field: Option<Field>,
    /// Adopted answers that agreed with a predicted copy / disagreed with it, over the process's life.
    confirmed: u64,
    diverged: u64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// The process's window, on the display thread once it exists: its TextInput's events go to the focused field from now on.
pub fn attach(window: Rc<PlatformWindow>) {
    window.text_input().on_events(on_event);
    STATE.with_borrow_mut(|s| s.window = Some(window));
}

/// The copy, in the toolkit's units, as the TextInput's surrounding: what GTK's next retrieve-surrounding reads.
fn show(window: Option<&Rc<PlatformWindow>>, text: Option<&EditorText>) {
    let (Some(window), Some(text)) = (window, text) else {
        return;
    };
    match text.surrounding() {
        Some(s) => window.text_input().set_surrounding(&s),
        None => tracing::warn!("atl-ime: editor text has no UTF-8 form: {}", text.shape()),
    }
}

/// A field gained input (IInputMethod.startInput, IInputMethodSession.invalidateInput): the copy starts from EditorInfo's initial surrounding text (TextView.onCreateInputConnection
/// setInitialSurroundingText), shown before the focus so the desktop input method's first read has it; without one the editor is asked.
pub(crate) fn start(mut field: Field, initial: Option<SurroundingText>) {
    field.text = initial.and_then(|t| report(&t, None));
    let ask = field.text.is_none();
    STATE.with_borrow_mut(|s| {
        let (purpose, hints) = crate::content::content(field.input_type);
        tracing::info!(
            "atl-ime: start input_type=0x{:x} session={} purpose={purpose:?} initial text {}",
            field.input_type,
            field.session_id,
            field
                .text
                .as_ref()
                .map_or("none".to_owned(), EditorText::shape)
        );
        s.field = Some(field);
        if let Some(window) = &s.window {
            let input = window.text_input();
            input.set_content(purpose, hints);
            show(
                s.window.as_ref(),
                s.field.as_ref().and_then(|f| f.text.as_ref()),
            );
            input.focus_in();
        }
    });
    // outside the mutable borrow: the in-process editor may answer (and reach `surrounding`) before the call returns
    if ask {
        selection_changed(None);
    }
}

/// `text` as a copy; `None` (logged) when its selection does not fit it.
fn report(text: &SurroundingText, composing: Option<(i32, i32)>) -> Option<EditorText> {
    let content = text.text.as_deref().unwrap_or_default();
    EditorText::from_report(
        content,
        text.selection_start,
        text.selection_end,
        text.offset,
        composing,
    )
    .inspect_err(|e| tracing::warn!("atl-ime: editor text report unusable: {e:?}"))
    .ok()
}

/// Asks the editor for the text around its selection (IRemoteInputConnection.getSurroundingText, oneway): the answer completes a pending AndroidFuture on a binder thread, and nothing waits for it.
/// The future carries the edit count at the request and `composing` (the reporting updateSelection's candidates), which the answer's SurroundingText lacks.
fn request_surrounding(field: &Field, composing: Option<(i32, i32)>) {
    let future = crate::method::surrounding_future(
        Arc::clone(&field.invoker),
        field.session_id,
        field.edits,
        composing,
    );
    send(field, "getSurroundingText".to_owned(), move |c, header| {
        let future = AndroidFuture::new(FutureState::Pending(Some(future)));
        c.r#getSurroundingText(
            Some(header),
            SURROUNDING_UNITS,
            SURROUNDING_UNITS,
            0,
            Some(&future),
        )
    });
}

/// The editor's selection or composing span changed (IInputMethodSession.updateSelection; `composing` = its candidates, absolute): its text is asked for again, to confirm or correct the copy.
pub(crate) fn selection_changed(composing: Option<(i32, i32)>) {
    STATE.with_borrow(|s| {
        if let Some(field) = &s.field {
            request_surrounding(field, composing);
        }
    });
}

/// The editor's answer for session `session_id`, asked for after `edits` edits: it replaces the copy unless an edit was sent since (the editor answered before it had that edit: stale, and the
/// edit's own updateSelection asks again). An answer for an older session (the field restarted meanwhile) is dropped.
pub(crate) fn surrounding(
    session_id: i32,
    edits: u64,
    composing: Option<(i32, i32)>,
    text: SurroundingText,
) {
    STATE.with_borrow_mut(|s| {
        let State {
            window,
            field,
            confirmed,
            diverged,
        } = s;
        let Some(field) = field.as_mut() else {
            return;
        };
        if field.session_id != session_id {
            return;
        }
        if field.edits != edits {
            tracing::debug!(
                "atl-ime: editor text stale: asked after {edits} edits, {} sent",
                field.edits
            );
            return;
        }
        let Some(new) = report(&text, composing) else {
            return;
        };
        match (&field.text, field.predicted) {
            (Some(copy), true) if copy.agrees(&new) => {
                *confirmed += 1;
                tracing::info!(
                    "atl-ime: editor text confirmed n={confirmed}: {}",
                    new.shape()
                );
            }
            (Some(copy), true) => {
                *diverged += 1;
                tracing::warn!(
                    "atl-ime: editor text diverged n={diverged}: copy {} editor {}",
                    copy.shape(),
                    new.shape()
                );
            }
            // the copy came from the editor: a difference is the editor's own change
            _ => tracing::info!("atl-ime: editor text {}", new.shape()),
        }
        field.text = Some(new);
        field.predicted = false;
        show(window.as_ref(), field.text.as_ref());
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

/// One edit: applied to the copy and shown (GTK reads it before this signal's `done` ends), then sent.
fn on_event(event: TextInputEvent) {
    STATE.with_borrow_mut(|s| {
        let State { window, field, .. } = s;
        let Some(field) = field.as_mut() else {
            tracing::debug!("atl-ime: {event:?} with no field");
            return;
        };
        let what = format!("{event:?}");
        let edit = match event {
            TextInputEvent::Preedit { text, .. } => Edit::Compose(text),
            TextInputEvent::PreeditEnd => Edit::Compose(String::new()),
            TextInputEvent::Commit(text) => Edit::Commit(text),
            TextInputEvent::DeleteSurrounding {
                offset_chars,
                n_chars,
            } => match units::delete_to_code_points(offset_chars, n_chars) {
                Some((before, after)) => Edit::Delete(before, after),
                None => {
                    tracing::warn!("atl-ime: delete-surrounding off the cursor: {what}");
                    return;
                }
            },
        };
        if let Some(text) = field.text.as_mut() {
            match &edit {
                Edit::Compose(new) => text.replace(new, true),
                Edit::Commit(new) => text.replace(new, false),
                Edit::Delete(before, after) => text.delete_code_points(*before, *after),
            }
            field.predicted = true;
        }
        field.edits += 1;
        show(window.as_ref(), field.text.as_ref());
        send(field, what, move |c, h| match edit {
            Edit::Compose(new) => {
                c.r#setComposingText(Some(h), Some(&CharSequence::new(new.as_str())), 1)
            }
            Edit::Commit(new) => c.r#commitText(Some(h), Some(&CharSequence::new(new.as_str())), 1),
            Edit::Delete(before, after) => c.r#deleteSurroundingTextInCodePoints(
                Some(h),
                i32::try_from(before).unwrap_or(i32::MAX),
                i32::try_from(after).unwrap_or(i32::MAX),
            ),
        });
    });
}

/// A TextInput event as the InputConnection edit it becomes.
enum Edit {
    /// setComposingText(text, 1); "" ends composing.
    Compose(String),
    /// commitText(text, 1).
    Commit(String),
    /// deleteSurroundingTextInCodePoints(before, after).
    Delete(u32, u32),
}
