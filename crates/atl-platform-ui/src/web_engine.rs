// SPDX-License-Identifier: MIT
//! The web engine's module (webview plan S2.3): `libatl_webview_linux.so`, loaded `RTLD_NOW | RTLD_LOCAL` on the first [`PlatformWebEngine`] call, from any thread; a failure is kept and every later call gets the same text. Identity check as the UI module's.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};

use atl_platform_ui_api::abi::{
    self, BytesOut, Callback, CookieList, CookieView, EvalResultView, SendCallback, Str, UiVtable,
    WebEventView, WebViewEntry, WebViewVtable, web_event,
};
use atl_platform_ui_api::webview::{
    ClaimReply, Cookie, CookiePolicy, InputClaim, LoadError, NavigationReply, NavigationRequest,
    PromptReply, SIDEBAND_WEB_VIEW, SettingValue, WebEngine, WebEvent, WebSetting, WebView,
    WebViewError,
};

static MODULE: OnceLock<Result<&'static WebViewVtable, String>> = OnceLock::new();

fn module() -> Result<&'static WebViewVtable, WebViewError> {
    MODULE
        .get_or_init(load)
        .clone()
        .map_err(WebViewError::Unavailable)
}

fn load() -> Result<&'static WebViewVtable, String> {
    let (entry, path) = crate::module::open(abi::WEBVIEW_MODULE_FILE, abi::WEBVIEW_ENTRY_SYMBOL)?;
    // SAFETY: the module's `atl_webview_entry`, an `abi::WebViewEntry` (the one definition both sides compile).
    let entry: WebViewEntry = unsafe { std::mem::transmute::<*mut c_void, WebViewEntry>(entry) };
    // SAFETY: the entry has no precondition; it returns its static vtable.
    let vtable =
        unsafe { entry().as_ref() }.ok_or_else(|| format!("platform module {path}: no vtable"))?;
    if vtable.abi != abi::ABI_ID {
        return Err(format!(
            "platform module {path}: abi {}, process {}",
            vtable.abi,
            abi::ABI_ID
        ));
    }
    // SAFETY: the sink is plain data valid for the process; its `log` is callable from any thread.
    unsafe { (vtable.set_log)(crate::module::sink()) };
    tracing::info!(
        "atl-platform-ui: web engine module {path} abi {}",
        abi::ABI_ID
    );
    Ok(vtable)
}

/// Loads the web engine module now, with the load and identity check of the first WebView call: the host's WebView capability (webview plan S6; atld declares FEATURE_WEBVIEW on Ok).
pub fn probe_web_engine() -> Result<(), String> {
    MODULE.get_or_init(load).clone().map(|_| ())
}

/// The web engine of this process (Linux: WebKitGTK in its own module).
pub struct PlatformWebEngine;

impl WebEngine for PlatformWebEngine {
    type View = PlatformWebView;

    fn default_user_agent() -> Result<String, WebViewError> {
        let m = module()?;
        // SAFETY: the sink is lent for this call only; the function is callable from any thread (vtable contract).
        let (ok, bytes) = BytesOut::collect(|out| unsafe { (m.default_user_agent)(out) });
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if ok {
            Ok(text)
        } else {
            Err(WebViewError::NoDisplay(text))
        }
    }

    fn create_view(
        events: Box<dyn FnMut(WebEvent) + Send>,
    ) -> Result<PlatformWebView, WebViewError> {
        let m = module()?;
        // two sources, one sink: the engine module's events and the UI module's unhandled-key reports, both on the display thread
        let events: Sink = Arc::new(Mutex::new(events));
        let shown_events = Arc::clone(&events);
        let sink = SendCallback::new(move |e: &WebEventView| {
            // SAFETY: the view's strings are alive for this call (module contract).
            let (text, detail) =
                unsafe { (e.text.get().into_owned(), e.detail.get().into_owned()) };
            let ev = match e.kind {
                web_event::LOAD_STARTED => WebEvent::LoadStarted(text),
                web_event::LOAD_FINISHED => WebEvent::LoadFinished(text),
                web_event::LOAD_FAILED => WebEvent::LoadFailed {
                    uri: text,
                    message: detail,
                    kind: LoadError::of(e.error),
                },
                web_event::NAVIGATION => WebEvent::Navigation {
                    request: NavigationRequest {
                        uri: text,
                        method: detail,
                        main_frame: e.flags & web_event::FLAG_MAIN_FRAME != 0,
                        redirect: e.flags & web_event::FLAG_REDIRECT != 0,
                        gesture: e.flags & web_event::FLAG_GESTURE != 0,
                    },
                    // SAFETY: a NAVIGATION event hands its reply to the receiver; this is its one take.
                    reply: Box::new(ManuallyDrop::into_inner(unsafe {
                        std::ptr::read(&e.reply)
                    })) as Box<dyn NavigationReply>,
                },
                web_event::LOCATION => WebEvent::Location {
                    uri: (e.flags & web_event::FLAG_HAS_URI != 0).then_some(text),
                    original_uri: (e.flags & web_event::FLAG_HAS_ORIGINAL_URI != 0)
                        .then_some(detail),
                    back: e.back,
                    forward: e.forward,
                },
                web_event::TITLE => WebEvent::Title(text),
                web_event::PROGRESS => WebEvent::Progress(e.progress),
                web_event::PROMPT => WebEvent::Prompt {
                    message: text,
                    default: detail,
                    // SAFETY: a PROMPT event hands its reply to the receiver; this is its one take.
                    reply: Box::new(ManuallyDrop::into_inner(unsafe {
                        std::ptr::read(&e.reply)
                    })) as Box<dyn PromptReply>,
                },
                web_event::DOWNLOAD => WebEvent::Download {
                    url: text,
                    content_disposition: detail,
                    // SAFETY: as above.
                    mime_type: unsafe { e.extra.get() }.into_owned(),
                    content_length: e.length,
                },
                _ => return,
            };
            deliver(&events, ev);
        });
        // the window draws the view through the UI module (sideband quads), so a process without it has no use for a view
        let ui = crate::module::module().map_err(WebViewError::Unavailable)?;
        static NEXT_KEY: AtomicI32 = AtomicI32::new(1);
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        let shown = SendCallback::new(move |widget: &*mut c_void| {
            if widget.is_null() {
                // SAFETY: display thread (vtable contract), before the view (the widget's owner) is destroyed.
                return unsafe { (ui.sideband_detach)(key) };
            }
            let events = Arc::clone(&shown_events);
            let unhandled_key =
                Callback::new(move |ns: &i64| deliver(&events, WebEvent::UnhandledKey(*ns)));
            // SAFETY: display thread = the UI module's thread until display-thread S5; the widget is the live view's (borrowed), the UI module takes its own reference (same libgtk in both modules).
            unsafe { (ui.sideband_attach)(key, *widget, unhandled_key) }
        });
        // SAFETY: any thread (vtable contract); the callbacks pass to the module.
        if !unsafe { (m.view_create)(key, sink, shown) } {
            return Err(WebViewError::NoDisplay(
                "no display: the process has no GTK display (no-UI mode)".into(),
            ));
        }
        Ok(PlatformWebView { m, ui, key })
    }

    fn watch_cookies(mut changed: Box<dyn FnMut(Vec<Cookie>) + Send>) -> Result<(), WebViewError> {
        let m = module()?;
        let cb = SendCallback::new(move |list: &CookieList| {
            // SAFETY: the module lends the list and its strings for this call.
            let views = unsafe { std::slice::from_raw_parts(list.ptr, list.len) };
            // SAFETY: as above.
            changed(views.iter().map(|v| unsafe { v.get() }).collect());
        });
        // SAFETY: any thread (vtable contract).
        ui_thread(unsafe { (m.cookies_watch)(cb) })
    }

    fn add_cookie(cookie: &Cookie, done: Box<dyn FnOnce(bool) + Send>) -> Result<(), WebViewError> {
        let m = module()?;
        let view = CookieView::of(cookie);
        // SAFETY: any thread; the module copies the cookie before return.
        ui_thread(unsafe { (m.cookies_add)(&view, once(done)) })
    }

    fn replace_cookies(
        cookies: &[Cookie],
        done: Box<dyn FnOnce(bool) + Send>,
    ) -> Result<(), WebViewError> {
        let m = module()?;
        let views: Vec<CookieView> = cookies.iter().map(CookieView::of).collect();
        let list = CookieList {
            ptr: views.as_ptr(),
            len: views.len(),
        };
        // SAFETY: as add_cookie; `views` borrows `cookies` for the call.
        ui_thread(unsafe { (m.cookies_replace)(list, once(done)) })
    }

    fn set_cookie_policy(policy: CookiePolicy) -> Result<(), WebViewError> {
        let m = module()?;
        // SAFETY: any thread.
        ui_thread(unsafe { (m.cookies_policy)(policy) })
    }
}

/// A module's "queued on the UI thread" answer; false = the process has no UI thread (no-UI mode).
fn ui_thread(queued: bool) -> Result<(), WebViewError> {
    queued
        .then_some(())
        .ok_or_else(|| WebViewError::NoDisplay("no UI thread for the cookie store".into()))
}

/// A view's event sink, fed on the display thread by the engine module and the UI module.
type Sink = Arc<Mutex<Box<dyn FnMut(WebEvent) + Send>>>;

/// `ev` to `events`. Both sources run on the display thread and the receiver only hands events on (WebEngine::create_view), so the lock is never held when another delivery starts; a delivery
/// that finds it held would be a receiver calling back into a source, which the trait does not allow: dropped and logged.
fn deliver(events: &Sink, ev: WebEvent) {
    match events.try_lock() {
        Ok(mut events) => events(ev),
        Err(TryLockError::Poisoned(p)) => p.into_inner()(ev),
        Err(TryLockError::WouldBlock) => {
            tracing::warn!("atl-platform-ui: view event during another delivery; dropped")
        }
    }
}

fn once(done: Box<dyn FnOnce(bool) + Send>) -> SendCallback<bool> {
    let mut done = Some(done);
    SendCallback::new(move |ok: &bool| {
        if let Some(d) = done.take() {
            d(*ok);
        }
    })
}

/// A view of the module, named by its key.
pub struct PlatformWebView {
    m: &'static WebViewVtable,
    ui: &'static UiVtable,
    /// The second int of the view's sideband stream ([`WebView::sideband_stream`]) and the module's name for the view: process-unique, never reused.
    key: i32,
}

// Every `view_*` call: any thread (vtable contract), the key is this view's (dropped only in Drop); borrowed strings live for the call (the module copies them).
impl WebView for PlatformWebView {
    fn load_uri(&self, uri: &str) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_load_uri)(self.key, Str::of(uri)) }
    }

    fn load_bytes(&self, content: &[u8], mime_type: &str, encoding: &str, base_uri: &str) {
        // SAFETY: see the impl comment.
        unsafe {
            (self.m.view_load_bytes)(
                self.key,
                content.as_ptr(),
                content.len(),
                Str::of(mime_type),
                Str::of(encoding),
                Str::of(base_uri),
            )
        }
    }

    fn stop_loading(&self) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_stop)(self.key) }
    }

    fn reload(&self) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_reload)(self.key) }
    }

    fn go_back_or_forward(&self, steps: i32) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_go)(self.key, steps) }
    }

    fn evaluate_javascript(
        &self,
        script: &str,
        done: Box<dyn FnOnce(Result<String, String>) + Send>,
    ) {
        let mut done = Some(done);
        let cb = SendCallback::new(move |r: &EvalResultView| {
            // SAFETY: the result's text is alive for this call.
            let text = unsafe { r.text.get() }.into_owned();
            if let Some(d) = done.take() {
                d(if r.ok { Ok(text) } else { Err(text) });
            }
        });
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_evaluate)(self.key, Str::of(script), cb) }
    }

    fn set_user_scripts(&self, sources: &[String]) {
        let views: Vec<Str> = sources.iter().map(|s| Str::of(s)).collect();
        // SAFETY: see the impl comment; `views` borrows `sources` for the call.
        unsafe { (self.m.view_set_user_scripts)(self.key, views.as_ptr(), views.len()) }
    }

    fn set_setting(&self, which: WebSetting, value: &SettingValue) {
        // SAFETY: see the impl comment; the value's text lives for the call.
        unsafe {
            match value {
                SettingValue::Bool(on) => (self.m.view_set_bool)(self.key, which, *on),
                SettingValue::Int(v) => (self.m.view_set_int)(self.key, which, *v),
                SettingValue::Str(s) => (self.m.view_set_str)(self.key, which, Str::of(s)),
            }
        }
    }

    fn set_background_color(&self, argb: u32) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_set_background)(self.key, argb) }
    }

    fn set_initial_scale(&self, percent: i32) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_set_initial_scale)(self.key, percent) }
    }

    fn sideband_stream(&self) -> [i32; 2] {
        [SIDEBAND_WEB_VIEW, self.key]
    }

    fn claim_input(&self, claim: InputClaim, time_ns: i64) -> ClaimReply {
        // SAFETY: UI thread (trait contract); the key is this live view's.
        ClaimReply::of_bits(unsafe { (self.ui.sideband_claim)(self.key, claim as u32, time_ns) })
    }

    fn set_focused(&self, on: bool) {
        // SAFETY: as above.
        unsafe { (self.ui.sideband_focus)(self.key, on) }
    }
}

impl Drop for PlatformWebView {
    fn drop(&mut self) {
        // SAFETY: any thread (vtable contract); dropped once. The module reports `shown(null)` (the sideband detach) on the display thread before it destroys the view.
        unsafe { (self.m.view_drop)(self.key) }
    }
}
