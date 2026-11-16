// SPDX-License-Identifier: MIT
//! The web engine's module (webview plan S2.3): `libatl_webview_linux.so`, loaded `RTLD_NOW | RTLD_LOCAL` on the first [`PlatformWebEngine`] call, from any thread; a failure is kept and every later call gets the same text. Identity check as the UI module's.

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};

use atl_platform_ui_api::abi::{
    self, BoolSetting, BytesOut, Callback, EvalResultView, Str, UiVtable, WebEventView,
    WebViewEntry, WebViewHandle, WebViewVtable, web_event,
};
use atl_platform_ui_api::webview::{
    LoadError, PromptReply, SIDEBAND_WEB_VIEW, WebEngine, WebEvent, WebView, WebViewError,
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
    tracing::info!(
        "atl-platform-ui: web engine module {path} abi {}",
        abi::ABI_ID
    );
    Ok(vtable)
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

    fn create_view(mut events: Box<dyn FnMut(WebEvent)>) -> Result<PlatformWebView, WebViewError> {
        let m = module()?;
        let sink = Callback::new(move |e: &WebEventView| {
            // SAFETY: the view's strings are alive for this call (module contract).
            let (text, detail) =
                unsafe { (e.text.get().into_owned(), e.detail.get().into_owned()) };
            let ev = match e.kind {
                web_event::LOAD_STARTED => WebEvent::LoadStarted(text),
                web_event::LOAD_FINISHED => WebEvent::LoadFinished(text),
                web_event::LOAD_FAILED => WebEvent::LoadFailed {
                    uri: text,
                    message: detail,
                    kind: match e.error {
                        1 => LoadError::Cancelled,
                        2 => LoadError::Network,
                        3 => LoadError::NotFound,
                        _ => LoadError::Other,
                    },
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
                _ => return,
            };
            events(ev);
        });
        // the window draws the view through the UI module (sideband quads), so a process without it has no use for a view
        let ui = crate::module::module().map_err(WebViewError::Unavailable)?;
        let mut h: *mut WebViewHandle = std::ptr::null_mut();
        // SAFETY: UI thread (the trait's contract); out and err live for the call.
        let (status, err) = BytesOut::collect(|err| unsafe { (m.view_create)(sink, &mut h, err) });
        if status != abi::OK || h.is_null() {
            return Err(WebViewError::NoDisplay(
                String::from_utf8_lossy(&err).into_owned(),
            ));
        }
        static NEXT_KEY: AtomicI32 = AtomicI32::new(1);
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        // SAFETY: UI thread; the widget is the live view's (borrowed), the UI module takes its own reference (same libgtk in both modules).
        unsafe { (ui.sideband_attach)(key, (m.view_widget)(h)) };
        Ok(PlatformWebView { m, ui, h, key })
    }
}

/// A view of the module; UI thread only.
pub struct PlatformWebView {
    m: &'static WebViewVtable,
    ui: &'static UiVtable,
    h: *mut WebViewHandle,
    /// The second int of the view's sideband stream ([`WebView::sideband_stream`]): process-unique, never reused.
    key: i32,
}

impl PlatformWebView {
    fn text(
        &self,
        f: unsafe extern "C" fn(*mut WebViewHandle, *mut BytesOut) -> bool,
    ) -> Option<String> {
        // SAFETY: a live handle on the UI thread; the sink lives for the call.
        let (some, bytes) = BytesOut::collect(|out| unsafe { f(self.h, out) });
        some.then(|| String::from_utf8_lossy(&bytes).into_owned())
    }
}

// Every method: a live handle (dropped only in Drop) on the UI thread (trait contract); borrowed strings live for the call.
impl WebView for PlatformWebView {
    fn load_uri(&self, uri: &str) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_load_uri)(self.h, Str::of(uri)) }
    }

    fn load_bytes(&self, content: &[u8], mime_type: &str, encoding: &str, base_uri: &str) {
        // SAFETY: see the impl comment.
        unsafe {
            (self.m.view_load_bytes)(
                self.h,
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
        unsafe { (self.m.view_stop)(self.h) }
    }

    fn reload(&self) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_reload)(self.h) }
    }

    fn evaluate_javascript(&self, script: &str, done: Box<dyn FnOnce(Result<String, String>)>) {
        let mut done = Some(done);
        let cb = Callback::new(move |r: &EvalResultView| {
            // SAFETY: the result's text is alive for this call.
            let text = unsafe { r.text.get() }.into_owned();
            if let Some(d) = done.take() {
                d(if r.ok { Ok(text) } else { Err(text) });
            }
        });
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_evaluate)(self.h, Str::of(script), cb) }
    }

    fn set_user_scripts(&self, sources: &[String]) {
        let views: Vec<Str> = sources.iter().map(|s| Str::of(s)).collect();
        // SAFETY: see the impl comment; `views` borrows `sources` for the call.
        unsafe { (self.m.view_set_user_scripts)(self.h, views.as_ptr(), views.len()) }
    }

    fn uri(&self) -> Option<String> {
        self.text(self.m.view_uri)
    }

    fn title(&self) -> Option<String> {
        self.text(self.m.view_title)
    }

    fn progress(&self) -> i32 {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_progress)(self.h) }
    }

    fn set_javascript_enabled(&self, on: bool) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_set_bool)(self.h, BoolSetting::JavaScript, on) }
    }

    fn javascript_enabled(&self) -> bool {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_get_bool)(self.h, BoolSetting::JavaScript) }
    }

    fn set_user_agent(&self, ua: &str) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_set_user_agent)(self.h, Str::of(ua)) }
    }

    fn user_agent(&self) -> String {
        // SAFETY: see the impl comment.
        let ((), bytes) = BytesOut::collect(|out| unsafe { (self.m.view_user_agent)(self.h, out) });
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn set_dom_storage_enabled(&self, on: bool) {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_set_bool)(self.h, BoolSetting::DomStorage, on) }
    }

    fn dom_storage_enabled(&self) -> bool {
        // SAFETY: see the impl comment.
        unsafe { (self.m.view_get_bool)(self.h, BoolSetting::DomStorage) }
    }

    fn sideband_stream(&self) -> [i32; 2] {
        [SIDEBAND_WEB_VIEW, self.key]
    }
}

impl Drop for PlatformWebView {
    fn drop(&mut self) {
        // SAFETY: UI thread; the window lets go of the widget before the view (its owner) is destroyed. Then the handle `view_create` made, dropped once.
        unsafe {
            (self.ui.sideband_detach)(self.key);
            (self.m.view_drop)(self.h)
        }
    }
}
