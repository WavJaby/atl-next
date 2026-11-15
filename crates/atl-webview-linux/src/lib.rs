// SPDX-License-Identifier: MIT
//! The WebView module's exports (`atl_platform_ui_api::abi::WebViewVtable`): WebKitGTK behind the facade's `PlatformWebEngine`.
//! Default UA: `webkit_settings_get_user_agent` of fresh settings -- the engine's own default. Measured (W1c probe, WebKitGTK 2.54): works with no display, without gtk_init and off the main thread, so a provider call from an app worker thread (AdMob) needs no UI.
//! Views: a `WebKitWebView` never parented (W0: loads and runs JS unrealized). Every `view_*` runs on the GTK main thread (the Android UI thread); signals arrive there.

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use atl_platform_ui_api::abi::{
    self, BoolSetting, BytesOut, Callback, EvalResultView, PromptReplyAbi, Status, Str,
    WebEventView, WebViewHandle, WebViewVtable, web_event,
};
use atl_platform_ui_api::webview::LoadError;
use webkit6::glib;
use webkit6::prelude::*;

static VTABLE: WebViewVtable = WebViewVtable {
    abi: abi::ABI_ID,
    default_user_agent,
    view_create,
    view_drop,
    view_load_uri,
    view_load_bytes,
    view_stop,
    view_reload,
    view_evaluate,
    view_set_user_scripts,
    view_uri,
    view_title,
    view_progress,
    view_set_bool,
    view_get_bool,
    view_set_user_agent,
    view_user_agent,
};

/// [`abi::WebViewEntry`]: the static vtable; starts nothing.
#[unsafe(no_mangle)]
pub extern "C" fn atl_webview_entry() -> *const WebViewVtable {
    &VTABLE
}

const NO_DISPLAY: &[u8] = b"no display: the process has no GTK display (no-UI mode)";

unsafe extern "C" fn default_user_agent(out: *mut BytesOut) -> bool {
    // WebKitSettings' constructor reads the default GDK display's GL support and dereferences a missing display (SIGSEGV, measured in the no-UI app run and a probe with no reachable compositor).
    // The display is GTK's: opened by the UI module's attach (same libgtk object in the process). A process without one gets a refusal, never the crash; GTK is not initialised here, because that would bind GTK's main thread to whichever thread called.
    if webkit6::gdk::Display::default().is_none() {
        // SAFETY: `out` is the sink the caller lent for this call.
        unsafe { BytesOut::put_to(out, NO_DISPLAY) };
        return false;
    }
    let ua = webkit6::Settings::new().user_agent().unwrap_or_default();
    // SAFETY: as above.
    unsafe { BytesOut::put_to(out, ua.as_bytes()) };
    true
}

/// A view and its event sink (shared with the signal closures).
struct View {
    web: webkit6::WebView,
    /// Kept so the sink drops with the view (the signal closures' clones drop with the WebKit object).
    _events: Rc<Events>,
}

fn view<'a>(h: *mut WebViewHandle) -> &'a View {
    // SAFETY: every handle is a Box<View> `view_create` leaked, alive until `view_drop`; the caller's contract (UI thread, live handle).
    unsafe { &*h.cast::<View>() }
}

/// One event not yet delivered.
struct Pending {
    kind: u32,
    text: String,
    detail: String,
    error: u32,
    progress: i32,
    reply: Option<PromptReplyAbi>,
}

/// The sink plus a queue: a callback may call back into the view (loadUrl from onPageFinished) and WebKit may signal synchronously inside it; such a nested event waits until the running one returns, so order is kept and nothing is dropped.
struct Events {
    sink: RefCell<Callback<WebEventView>>,
    queue: RefCell<std::collections::VecDeque<Pending>>,
    busy: std::cell::Cell<bool>,
    /// The title last reported for the current load.
    title: RefCell<Option<String>>,
}

impl Events {
    /// Reports `w`'s title if it differs from the last one reported for this load.
    fn title_changed(&self, w: &webkit6::WebView) {
        let Some(t) = w.title().map(|t| t.to_string()) else {
            return;
        };
        if self.title.borrow().as_deref() == Some(t.as_str()) {
            return;
        }
        self.title.replace(Some(t.clone()));
        self.emit(web_event::TITLE, &t, "", 0, 0, None);
    }

    fn emit(
        &self,
        kind: u32,
        text: &str,
        detail: &str,
        error: u32,
        progress: i32,
        reply: Option<PromptReplyAbi>,
    ) {
        self.queue.borrow_mut().push_back(Pending {
            kind,
            text: text.to_owned(),
            detail: detail.to_owned(),
            error,
            progress,
            reply,
        });
        if self.busy.replace(true) {
            return;
        }
        loop {
            let Some(p) = self.queue.borrow_mut().pop_front() else {
                break;
            };
            let handed = p.reply.is_some();
            let ev = WebEventView {
                kind: p.kind,
                text: Str::of(&p.text),
                detail: Str::of(&p.detail),
                error: p.error,
                progress: p.progress,
                // the receiver takes `reply` only for PROMPT; any other event carries a placeholder this side drops
                reply: ManuallyDrop::new(p.reply.unwrap_or_else(|| PromptReplyAbi::new(|_| {}))),
            };
            self.sink.borrow_mut().call(&ev);
            if !handed {
                drop(ManuallyDrop::into_inner(ev.reply));
            }
        }
        self.busy.set(false);
    }
}

fn load_error(e: &glib::Error) -> LoadError {
    use webkit6::{NetworkError, PolicyError};
    if e.matches(NetworkError::Cancelled)
        || e.matches(PolicyError::FrameLoadInterruptedByPolicyChange)
    {
        LoadError::Cancelled
    } else if e.matches(NetworkError::FileDoesNotExist) {
        LoadError::NotFound
    } else if e.domain() == NetworkError::domain() {
        LoadError::Network
    } else {
        LoadError::Other
    }
}

unsafe extern "C" fn view_create(
    events: Callback<WebEventView>,
    out: *mut *mut WebViewHandle,
    err: *mut BytesOut,
) -> Status {
    if webkit6::gdk::Display::default().is_none() {
        // SAFETY: the sink the caller lent.
        unsafe { BytesOut::put_to(err, NO_DISPLAY) };
        return 1;
    }
    let web = webkit6::WebView::new();
    if let Some(s) = WebViewExt::settings(&web) {
        s.set_enable_javascript(false);
        s.set_enable_html5_local_storage(false);
    }
    let events = Rc::new(Events {
        sink: RefCell::new(events),
        queue: RefCell::default(),
        busy: std::cell::Cell::new(false),
        title: RefCell::default(),
    });
    let ev = events.clone();
    web.connect_load_changed(move |w, e| {
        let uri = w.uri().map(|u| u.to_string()).unwrap_or_default();
        match e {
            webkit6::LoadEvent::Started => {
                // a new load reports its title again, even an equal one
                ev.title.replace(None);
                ev.emit(web_event::LOAD_STARTED, &uri, "", 0, 0, None);
            }
            webkit6::LoadEvent::Finished => {
                ev.emit(web_event::LOAD_FINISHED, &uri, "", 0, 0, None);
            }
            _ => {}
        }
    });
    let ev = events.clone();
    web.connect_load_failed(move |_, _, uri, e| {
        ev.emit(
            web_event::LOAD_FAILED,
            uri,
            e.message(),
            load_error(e) as u32,
            0,
            None,
        );
        false // WebKit's own error page handling continues, then LOAD_FINISHED follows
    });
    let ev = events.clone();
    web.connect_title_notify(move |w| ev.title_changed(w));
    let ev = events.clone();
    web.connect_estimated_load_progress_notify(move |w| {
        ev.emit(
            web_event::PROGRESS,
            "",
            "",
            0,
            (w.estimated_load_progress() * 100.0).round() as i32,
            None,
        );
    });
    let ev = events.clone();
    web.connect_script_dialog(move |_, dialog| {
        if dialog.dialog_type() != webkit6::ScriptDialogType::Prompt {
            return false; // alert / confirm / beforeunload: WebKit's default (WebChromeClient dialogs are later slices)
        }
        let message = dialog.message().map(|m| m.to_string()).unwrap_or_default();
        let default = dialog
            .prompt_get_default_text()
            .map(|m| m.to_string())
            .unwrap_or_default();
        // answered asynchronously (WebKit keeps the dialog pending while a ref is held and close is not called); the answer may come from any thread, so it hops back to the main context
        let held = glib::thread_guard::ThreadGuard::new(dialog.clone());
        let reply = PromptReplyAbi::new(move |text: Option<String>| {
            glib::MainContext::default().invoke(move || {
                let d = held.into_inner();
                if let Some(t) = text {
                    d.prompt_set_text(&t);
                }
                d.close();
            });
        });
        ev.emit(web_event::PROMPT, &message, &default, 0, 0, Some(reply));
        true
    });
    // SAFETY: `out` is the caller's out-parameter.
    unsafe {
        *out = Box::into_raw(Box::new(View {
            web,
            _events: events,
        }))
        .cast()
    };
    abi::OK
}

unsafe extern "C" fn view_drop(h: *mut WebViewHandle) {
    // SAFETY: the Box `view_create` leaked; dropped once.
    let v = unsafe { Box::from_raw(h.cast::<View>()) };
    v.web.try_close();
    drop(v);
}

unsafe extern "C" fn view_load_uri(h: *mut WebViewHandle, uri: Str) {
    // SAFETY: `uri` is alive for this call.
    view(h).web.load_uri(&unsafe { uri.get() });
}

unsafe extern "C" fn view_load_bytes(
    h: *mut WebViewHandle,
    content: *const u8,
    len: usize,
    mime: Str,
    enc: Str,
    base: Str,
) {
    // SAFETY: the caller lends `content[..len]` and the strings for this call.
    let (bytes, mime, enc, base) = unsafe {
        (
            std::slice::from_raw_parts(content, len),
            mime.get(),
            enc.get(),
            base.get(),
        )
    };
    view(h).web.load_bytes(
        &glib::Bytes::from(bytes),
        Some(&mime),
        Some(&enc),
        Some(&base),
    );
}

unsafe extern "C" fn view_stop(h: *mut WebViewHandle) {
    view(h).web.stop_loading();
}

unsafe extern "C" fn view_reload(h: *mut WebViewHandle) {
    view(h).web.reload();
}

unsafe extern "C" fn view_evaluate(
    h: *mut WebViewHandle,
    script: Str,
    done: Callback<EvalResultView>,
) {
    let mut done = Some(done);
    let mut finish = move |ok: bool, text: &str| {
        if let Some(mut d) = done.take() {
            d.call(&EvalResultView {
                ok,
                text: Str::of(text),
            });
        }
    };
    // SAFETY: `script` is alive for this call (copied before the async call returns).
    let script = unsafe { script.get() }.into_owned();
    view(h).web.evaluate_javascript(
        &script,
        None,
        None,
        None::<&webkit6::gio::Cancellable>,
        move |r| match r {
            // undefined, a function, a symbol: no JSON form -> "null", as Android's evaluateJavascript reports them
            Ok(v) => finish(
                true,
                &v.to_json(0)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "null".into()),
            ),
            Err(e) => finish(false, e.message()),
        },
    );
}

unsafe extern "C" fn view_set_user_scripts(h: *mut WebViewHandle, sources: *const Str, len: usize) {
    let Some(ucm) = view(h).web.user_content_manager() else {
        return;
    };
    ucm.remove_all_scripts();
    // SAFETY: the caller lends `sources[..len]` and their strings for this call.
    for s in unsafe { std::slice::from_raw_parts(sources, len) } {
        // SAFETY: as above.
        let src = unsafe { s.get() };
        ucm.add_script(&webkit6::UserScript::new(
            &src,
            webkit6::UserContentInjectedFrames::AllFrames,
            webkit6::UserScriptInjectionTime::Start,
            &[],
            &[],
        ));
    }
}

unsafe extern "C" fn view_uri(h: *mut WebViewHandle, out: *mut BytesOut) -> bool {
    view(h).web.uri().is_some_and(|u| {
        // SAFETY: the sink the caller lent.
        unsafe { BytesOut::put_to(out, u.as_bytes()) };
        true
    })
}

unsafe extern "C" fn view_title(h: *mut WebViewHandle, out: *mut BytesOut) -> bool {
    view(h).web.title().is_some_and(|u| {
        // SAFETY: the sink the caller lent.
        unsafe { BytesOut::put_to(out, u.as_bytes()) };
        true
    })
}

unsafe extern "C" fn view_progress(h: *mut WebViewHandle) -> i32 {
    (view(h).web.estimated_load_progress() * 100.0).round() as i32
}

fn settings(h: *mut WebViewHandle) -> webkit6::Settings {
    // a WebKitWebView always has settings (webkit_web_view_get_settings never returns NULL once constructed)
    WebViewExt::settings(&view(h).web).expect("WebKitWebView without settings")
}

unsafe extern "C" fn view_set_bool(h: *mut WebViewHandle, which: BoolSetting, on: bool) {
    let s = settings(h);
    match which {
        BoolSetting::JavaScript => s.set_enable_javascript(on),
        BoolSetting::DomStorage => s.set_enable_html5_local_storage(on),
    }
}

unsafe extern "C" fn view_get_bool(h: *mut WebViewHandle, which: BoolSetting) -> bool {
    let s = settings(h);
    match which {
        BoolSetting::JavaScript => s.enables_javascript(),
        BoolSetting::DomStorage => s.enables_html5_local_storage(),
    }
}

unsafe extern "C" fn view_set_user_agent(h: *mut WebViewHandle, ua: Str) {
    // SAFETY: `ua` is alive for this call.
    settings(h).set_user_agent(Some(&unsafe { ua.get() }));
}

unsafe extern "C" fn view_user_agent(h: *mut WebViewHandle, out: *mut BytesOut) {
    let ua = settings(h).user_agent().unwrap_or_default();
    // SAFETY: the sink the caller lent.
    unsafe { BytesOut::put_to(out, ua.as_bytes()) };
}
