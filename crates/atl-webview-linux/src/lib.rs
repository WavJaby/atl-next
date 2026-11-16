// SPDX-License-Identifier: MIT
//! The WebView module's exports (`atl_platform_ui_api::abi::WebViewVtable`): WebKitGTK behind the facade's `PlatformWebEngine`.
//! Default UA: `webkit_settings_get_user_agent` of fresh settings -- the engine's own default, made once on the GTK main thread and cached. WebKit binds its main thread to the first thread that
//! touches it: settings made on an app worker (AdMob's AdWorker asks the UA first) made every later WebView on the UI thread SIGSEGV/abort in instance init (AB2, Temple Run; test app row
//! webview-ua runs on a worker for this). So WebKit is touched on the UI thread only; a worker's call hops there and waits.
//! Views: a `WebKitWebView` (W0: loads and runs JS unrealized, before any window parents it); shown by the UI module, which parents it into the window whose scene has its sideband quad (`view_widget`). Every `view_*` runs on the GTK main thread (the Android UI thread); signals arrive there.

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use atl_platform_ui_api::abi::{
    self, BytesOut, Callback, CookieList, CookieView, EvalResultView, PromptReplyAbi, SendCallback,
    Status, Str, WebEventView, WebViewHandle, WebViewVtable, web_event,
};
use atl_platform_ui_api::webview::{Cookie, CookiePolicy, LoadError, WebSetting};
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
    view_set_int,
    view_set_str,
    view_set_background,
    view_widget,
    view_set_initial_scale,
    cookies_watch,
    cookies_add,
    cookies_replace,
    cookies_policy,
};

/// [`abi::WebViewEntry`]: the static vtable; starts nothing.
#[unsafe(no_mangle)]
pub extern "C" fn atl_webview_entry() -> *const WebViewVtable {
    &VTABLE
}

const NO_DISPLAY: &[u8] = b"no display: the process has no GTK display (no-UI mode)";

unsafe extern "C" fn default_user_agent(out: *mut BytesOut) -> bool {
    let (ok, bytes) = match user_agent() {
        Ok(ua) => (true, ua.into_bytes()),
        Err(why) => (false, why.to_vec()),
    };
    // SAFETY: `out` is the sink the caller lent for this call.
    unsafe { BytesOut::put_to(out, &bytes) };
    ok
}

/// The engine's default UA, made once on the UI thread ([`ui_user_agent`]).
static USER_AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// [`USER_AGENT`] from any thread. The UI thread owns GLib's default context for the process's life (the UI module's mirror acquires it at attach), so: owner -> run here; a context nobody
/// owns -> no UI thread in this process, refuse (invoking would run WebKit right here, on the caller); else invoke on the UI thread and wait.
/// Deadlock: a worker waits here while the UI thread runs its Looper; if the app blocks its UI thread on this worker (join, Future.get) both wait -- as Chromium's provider, whose statics start
/// the engine on the UI thread and block a background caller until it ran. No fallback: answering on the worker is exactly the off-thread WebKit touch this avoids.
fn user_agent() -> Result<String, &'static [u8]> {
    if let Some(ua) = USER_AGENT.get() {
        return Ok(ua.clone());
    }
    let ctx = glib::MainContext::default();
    if ctx.is_owner() {
        return ui_user_agent();
    }
    if ctx.acquire().is_ok() {
        return Err(NO_DISPLAY);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    ctx.invoke(move || {
        let _ = tx.send(ui_user_agent());
    });
    rx.recv().unwrap_or(Err(
        b"web engine: the UI thread dropped the user agent call",
    ))
}

/// On the UI thread only. WebKitSettings' constructor reads the default GDK display's GL support and dereferences a missing display (SIGSEGV, measured in the no-UI app run): no display -> refusal.
fn ui_user_agent() -> Result<String, &'static [u8]> {
    if webkit6::gdk::Display::default().is_none() {
        return Err(NO_DISPLAY);
    }
    Ok(USER_AGENT
        .get_or_init(|| {
            webkit6::Settings::new()
                .user_agent()
                .unwrap_or_default()
                .to_string()
        })
        .clone())
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
    extra: String,
    length: i64,
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
        self.push(Pending {
            kind,
            text: text.to_owned(),
            detail: detail.to_owned(),
            error,
            progress,
            reply,
            extra: String::new(),
            length: 0,
        });
    }

    fn push(&self, p: Pending) {
        self.queue.borrow_mut().push_back(p);
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
                extra: Str::of(&p.extra),
                length: p.length,
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
    web.connect_decide_policy(move |_, decision, kind| {
        if kind != webkit6::PolicyDecisionType::Response {
            return false;
        }
        let Some(d) = decision.downcast_ref::<webkit6::ResponsePolicyDecision>() else {
            return false;
        };
        let Some(r) = d.response() else {
            return false;
        };
        let disposition = r
            .http_headers()
            .and_then(|h| h.one("Content-Disposition"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        // what Chromium hands to DownloadListener instead of showing: a type it cannot render, or a response that asks to be saved (RFC 6266 disposition type, case-insensitive)
        let attachment = disposition
            .split(';')
            .next()
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("attachment"));
        if d.is_mime_type_supported() && !attachment {
            return false;
        }
        // the navigation is dropped, not saved: the app's DownloadListener decides what happens (Chromium's WebView never downloads by itself)
        decision.ignore();
        let length = r.content_length();
        ev.push(Pending {
            kind: web_event::DOWNLOAD,
            text: r.uri().map(|u| u.to_string()).unwrap_or_default(),
            detail: disposition,
            error: 0,
            progress: 0,
            reply: None,
            extra: r.mime_type().map(|m| m.to_string()).unwrap_or_default(),
            // WebKit reports 0 for an unknown length (no Content-Length) -> -1, the API's "unknown"
            length: if length == 0 { -1 } else { length as i64 },
        });
        true
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

// The core's setting table (atl-webview settings.rs) decides each value's kind; a setting of another kind here is a broken table row and only logged.
unsafe extern "C" fn view_set_bool(h: *mut WebViewHandle, which: WebSetting, on: bool) {
    let s = settings(h);
    match which {
        WebSetting::JavaScript => s.set_enable_javascript(on),
        WebSetting::DomStorage => s.set_enable_html5_local_storage(on),
        WebSetting::MediaPlaybackRequiresUserGesture => {
            s.set_media_playback_requires_user_gesture(on)
        }
        WebSetting::AllowUniversalAccessFromFileUrls => {
            s.set_allow_universal_access_from_file_urls(on)
        }
        WebSetting::AllowFileAccessFromFileUrls => s.set_allow_file_access_from_file_urls(on),
        WebSetting::JavaScriptCanOpenWindowsAutomatically => {
            s.set_javascript_can_open_windows_automatically(on)
        }
        WebSetting::AutoLoadImages => s.set_auto_load_images(on),
        other => tracing::warn!("webview: {other:?} is not a boolean setting"),
    }
}

unsafe extern "C" fn view_set_int(h: *mut WebViewHandle, which: WebSetting, value: i32) {
    let s = settings(h);
    // WebKit's font sizes are unsigned; a negative one is no size
    let size = u32::try_from(value).unwrap_or(0);
    match which {
        WebSetting::MinimumFontSize => s.set_minimum_font_size(size),
        WebSetting::DefaultFontSize => s.set_default_font_size(size),
        WebSetting::DefaultMonospaceFontSize => s.set_default_monospace_font_size(size),
        other => tracing::warn!("webview: {other:?} is not an int setting"),
    }
}

unsafe extern "C" fn view_set_str(h: *mut WebViewHandle, which: WebSetting, value: Str) {
    // SAFETY: `value` is alive for this call.
    let text = unsafe { value.get() };
    let text: &str = &text;
    let s = settings(h);
    match which {
        // empty = the engine's own default (WebKit: a NULL user agent)
        WebSetting::UserAgent => s.set_user_agent((!text.is_empty()).then_some(text)),
        WebSetting::DefaultFontFamily => s.set_default_font_family(text),
        WebSetting::MonospaceFontFamily => s.set_monospace_font_family(text),
        WebSetting::SansSerifFontFamily => s.set_sans_serif_font_family(text),
        WebSetting::SerifFontFamily => s.set_serif_font_family(text),
        WebSetting::CursiveFontFamily => s.set_cursive_font_family(text),
        WebSetting::FantasyFontFamily => s.set_fantasy_font_family(text),
        WebSetting::DefaultCharset => s.set_default_charset(text),
        other => tracing::warn!("webview: {other:?} is not a text setting"),
    }
}

unsafe extern "C" fn view_set_background(h: *mut WebViewHandle, argb: u32) {
    // Android's colour int (alpha in the top byte); GdkRGBA takes each channel as 0..=1
    let channel = |shift: u32| f32::from(((argb >> shift) & 0xff) as u8) / 255.0;
    let rgba = webkit6::gdk::RGBA::new(channel(16), channel(8), channel(0), channel(24));
    view(h).web.set_background_color(&rgba);
}

unsafe extern "C" fn view_set_initial_scale(h: *mut WebViewHandle, percent: i32) {
    let web = &view(h).web;
    // WebKit's zoom multiplies the toolkit's device scale; setInitialScale counts device pixels ("doesn't take the screen density into account", WebView.java:1186) -> divide it out
    let zoom = if percent > 0 {
        f64::from(percent) / 100.0 / f64::from(web.scale_factor().max(1))
    } else {
        1.0
    };
    web.set_zoom_level(zoom);
}

/// Runs `f` on the UI thread (the owner of GLib's default context, as [`user_agent`]): now when this is it, else queued; false = no UI thread (nobody owns the context), `f` dropped.
fn on_ui(f: impl FnOnce() + Send + 'static) -> bool {
    let ctx = glib::MainContext::default();
    if ctx.is_owner() {
        f();
        return true;
    }
    if ctx.acquire().is_ok() {
        return false;
    }
    ctx.invoke(f);
    true
}

/// The default network session's cookie store (every view uses that session: `WebView::new`). UI thread.
fn cookie_manager() -> Option<webkit6::CookieManager> {
    webkit6::NetworkSession::default()?.cookie_manager()
}

fn soup_cookie(c: &Cookie) -> webkit6::soup::Cookie {
    // max_age -1 = a session cookie; an expiry is set from the absolute time below
    let mut s = webkit6::soup::Cookie::new(&c.name, &c.value, &c.domain, &c.path, -1);
    if let Some(t) = c.expires
        && let Ok(d) = glib::DateTime::from_unix_utc(t)
    {
        s.set_expires(&d);
    }
    s.set_secure(c.secure);
    s.set_http_only(c.http_only);
    s
}

fn from_soup(mut s: webkit6::soup::Cookie) -> Cookie {
    let text = |g: Option<glib::GString>| g.map(|g| g.to_string()).unwrap_or_default();
    Cookie {
        name: text(s.name()),
        value: text(s.value()),
        domain: text(s.domain()),
        path: text(s.path()),
        expires: s.expires().map(|d| d.to_unix()),
        secure: s.is_secure(),
        http_only: s.is_http_only(),
    }
}

/// The one watcher ([`cookies_watch`]); a second watch replaces it, the `changed` handler is connected once.
static WATCHER: std::sync::Mutex<Option<SendCallback<CookieList>>> = std::sync::Mutex::new(None);

/// Fetches the whole store and hands it to the watcher. UI thread.
fn deliver_cookies(cm: &webkit6::CookieManager) {
    cm.all_cookies(None::<&webkit6::gio::Cancellable>, |r| {
        let list: Vec<Cookie> = match r {
            Ok(v) => v.into_iter().map(from_soup).collect(),
            Err(e) => {
                tracing::warn!("webview: cookie fetch: {e}");
                return;
            }
        };
        let views: Vec<CookieView> = list.iter().map(CookieView::of).collect();
        if let Some(w) = WATCHER.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            w.call(&CookieList {
                ptr: views.as_ptr(),
                len: views.len(),
            });
        }
    });
}

unsafe extern "C" fn cookies_watch(changed: SendCallback<CookieList>) -> bool {
    on_ui(move || {
        let Some(cm) = cookie_manager() else {
            tracing::warn!("webview: no cookie manager");
            return;
        };
        let first = WATCHER
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replace(changed)
            .is_none();
        if first {
            cm.connect_changed(deliver_cookies);
        }
        deliver_cookies(&cm);
    })
}

fn finish(mut done: SendCallback<bool>, ok: bool) {
    done.call(&ok);
}

unsafe extern "C" fn cookies_add(cookie: *const CookieView, done: SendCallback<bool>) -> bool {
    // SAFETY: the caller lends the cookie and its strings for this call; copied here.
    let c = unsafe { (*cookie).get() };
    on_ui(move || {
        let Some(cm) = cookie_manager() else {
            return finish(done, false);
        };
        cm.add_cookie(
            &soup_cookie(&c),
            None::<&webkit6::gio::Cancellable>,
            move |r| finish(done, r.is_ok()),
        );
    })
}

unsafe extern "C" fn cookies_replace(cookies: CookieList, done: SendCallback<bool>) -> bool {
    // SAFETY: as cookies_add, for every cookie of the list.
    let want: Vec<Cookie> = unsafe { std::slice::from_raw_parts(cookies.ptr, cookies.len) }
        .iter()
        // SAFETY: as above.
        .map(|v| unsafe { v.get() })
        .collect();
    on_ui(move || {
        let Some(cm) = cookie_manager() else {
            return finish(done, false);
        };
        // WebKitGTK 2.42-2.54 has no replace call in the bindings: delete what is there, then add the list, in order on this thread
        glib::MainContext::default().spawn_local(async move {
            let mut ok = true;
            match cm.all_cookies_future().await {
                Ok(have) => {
                    for c in have {
                        ok &= cm.delete_cookie_future(&c).await.is_ok();
                    }
                }
                Err(_) => ok = false,
            }
            for c in &want {
                ok &= cm.add_cookie_future(&soup_cookie(c)).await.is_ok();
            }
            finish(done, ok);
        });
    })
}

unsafe extern "C" fn cookies_policy(policy: CookiePolicy) -> bool {
    on_ui(move || {
        let Some(cm) = cookie_manager() else { return };
        cm.set_accept_policy(match policy {
            CookiePolicy::Always => webkit6::CookieAcceptPolicy::Always,
            CookiePolicy::Never => webkit6::CookieAcceptPolicy::Never,
            CookiePolicy::NoThirdParty => webkit6::CookieAcceptPolicy::NoThirdParty,
        });
    })
}

unsafe extern "C" fn view_widget(h: *mut WebViewHandle) -> *mut std::ffi::c_void {
    use webkit6::glib::translate::ToGlibPtr;
    let w: *mut webkit6::gtk::ffi::GtkWidget = view(h)
        .web
        .upcast_ref::<webkit6::gtk::Widget>()
        .to_glib_none()
        .0;
    w.cast()
}
