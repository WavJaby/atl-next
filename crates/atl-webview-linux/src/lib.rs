// SPDX-License-Identifier: MIT
//! The WebView module's exports (`atl_platform_ui_api::abi::WebViewVtable`): WebKitGTK behind the facade's `PlatformWebEngine`.
//! The display thread: the thread that owns GLib's default context and runs GTK (today the Android UI thread, which the UI module's mirror makes the context's owner; display-thread plan D2: its own
//! thread later). WebKit binds its main thread to the first thread that touches it: settings made on an app worker (AdMob's AdWorker asks the UA first) made every later WebView SIGSEGV/abort in
//! instance init (AB2, Temple Run; test app row webview-ua runs on a worker for this). So WebKit is touched on the display thread only, found as the default context's owner; another thread's call hops there.
//! Default UA: `webkit_settings_get_user_agent` of fresh settings -- the engine's own default, made once on the display thread and cached.
//! Views: a `WebKitWebView` (W0: loads and runs JS unrealized, before any window parents it); shown by the UI module, which parents it into the window whose scene has its sideband quad (`view_widget`). Every `view_*` runs on the display thread; signals arrive there.

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use atl_platform_ui_api::abi::{
    self, BytesOut, Callback, CookieList, CookieView, EvalResultView, ReplyAbi, SendCallback,
    Status, Str, WebEventView, WebViewHandle, WebViewVtable, web_event,
};
use atl_platform_ui_api::webview::{Cookie, CookiePolicy, LoadError, WebSetting};
use webkit6::glib;
use webkit6::prelude::*;

static VTABLE: WebViewVtable = WebViewVtable {
    abi: abi::ABI_ID,
    set_log: abi::set_log,
    default_user_agent,
    view_create,
    view_drop,
    view_load_uri,
    view_load_bytes,
    view_stop,
    view_reload,
    view_go,
    view_evaluate,
    view_set_user_scripts,
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

/// The first view's engine line, then `<major>.<minor>.<micro>` (the window gate's webview `module-log` sub-gate reads this text).
const ENGINE_LINE_PREFIX: &str = "webview: engine WebKitGTK ";

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

/// The engine's default UA, made once on the display thread ([`display_user_agent`]).
static USER_AGENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// [`USER_AGENT`] from any thread: the display plan's `call` form (an app thread waits on the display thread, which never waits on an app thread). The display thread owns GLib's default context for
/// the process's life, so: owner -> run here; a context nobody owns -> no display thread in this process, refuse (invoking would run WebKit right here, on the caller); else invoke there and wait.
/// While the display thread is the UI thread (until display-thread S5): if the app blocks its UI thread on this worker (join, Future.get) both wait, as Chromium's provider blocks a background
/// caller until its UI thread started the engine. No fallback: answering on the worker is exactly the off-thread WebKit touch this avoids.
fn user_agent() -> Result<String, &'static [u8]> {
    if let Some(ua) = USER_AGENT.get() {
        return Ok(ua.clone());
    }
    let ctx = glib::MainContext::default();
    if ctx.is_owner() {
        return display_user_agent();
    }
    if ctx.acquire().is_ok() {
        return Err(NO_DISPLAY);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    ctx.invoke(move || {
        let _ = tx.send(display_user_agent());
    });
    rx.recv().unwrap_or(Err(
        b"web engine: the display thread dropped the user agent call",
    ))
}

/// On the display thread only. WebKitSettings' constructor reads the default GDK display's GL support and dereferences a missing display (SIGSEGV, measured in the no-UI app run): no display -> refusal.
fn display_user_agent() -> Result<String, &'static [u8]> {
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
    /// The sink, dropped with the view (the signal closures' clones drop with the WebKit object).
    events: Rc<Events>,
}

impl View {
    /// The provider asked for the next navigation (load_uri, load_bytes): its policy decision is not reported (WebViewClient.shouldOverrideUrlLoading javadoc: "not called for navigations
    /// which the app initiated with loadUrl()"). Cleared by that decision or by the next main-frame load start, so a load that makes no decision cannot hide a later page navigation.
    fn provider_load(&self) {
        self.events.provider_load.set(true);
    }
}

fn view<'a>(h: *mut WebViewHandle) -> &'a View {
    // SAFETY: every handle is a Box<View> `view_create` leaked, alive until `view_drop`; the caller's contract (display thread, live handle).
    unsafe { &*h.cast::<View>() }
}

/// One event not yet delivered.
#[derive(Default)]
struct Pending {
    kind: u32,
    text: String,
    detail: String,
    error: u32,
    progress: i32,
    reply: Option<ReplyAbi>,
    extra: String,
    length: i64,
    flags: u32,
    back: u32,
    forward: u32,
}

/// What a LOCATION event reports: URL, original URL, entries back, entries forward.
type Location = (Option<String>, Option<String>, u32, u32);

/// `w`'s location now. Original URL: the current back-forward item's (WebKitBackForwardListItem: the URI first requested, before redirects); a page without an item (load_bytes makes none) -> its
/// URI, which nothing redirected.
fn location(w: &webkit6::WebView) -> Location {
    let uri = w.uri().map(|u| u.to_string());
    let list = w.back_forward_list();
    let original = list
        .as_ref()
        .and_then(|l| l.current_item())
        .and_then(|i| i.original_uri())
        .map(|u| u.to_string())
        .or_else(|| uri.clone());
    let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    let (back, forward) = list.map_or((0, 0), |l| {
        (count(l.back_list().len()), count(l.forward_list().len()))
    });
    (uri, original, back, forward)
}

/// The sink plus a queue: a callback may call back into the view (loadUrl from onPageFinished) and WebKit may signal synchronously inside it; such a nested event waits until the running one returns, so order is kept and nothing is dropped.
struct Events {
    sink: RefCell<Callback<WebEventView>>,
    queue: RefCell<std::collections::VecDeque<Pending>>,
    busy: std::cell::Cell<bool>,
    /// The title last reported for the current load.
    title: RefCell<Option<String>>,
    /// [`View::provider_load`].
    provider_load: std::cell::Cell<bool>,
    /// The main frame is loading (load-changed Started .. Finished): a navigation decision for an engine-loaded scheme, of type "other", without gesture or redirect, in this window is taken for a subframe's first load.
    main_loading: std::cell::Cell<bool>,
    /// The URL of the main-frame navigation last let through, reported by the next load start: after a cross-site navigation (WebKit swaps the web process) the view's URI still names the old
    /// page at load-changed Started (measured: test app row webview-http). Taken by the next load start, cleared by every decision.
    navigating: RefCell<Option<String>>,
    /// The current main-frame load's URL for its LOAD_FINISHED: the started one, then the view's from redirect / commit on; a load that failed before commit keeps the URL it was started for.
    load_uri: RefCell<String>,
    /// The location last reported ([`Self::location_changed`]).
    location: RefCell<Option<Location>>,
}

impl Events {
    /// Reports `w`'s location if it differs from the last one reported. Called on every change WebKit notifies (uri, back-forward list) and before each load event, so the receiver's copy is current
    /// when that event's callback runs.
    fn location_changed(&self, w: &webkit6::WebView) {
        let now = location(w);
        if self.location.borrow().as_ref() == Some(&now) {
            return;
        }
        self.location.replace(Some(now.clone()));
        let (uri, original, back, forward) = now;
        let mut flags = 0;
        if uri.is_some() {
            flags |= web_event::FLAG_HAS_URI;
        }
        if original.is_some() {
            flags |= web_event::FLAG_HAS_ORIGINAL_URI;
        }
        self.push(Pending {
            kind: web_event::LOCATION,
            text: uri.unwrap_or_default(),
            detail: original.unwrap_or_default(),
            flags,
            back,
            forward,
            ..Pending::default()
        });
    }

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
        reply: Option<ReplyAbi>,
    ) {
        self.push(Pending {
            kind,
            text: text.to_owned(),
            detail: detail.to_owned(),
            error,
            progress,
            reply,
            ..Pending::default()
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
                flags: p.flags,
                back: p.back,
                forward: p.forward,
                // the receiver takes `reply` only for PROMPT and NAVIGATION; any other event carries a placeholder this side drops
                reply: ManuallyDrop::new(p.reply.unwrap_or_else(|| ReplyAbi::new(|_| {}))),
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
    } else if e.matches(NetworkError::UnknownProtocol) {
        LoadError::UnsupportedScheme
    } else if e.domain() == NetworkError::domain() {
        LoadError::Network
    } else {
        LoadError::Other
    }
}

/// The schemes WebKitGTK loads in a frame with no scheme registered by the embedder (we register none): its network loaders (http, https, file, data, blob) and about:. Anything else
/// (`ponos-api:`, `intent:`, `market:`, `tel:`) fails as an unknown protocol after a load started, leaving an error page.
const ENGINE_SCHEMES: [&str; 6] = ["http", "https", "file", "data", "blob", "about"];

/// `uri`'s scheme, lower case; None without one.
fn scheme(uri: &str) -> Option<String> {
    let (s, _) = uri.split_once(':')?;
    let ok = s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    ok.then(|| s.to_ascii_lowercase())
}

/// A navigation-action decision: held (WebKit waits on a decision whose ref is held and neither `use` nor `ignore` was called) and reported as NAVIGATION, unless it is not the app's to decide
/// ([`WebEvent::Navigation`](atl_platform_ui_api::webview::WebEvent::Navigation) lists them) -> WebKit's default. The reply ignores it (taken), or uses it; a scheme no engine loader takes is
/// ignored and reported as a LOAD_FAILED of kind UnsupportedScheme instead, so the page stays (Chromium shows an error for it; WebKit would start a load and show its error page).
fn navigation_policy(ev: &Rc<Events>, decision: &webkit6::PolicyDecision) -> bool {
    let Some(d) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() else {
        return false;
    };
    let Some(action) = d.navigation_action() else {
        return false;
    };
    let Some(request) = action.request() else {
        return false;
    };
    let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
    let method = request
        .http_method()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "GET".into());
    let (redirect, gesture, kind) = (
        action.is_redirect(),
        action.is_user_gesture(),
        action.navigation_type(),
    );
    ev.navigating.take();
    if !redirect && ev.provider_load.replace(false) {
        ev.navigating.replace(Some(uri));
        return false;
    }
    let scheme = scheme(&uri).unwrap_or_default();
    let engine_loads = ENGINE_SCHEMES.contains(&scheme.as_str());
    // an engine-loaded document (http(s), and the about:blank / data: iframes ad pages make: Battle Cats TW's AdMob client cancelled two `about:` ones, 2026-11-18) while the page loads;
    // a custom scheme is always the app's to decide (WebViewClient javadoc: "may be called for subframes and with non-HTTP(S) schemes")
    let subframe_load = engine_loads
        && !gesture
        && !redirect
        && kind == webkit6::NavigationType::Other
        && ev.main_loading.get();
    // history and reload: the provider's own (goBack, reload) or the page's history.back(), not a new URL; POST: WebViewClient javadoc "not called for POST requests"
    if matches!(
        kind,
        webkit6::NavigationType::BackForward
            | webkit6::NavigationType::Reload
            | webkit6::NavigationType::FormResubmitted
    ) || method.eq_ignore_ascii_case("POST")
        || subframe_load
    {
        return false;
    }
    let held = glib::thread_guard::ThreadGuard::new((decision.clone(), Rc::downgrade(ev)));
    let nav_uri = uri.clone();
    let reply = ReplyAbi::new(move |taken: Option<String>| {
        glib::MainContext::default().invoke(move || {
            let (d, ev) = held.into_inner();
            if taken.is_some() {
                d.ignore();
            } else if engine_loads {
                if let Some(ev) = ev.upgrade() {
                    ev.navigating.replace(Some(nav_uri));
                }
                d.use_();
            } else {
                d.ignore();
                if let Some(ev) = ev.upgrade() {
                    // Chromium's net error text for the case (net_error_list.h UNKNOWN_URL_SCHEME), which WebResourceError.getDescription shows
                    ev.emit(
                        web_event::LOAD_FAILED,
                        &nav_uri,
                        "net::ERR_UNKNOWN_URL_SCHEME",
                        LoadError::UnsupportedScheme as u32,
                        0,
                        None,
                    );
                }
            }
        });
    });
    let mut flags = web_event::FLAG_MAIN_FRAME;
    if redirect {
        flags |= web_event::FLAG_REDIRECT;
    }
    if gesture {
        flags |= web_event::FLAG_GESTURE;
    }
    ev.push(Pending {
        kind: web_event::NAVIGATION,
        text: uri,
        detail: method,
        reply: Some(reply),
        flags,
        ..Pending::default()
    });
    true
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
    static ENGINE_LINE: std::sync::Once = std::sync::Once::new();
    // once per process: the engine version, and proof this module's lines reach the app log (abi::set_log; window gate webview `module-log`)
    ENGINE_LINE.call_once(|| {
        tracing::info!(
            "{ENGINE_LINE_PREFIX}{}.{}.{}",
            webkit6::functions::major_version(),
            webkit6::functions::minor_version(),
            webkit6::functions::micro_version()
        );
    });
    // autoplay is the app's setting alone (WebSettings.setMediaPlaybackRequiresUserGesture -> media-playback-requires-user-gesture): WebKit's own policy, ALLOW_WITHOUT_SOUND by default,
    // would refuse an audible play() the app allowed (NotAllowedError), which Android's WebView plays. Construct-only on the view, so set here once.
    let web = webkit6::WebView::builder()
        .website_policies(
            &webkit6::WebsitePolicies::builder()
                .autoplay(webkit6::AutoplayPolicy::Allow)
                .build(),
        )
        .build();
    if let Some(s) = WebViewExt::settings(&web) {
        s.set_enable_javascript(false);
        s.set_enable_html5_local_storage(false);
    }
    let events = Rc::new(Events {
        sink: RefCell::new(events),
        queue: RefCell::default(),
        busy: std::cell::Cell::new(false),
        title: RefCell::default(),
        provider_load: std::cell::Cell::new(false),
        main_loading: std::cell::Cell::new(false),
        navigating: RefCell::default(),
        load_uri: RefCell::default(),
        location: RefCell::default(),
    });
    let ev = events.clone();
    web.connect_uri_notify(move |w| ev.location_changed(w));
    if let Some(list) = web.back_forward_list() {
        let ev = events.clone();
        let weak = web.downgrade();
        // the bindings carry no typed `changed` (its removed-items argument is a GList): connected by name, arguments unread
        list.connect_local("changed", false, move |_| {
            if let Some(w) = weak.upgrade() {
                ev.location_changed(&w);
            }
            None
        });
    }
    let ev = events.clone();
    web.connect_load_changed(move |w, e| {
        ev.location_changed(w);
        let uri = w.uri().map(|u| u.to_string()).unwrap_or_default();
        match e {
            webkit6::LoadEvent::Started => {
                let uri = ev.navigating.take().unwrap_or(uri);
                // a new load reports its title again, even an equal one
                ev.title.replace(None);
                ev.provider_load.set(false);
                ev.main_loading.set(true);
                ev.load_uri.replace(uri.clone());
                ev.emit(web_event::LOAD_STARTED, &uri, "", 0, 0, None);
            }
            // from here the view's URI is the load's
            webkit6::LoadEvent::Redirected | webkit6::LoadEvent::Committed => {
                ev.load_uri.replace(uri);
            }
            webkit6::LoadEvent::Finished => {
                ev.main_loading.set(false);
                let uri = ev.load_uri.take();
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
        if kind == webkit6::PolicyDecisionType::NavigationAction {
            return navigation_policy(&ev, decision);
        }
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
            extra: r.mime_type().map(|m| m.to_string()).unwrap_or_default(),
            // WebKit reports 0 for an unknown length (no Content-Length) -> -1, the API's "unknown"
            length: if length == 0 { -1 } else { length as i64 },
            ..Pending::default()
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
        let reply = ReplyAbi::new(move |text: Option<String>| {
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
    unsafe { *out = Box::into_raw(Box::new(View { web, events })).cast() };
    abi::OK
}

unsafe extern "C" fn view_drop(h: *mut WebViewHandle) {
    // SAFETY: the Box `view_create` leaked; dropped once.
    let v = unsafe { Box::from_raw(h.cast::<View>()) };
    v.web.try_close();
    drop(v);
}

unsafe extern "C" fn view_load_uri(h: *mut WebViewHandle, uri: Str) {
    view(h).provider_load();
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
    view(h).provider_load();
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

/// The back-forward list's item `steps` from the current one (WebKitBackForwardList.get_nth_item: 0 current, negative back), as Chromium's NavigationController offsets count.
fn history_item(h: *mut WebViewHandle, steps: i32) -> Option<webkit6::BackForwardListItem> {
    if steps == 0 {
        return None;
    }
    view(h).web.back_forward_list()?.nth_item(steps)
}

unsafe extern "C" fn view_go(h: *mut WebViewHandle, steps: i32) {
    if let Some(item) = history_item(h, steps) {
        view(h).web.go_to_back_forward_list_item(&item);
    }
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

/// Runs `f` on the display thread (the owner of GLib's default context, as [`user_agent`]): now when this is it, else queued; false = no display thread (nobody owns the context), `f` dropped.
fn on_display(f: impl FnOnce() + Send + 'static) -> bool {
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

/// The default network session's cookie store (every view uses that session: `WebView::new`). Display thread.
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

/// Fetches the whole store and hands it to the watcher. Display thread.
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
    on_display(move || {
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
    on_display(move || {
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
    on_display(move || {
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
    on_display(move || {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 3986 3.1: scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ), case-insensitive.
    #[test]
    fn scheme_by_rfc3986() {
        assert_eq!(
            scheme("ponos-api://function?x=1").as_deref(),
            Some("ponos-api")
        );
        assert_eq!(scheme("HTTPS://a.example/").as_deref(), Some("https"));
        assert_eq!(scheme("intent:#Intent;end").as_deref(), Some("intent"));
        assert_eq!(scheme("1abc:x"), None);
        assert_eq!(scheme("a b:x"), None);
        assert_eq!(scheme("no-colon"), None);
    }
}
