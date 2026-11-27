// SPDX-License-Identifier: MIT
//! The WebView module's exports (`atl_platform_ui_api::abi::WebViewVtable`): WebKitGTK behind the facade's `PlatformWebEngine`.
//! The display thread: the thread that owns GLib's default context and runs GTK, which the UI module starts at attach before ART boots (`atl-display`, display-thread plan D2). WebKit binds its main thread to the first thread that touches it: settings made on an app worker (AdMob's AdWorker asks the UA first) made every later WebView SIGSEGV/abort in
//! instance init (AB2, Temple Run; test app row webview-ua runs on a worker for this). So WebKit is touched on the display thread only, found as the default context's owner; another thread's call hops there.
//! Default UA: `webkit_settings_get_user_agent` of fresh settings -- the engine's own default, made once on the display thread and cached.
//! Views: a `WebKitWebView` (W0: loads and runs JS unrealized, before any window parents it), named by the process's sideband key; shown by the UI module, which parents it into the window whose
//! scene has its sideband quad (`view_create`'s `shown`). Every `view_*` may come from any thread and runs on the display thread ([`on_display`], in call order), where the views live ([`VIEWS`])
//! and signals arrive; events go to the process's sink there, which only hands them on (display-thread plan D5: posted to the WebView's Looper), so no app code runs inside a signal.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use atl_platform_ui_api::abi::{
    self, BytesOut, CookieList, CookieView, EvalResultView, ReplyAbi, SendCallback, Str,
    WebEventView, WebViewVtable, web_event,
};
use atl_platform_ui_api::webview::{
    Cookie, CookiePolicy, LoadError, NavigationReply, PromptReply, WebSetting,
};
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
    view_set_initial_scale,
    view_cancel_popup,
    view_adopt_popup,
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
/// The wait is bounded by the display thread's own work: a UI thread blocked on this worker (join, Future.get) does not stall it. No fallback: answering on the worker is exactly the off-thread WebKit touch this avoids.
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

/// A view, its event sink (shared with the signal closures) and its `shown` report.
struct View {
    web: webkit6::WebView,
    /// The sink, dropped with the view (the signal closures' clones drop with the WebKit object).
    events: Rc<Events>,
    /// `view_create`'s `shown`: the widget now, null at `view_drop`.
    shown: SendCallback<*mut c_void>,
    /// This view's references in [`SCRIPTS`] (`view_set_user_scripts`' last sources), released at retirement.
    scripts: Vec<String>,
}

/// The one script controller every view is built on, shared as a popup's related view must share its opener's. A source is in it while any view holds it (count), so no view's
/// update removes another view's script; the core's sources are view-agnostic (atl-webview bridge bootstrap), each document asking its own view.
struct Scripts {
    ucm: webkit6::UserContentManager,
    held: HashMap<String, (webkit6::UserScript, u32)>,
}

impl Scripts {
    fn hold(&mut self, src: &str) {
        let ucm = &self.ucm;
        self.held
            .entry(src.to_owned())
            .or_insert_with(|| {
                let script = webkit6::UserScript::new(
                    src,
                    webkit6::UserContentInjectedFrames::AllFrames,
                    webkit6::UserScriptInjectionTime::Start,
                    &[],
                    &[],
                );
                ucm.add_script(&script);
                (script, 0)
            })
            .1 += 1;
    }

    fn release(&mut self, src: &str) {
        let Some((script, n)) = self.held.get_mut(src) else {
            return tracing::warn!("webview: release of a script no view holds; ignored");
        };
        *n -= 1;
        if *n == 0 {
            self.ucm.remove_script(script);
            self.held.remove(src);
        }
    }
}

thread_local! {
    /// The live views by key; filled and read on the display thread only.
    static VIEWS: RefCell<HashMap<i32, View>> = RefCell::default();
    /// Made with the first view (after GTK init), on the display thread only.
    static SCRIPTS: RefCell<Option<Scripts>> = const { RefCell::new(None) };
}

/// Runs `f` on [`SCRIPTS`], made on first use.
fn with_scripts<R>(f: impl FnOnce(&mut Scripts) -> R) -> R {
    SCRIPTS.with_borrow_mut(|s| {
        f(s.get_or_insert_with(|| Scripts {
            ucm: webkit6::UserContentManager::new(),
            held: HashMap::new(),
        }))
    })
}

/// Runs `f` with `key`'s view on the display thread ([`on_display`]); a key with no view there (creation failed, already dropped) is logged and `f` dropped. The view is cloned out of
/// [`VIEWS`] first, so `f` may run a nested main loop that drops a view.
fn on_view(
    key: i32,
    what: &'static str,
    f: impl FnOnce(&webkit6::WebView, &Rc<Events>) + Send + 'static,
) {
    on_display(move || {
        let Some((web, events)) =
            VIEWS.with_borrow(|m| m.get(&key).map(|v| (v.web.clone(), Rc::clone(&v.events))))
        else {
            return tracing::warn!("webview[{key}]: {what} for no view; ignored");
        };
        f(&web, &events);
    });
}

/// The provider asked for the next navigation (load_uri, load_bytes): its policy decision is not reported (WebViewClient.shouldOverrideUrlLoading javadoc: "not called for navigations which the
/// app initiated with loadUrl()"). Cleared by that decision or by the next main-frame load start, so a load that makes no decision cannot hide a later page navigation.
fn provider_load(events: &Events) {
    events.provider_load.set(true);
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
    popup_token: u64,
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

/// The sink and what the signal closures share. The sink only hands events on (no app code runs inside it, so nothing calls back into the view from there), so no event nests in another.
struct Events {
    sink: RefCell<SendCallback<WebEventView>>,
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
    /// False once the view is retired ([`Self::retire`]) and while it is a parked popup: no event reaches the sink then ([`Self::push`]).
    active: std::cell::Cell<bool>,
    /// The key the view is named by; 0 while a parked popup (no key yet).
    key: std::cell::Cell<i32>,
    /// WebSettings.setSupportMultipleWindows: `create` parks a popup only while on.
    multiple_windows: std::cell::Cell<bool>,
    /// A parked popup's hold ([`Park`]); None for every other view.
    park: RefCell<Option<Park>>,
    /// An adopted popup: its page's `window.close()` is reported (once, [`Self::closed`]).
    popup: std::cell::Cell<bool>,
    /// POPUP_CLOSED was reported.
    closed: std::cell::Cell<bool>,
    /// Evaluations this view started and has not finished ([`view_evaluate`]). A window opened while one runs is not a user gesture: AOSP reports none for an evaluateJavascript-driven
    /// navigation (WebViewClientTest.testShouldOverrideUrlLoading: hasGesture false); WebKit's evaluate grants one.
    evaluating: std::cell::Cell<u32>,
}

/// A parked popup's first navigation: the one whose URI equals what `create` received is held until adoption (`held`); adopted before it came -> `released`, it proceeds when it comes.
/// No frame evidence in WebKitGTK's API (`frame_name` None for main and injected subframe alike, a34 gate f2), so the URI match is the identity.
struct Park {
    uri: String,
    held: Option<webkit6::PolicyDecision>,
    released: bool,
}

impl Events {
    /// A view's sink, live until [`Self::retire`].
    fn new(sink: SendCallback<WebEventView>) -> Rc<Self> {
        Rc::new(Self {
            sink: RefCell::new(sink),
            title: RefCell::default(),
            provider_load: std::cell::Cell::new(false),
            main_loading: std::cell::Cell::new(false),
            navigating: RefCell::default(),
            load_uri: RefCell::default(),
            location: RefCell::default(),
            active: std::cell::Cell::new(true),
            key: std::cell::Cell::new(0),
            multiple_windows: std::cell::Cell::new(false),
            park: RefCell::default(),
            popup: std::cell::Cell::new(false),
            closed: std::cell::Cell::new(false),
            evaluating: std::cell::Cell::new(0),
        })
    }

    /// Silences the view: [`Self::push`] delivers nothing from here on. [`retire_view`] calls it before the widget is detached or closed.
    fn retire(&self) {
        self.active.set(false);
    }

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

    /// The one submission point. A retired view delivers nothing; a reply-bearing event gets the terminal its kind means here: navigation taken (ignored, the page stays), prompt cancelled (JS gets null).
    /// Dropped unfinished, a navigation would load its URL.
    fn push(&self, p: Pending) {
        if !self.active.get() {
            match (p.kind, p.reply) {
                (web_event::NAVIGATION, Some(r)) => NavigationReply::finish(Box::new(r), true),
                (web_event::PROMPT, Some(r)) => PromptReply::finish(Box::new(r), None),
                _ => {}
            }
            return;
        }
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
            popup_token: p.popup_token,
            // the receiver takes `reply` only for PROMPT and NAVIGATION; any other event carries a placeholder this side drops
            reply: ManuallyDrop::new(p.reply.unwrap_or_else(|| ReplyAbi::new(|_| {}))),
        };
        self.sink.borrow_mut().call(&ev);
        if !handed {
            drop(ManuallyDrop::into_inner(ev.reply));
        }
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
    if let Some(taken) = parked_policy(ev, decision, &uri) {
        return taken;
    }
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
        defer(move || {
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

/// Runs a reply's engine step on the display thread, always as a later dispatch, never inline (`MainContext::invoke` runs inline on the owner): a reply finished inside the signal that made it
/// (the bridge query answered on D at once; a retired view's [`Events::push`]; a parked popup's ignored navigation) would close a dialog / decide a policy before the handler returned, and WebKit's
/// unref after it crashed (webkit_script_dialog_unref, measured in 3a). Any thread.
fn defer(f: impl FnOnce() + Send + 'static) {
    glib::idle_add_once(f);
}

/// A parked popup's navigation decision (None: not parked, the normal policy decides). Held: the first one for the URI `create` received. Every other while parked: ignored, named. Adopted
/// before it came: it proceeds now, unreported (the popup's own load, not a page navigation).
fn parked_policy(ev: &Events, decision: &webkit6::PolicyDecision, uri: &str) -> Option<bool> {
    let mut park = ev.park.borrow_mut();
    let p = park.as_mut()?;
    if p.released {
        let first = p.uri == uri;
        *park = None;
        if !first {
            return None;
        }
        ev.navigating.replace(Some(uri.to_owned()));
        decision.use_();
        return Some(true);
    }
    if p.held.is_none() && p.uri == uri {
        p.held = Some(decision.clone());
    } else {
        tracing::warn!(
            "webview: popup_parked_navigation_ignored: a navigation of a parked popup other than its first; ignored"
        );
        decision.ignore();
    }
    Some(true)
}

/// A pending popup (D only), one per opener key ([`PENDING`]).
struct PendingPopup {
    token: u64,
    web: webkit6::WebView,
    events: Rc<Events>,
}

thread_local! {
    /// Pending popups by opener key.
    static PENDING: RefCell<HashMap<i32, PendingPopup>> = RefCell::default();
    /// The last token issued; tokens are nonzero, never reused.
    static LAST_TOKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A popup target `create` refuses: no URL, empty, or about:blank (`window.open('')`, `window.open('about:blank')`). Its document is written synchronously by the opener with no navigation
/// decision to hold: written subresources reach the network before any Android decision (a34 probe case g); refused, the page's `window.open` returns null and nothing loads (gate g1, g2).
fn unsupported_popup_target(uri: Option<&str>) -> bool {
    match uri {
        None | Some("") => true,
        Some(u) => {
            u == "about:blank" || u.starts_with("about:blank#") || u.starts_with("about:blank?")
        }
    }
}

/// Drops a pending popup: its held navigation ignored, its view silenced and closed.
fn discard_popup(p: PendingPopup) {
    if let Some(d) = p.events.park.take().and_then(|park| park.held) {
        d.ignore();
    }
    p.events.retire();
    p.web.try_close();
}

/// `create` and `close` of `web` ([`Events::key`] read at signal time: a popup's key changes at adoption).
fn connect_popups(web: &webkit6::WebView, events: &Rc<Events>) {
    let ev = events.clone();
    web.connect_create(move |parent, action| {
        let key = ev.key.get();
        // a retired or parked view opens nothing (no recursive popup before adoption)
        if !ev.active.get() || key == 0 {
            return None;
        }
        // disabled: WebKit gets no view; same-view routing (WebSettings.java setSupportMultipleWindows false) is a later slice
        if !ev.multiple_windows.get() {
            return None;
        }
        let uri = action.request().and_then(|r| r.uri()).map(|u| u.to_string());
        if unsupported_popup_target(uri.as_deref()) {
            tracing::warn!("webview[{key}]: unsupported_popup_target: window.open with no URL or about:blank; refused");
            return None;
        }
        if PENDING.with_borrow(|m| m.contains_key(&key)) {
            tracing::warn!("webview[{key}]: popup_overlap_refused: a popup is already pending for this view; refused");
            return None;
        }
        // an open inside an evaluate this view started is no gesture (AOSP; `Events::evaluating`), whatever WebKit's evaluate grants
        let gesture = action.is_user_gesture() && ev.evaluating.get() == 0;
        // the related view shares the opener's web process, script controller and (WebKit default) settings: the settings are made private here, the destination's replace them at adoption
        let child = webkit6::WebView::builder().related_view(parent).build();
        let private = webkit6::Settings::new();
        private.set_enable_javascript(false);
        private.set_enable_html5_local_storage(false);
        child.set_settings(&private);
        let cev = Events::new(SendCallback::new(|_: &WebEventView| {}));
        cev.active.set(false);
        cev.park.replace(Some(Park {
            uri: uri.unwrap_or_default(),
            held: None,
            released: false,
        }));
        connect_view(&child, &cev);
        connect_popups(&child, &cev);
        let token = LAST_TOKEN.with(|t| {
            t.set(t.get() + 1);
            t.get()
        });
        PENDING.with_borrow_mut(|m| {
            m.insert(
                key,
                PendingPopup {
                    token,
                    web: child.clone(),
                    events: cev,
                },
            )
        });
        ev.push(Pending {
            kind: web_event::POPUP_REQUESTED,
            popup_token: token,
            flags: if gesture { web_event::FLAG_GESTURE } else { 0 },
            ..Pending::default()
        });
        Some(child.upcast())
    });
    let ev = events.clone();
    web.connect_close(move |_| {
        if ev.park.borrow().is_some() {
            // a parked popup closed by its page before adoption: cancelled, its opener told
            let Some((opener, p)) = PENDING.with_borrow_mut(|m| {
                let k = m
                    .iter()
                    .find(|(_, p)| Rc::ptr_eq(&p.events, &ev))
                    .map(|(k, _)| *k)?;
                m.remove(&k).map(|p| (k, p))
            }) else {
                return;
            };
            let token = p.token;
            discard_popup(p);
            if let Some(oev) = VIEWS.with_borrow(|m| m.get(&opener).map(|v| Rc::clone(&v.events))) {
                oev.push(Pending {
                    kind: web_event::POPUP_CANCELLED,
                    popup_token: token,
                    ..Pending::default()
                });
            }
            return;
        }
        // a retired view (view_drop's own try_close) pushes nothing; an adopted popup reports once
        if ev.popup.get() && !ev.closed.replace(true) {
            ev.emit(web_event::POPUP_CLOSED, "", "", 0, 0, None);
        }
    });
}

unsafe extern "C" fn view_cancel_popup(key: i32, token: u64) {
    on_display(move || match take_popup(key, token) {
        Some(p) => discard_popup(p),
        None => {
            tracing::warn!("webview[{key}]: cancel_popup for no pending popup {token}; ignored")
        }
    });
}

/// `key`'s pending popup if it is `token` (a stale token leaves a newer one in place).
fn take_popup(key: i32, token: u64) -> Option<PendingPopup> {
    PENDING.with_borrow_mut(|m| {
        if m.get(&key).is_some_and(|p| p.token == token) {
            m.remove(&key)
        } else {
            None
        }
    })
}

unsafe extern "C" fn view_adopt_popup(
    key: i32,
    token: u64,
    destination: i32,
    mut done: SendCallback<bool>,
) {
    let queued = on_display(move || {
        let Some(p) = take_popup(key, token) else {
            tracing::warn!("webview[{key}]: adopt_popup for no pending popup {token}: done(false)");
            return done.call(&false);
        };
        if destination == key || !VIEWS.with_borrow(|m| m.contains_key(&destination)) {
            tracing::warn!(
                "webview[{key}]: adopt_popup into no view {destination}; popup cancelled"
            );
            discard_popup(p);
            return done.call(&false);
        }
        adopt(p, destination);
        done.call(&true);
    });
    if !queued {
        tracing::error!("webview[{key}]: adopt_popup with no display thread");
    }
}

/// Puts the popup's view under `destination`'s key in place of its blank page: the old page's sources silenced and its widget detached, then the destination's sink, settings, background,
/// scale and scripts move to the popup, its widget is attached, and its held first navigation proceeds.
fn adopt(p: PendingPopup, destination: i32) {
    let Some(mut d) = VIEWS.with_borrow_mut(|m| m.remove(&destination)) else {
        return discard_popup(p);
    };
    d.events.retire();
    let sink = d
        .events
        .sink
        .replace(SendCallback::new(|_: &WebEventView| {}));
    let ce = p.events;
    ce.sink.replace(sink);
    ce.multiple_windows.set(d.events.multiple_windows.get());
    ce.key.set(destination);
    ce.popup.set(true);
    // every setting command queued before this one has been applied to the destination's settings object; later ones reach it through the key
    if let Some(s) = WebViewExt::settings(&d.web) {
        p.web.set_settings(&s);
    }
    p.web.set_background_color(&d.web.background_color());
    p.web.set_zoom_level(d.web.zoom_level());
    d.shown.call(&std::ptr::null_mut());
    use webkit6::glib::translate::ToGlibPtr;
    let widget: *mut webkit6::gtk::ffi::GtkWidget =
        p.web.upcast_ref::<webkit6::gtk::Widget>().to_glib_none().0;
    d.shown.call(&widget.cast());
    let old = std::mem::replace(&mut d.web, p.web);
    d.events = Rc::clone(&ce);
    VIEWS.with_borrow_mut(|m| m.insert(destination, d));
    ce.active.set(true);
    let held = {
        let mut park = ce.park.borrow_mut();
        if park.as_ref().is_some_and(|pk| pk.held.is_some()) {
            park.take().and_then(|pk| pk.held.map(|h| (pk.uri, h)))
        } else {
            if let Some(pk) = park.as_mut() {
                pk.released = true;
            }
            None
        }
    };
    if let Some((uri, decision)) = held {
        ce.navigating.replace(Some(uri));
        decision.use_();
    }
    old.try_close();
}

unsafe extern "C" fn view_create(
    key: i32,
    events: SendCallback<WebEventView>,
    shown: SendCallback<*mut c_void>,
) -> bool {
    on_display(move || create(key, events, shown))
}

/// [`view_create`] on the display thread.
fn create(key: i32, events: SendCallback<WebEventView>, mut shown: SendCallback<*mut c_void>) {
    // a display thread exists (on_display found the context owned) but GTK has no display: not reached in a launcher process, which owns the context only with GTK up
    if webkit6::gdk::Display::default().is_none() {
        return tracing::error!(
            "webview[{key}]: not created: {}",
            String::from_utf8_lossy(NO_DISPLAY)
        );
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
    let (web, events) = build_view(events);
    events.key.set(key);
    connect_view(&web, &events);
    connect_popups(&web, &events);
    use webkit6::glib::translate::ToGlibPtr;
    let widget: *mut webkit6::gtk::ffi::GtkWidget =
        web.upcast_ref::<webkit6::gtk::Widget>().to_glib_none().0;
    shown.call(&widget.cast());
    VIEWS.with_borrow_mut(|m| {
        m.insert(
            key,
            View {
                web,
                events,
                shown,
                scripts: Vec::new(),
            },
        )
    });
}

/// The engine defaults and the event sink; no signal connected yet ([`connect_view`]).
fn build_view(events: SendCallback<WebEventView>) -> (webkit6::WebView, Rc<Events>) {
    // autoplay is the app's setting alone (WebSettings.setMediaPlaybackRequiresUserGesture -> media-playback-requires-user-gesture): WebKit's own policy, ALLOW_WITHOUT_SOUND by default,
    // would refuse an audible play() the app allowed (NotAllowedError), which Android's WebView plays. Construct-only on the view, so set here once.
    let web = webkit6::WebView::builder()
        .user_content_manager(&with_scripts(|s| s.ucm.clone()))
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
    (web, Events::new(events))
}

/// Connects every engine signal of `web` to `events`; [`Events::retire`] silences them all.
fn connect_view(web: &webkit6::WebView, events: &Rc<Events>) {
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
        // answered asynchronously (WebKit keeps the dialog pending while a ref is held and close is not called), through [`defer`]
        let held = glib::thread_guard::ThreadGuard::new(dialog.clone());
        let reply = ReplyAbi::new(move |text: Option<String>| {
            defer(move || {
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
}

unsafe extern "C" fn view_drop(key: i32) {
    on_display(move || {
        // out of the map before anything runs: the detach and the close may run GTK code
        // its pending popup goes with it (no event: the opener is gone)
        if let Some(p) = PENDING.with_borrow_mut(|m| m.remove(&key)) {
            discard_popup(p);
        }
        let Some(v) = VIEWS.with_borrow_mut(|m| m.remove(&key)) else {
            return tracing::warn!("webview[{key}]: drop for no view; ignored");
        };
        retire_view(v);
    });
}

/// Retires `v` on D: its sources go silent first, then the widget is detached (`shown(null)`) and the page closed, so nothing from `v` reaches the sink after the detach.
fn retire_view(mut v: View) {
    v.events.retire();
    with_scripts(|s| v.scripts.iter().for_each(|src| s.release(src)));
    v.shown.call(&std::ptr::null_mut());
    v.web.try_close();
}

unsafe extern "C" fn view_load_uri(key: i32, uri: Str) {
    // SAFETY: `uri` is alive for this call.
    let uri = unsafe { uri.get() }.into_owned();
    on_view(key, "load_uri", move |web, events| {
        provider_load(events);
        web.load_uri(&uri);
    });
}

unsafe extern "C" fn view_load_bytes(
    key: i32,
    content: *const u8,
    len: usize,
    mime: Str,
    enc: Str,
    base: Str,
) {
    // SAFETY: the caller lends `content[..len]` and the strings for this call.
    let (bytes, mime, enc, base) = unsafe {
        (
            glib::Bytes::from(std::slice::from_raw_parts(content, len)),
            mime.get().into_owned(),
            enc.get().into_owned(),
            base.get().into_owned(),
        )
    };
    on_view(key, "load_bytes", move |web, events| {
        provider_load(events);
        web.load_bytes(&bytes, Some(&mime), Some(&enc), Some(&base));
    });
}

unsafe extern "C" fn view_stop(key: i32) {
    on_view(key, "stop", |web, _| web.stop_loading());
}

unsafe extern "C" fn view_reload(key: i32) {
    on_view(key, "reload", |web, _| web.reload());
}

/// The back-forward list's item `steps` from the current one (WebKitBackForwardList.get_nth_item: 0 current, negative back), as Chromium's NavigationController offsets count.
fn history_item(web: &webkit6::WebView, steps: i32) -> Option<webkit6::BackForwardListItem> {
    if steps == 0 {
        return None;
    }
    web.back_forward_list()?.nth_item(steps)
}

unsafe extern "C" fn view_go(key: i32, steps: i32) {
    on_view(key, "go", move |web, _| {
        if let Some(item) = history_item(web, steps) {
            web.go_to_back_forward_list_item(&item);
        }
    });
}

unsafe extern "C" fn view_evaluate(key: i32, script: Str, done: SendCallback<EvalResultView>) {
    // SAFETY: `script` is alive for this call.
    let script = unsafe { script.get() }.into_owned();
    on_view(key, "evaluate", move |web, events| {
        events.evaluating.set(events.evaluating.get() + 1);
        let events = Rc::clone(events);
        let mut done = Some(done);
        let mut finish = move |ok: bool, text: &str| {
            if let Some(mut d) = done.take() {
                d.call(&EvalResultView {
                    ok,
                    text: Str::of(text),
                });
            }
        };
        web.evaluate_javascript(
            &script,
            None,
            None,
            None::<&webkit6::gio::Cancellable>,
            move |r| {
                events
                    .evaluating
                    .set(events.evaluating.get().saturating_sub(1));
                match r {
                    // undefined, a function, a symbol: no JSON form -> "null", as Android's evaluateJavascript reports them
                    Ok(v) => finish(
                        true,
                        &v.to_json(0)
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "null".into()),
                    ),
                    Err(e) => finish(false, e.message()),
                }
            },
        );
    });
}

unsafe extern "C" fn view_set_user_scripts(key: i32, sources: *const Str, len: usize) {
    // SAFETY: the caller lends `sources[..len]` and their strings for this call.
    let sources: Vec<String> = unsafe { std::slice::from_raw_parts(sources, len) }
        .iter()
        // SAFETY: as above.
        .map(|s| unsafe { s.get() }.into_owned())
        .collect();
    on_display(move || {
        let Some(old) = VIEWS.with_borrow_mut(|m| {
            m.get_mut(&key)
                .map(|v| std::mem::replace(&mut v.scripts, sources.clone()))
        }) else {
            return tracing::warn!("webview[{key}]: set_user_scripts for no view; ignored");
        };
        // hold the new before releasing the old: a source in both stays installed
        with_scripts(|s| {
            sources.iter().for_each(|src| s.hold(src));
            old.iter().for_each(|src| s.release(src));
        });
    });
}

fn settings(web: &webkit6::WebView) -> webkit6::Settings {
    // a WebKitWebView always has settings (webkit_web_view_get_settings never returns NULL once constructed)
    WebViewExt::settings(web).expect("WebKitWebView without settings")
}

// The core's setting table (atl-webview settings.rs) decides each value's kind; a setting of another kind here is a broken table row and only logged.
unsafe extern "C" fn view_set_bool(key: i32, which: WebSetting, on: bool) {
    on_view(key, "set_bool", move |web, ev| match which {
        // no engine setting: this module's own switch for `create`
        WebSetting::SupportMultipleWindows => ev.multiple_windows.set(on),
        _ => set_bool(&settings(web), which, on),
    });
}

fn set_bool(s: &webkit6::Settings, which: WebSetting, on: bool) {
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

unsafe extern "C" fn view_set_int(key: i32, which: WebSetting, value: i32) {
    on_view(key, "set_int", move |web, _| {
        let s = settings(web);
        // WebKit's font sizes are unsigned; a negative one is no size
        let size = u32::try_from(value).unwrap_or(0);
        match which {
            WebSetting::MinimumFontSize => s.set_minimum_font_size(size),
            WebSetting::DefaultFontSize => s.set_default_font_size(size),
            WebSetting::DefaultMonospaceFontSize => s.set_default_monospace_font_size(size),
            other => tracing::warn!("webview: {other:?} is not an int setting"),
        }
    });
}

unsafe extern "C" fn view_set_str(key: i32, which: WebSetting, value: Str) {
    // SAFETY: `value` is alive for this call.
    let text = unsafe { value.get() }.into_owned();
    on_view(key, "set_str", move |web, _| {
        set_str(&settings(web), which, &text)
    });
}

fn set_str(s: &webkit6::Settings, which: WebSetting, text: &str) {
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

unsafe extern "C" fn view_set_background(key: i32, argb: u32) {
    // Android's colour int (alpha in the top byte); GdkRGBA takes each channel as 0..=1
    let channel = |shift: u32| f32::from(((argb >> shift) & 0xff) as u8) / 255.0;
    let rgba = webkit6::gdk::RGBA::new(channel(16), channel(8), channel(0), channel(24));
    on_view(key, "set_background", move |web, _| {
        web.set_background_color(&rgba)
    });
}

unsafe extern "C" fn view_set_initial_scale(key: i32, percent: i32) {
    on_view(key, "set_initial_scale", move |web, _| {
        // WebKit's zoom multiplies the toolkit's device scale; setInitialScale counts device pixels ("doesn't take the screen density into account", WebView.java:1186) -> divide it out
        let zoom = if percent > 0 {
            f64::from(percent) / 100.0 / f64::from(web.scale_factor().max(1))
        } else {
            1.0
        };
        web.set_zoom_level(zoom);
    });
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

    /// A reply's engine step never runs inside the call that finishes it, even on the display thread: it waits for the next dispatch ([`defer`]).
    #[test]
    fn deferred_reply_runs_after_the_finishing_call() {
        let ctx = glib::MainContext::default();
        let _owner = ctx.acquire().expect("default context free in this test");
        let ran = Rc::new(std::cell::Cell::new(false));
        let flag = glib::thread_guard::ThreadGuard::new(Rc::clone(&ran));
        defer(move || flag.into_inner().set(true));
        assert!(!ran.get(), "ran inside the finishing call (re-entrant)");
        while ctx.iteration(false) {}
        assert!(ran.get(), "never ran");
    }

    /// The refusal rule (a34 gate g1, g2): no URL, empty, about:blank with or without a fragment or query; anything else, about:blank-like or not, is held.
    #[test]
    fn unsupported_popup_targets() {
        for u in [
            None,
            Some(""),
            Some("about:blank"),
            Some("about:blank#x"),
            Some("about:blank?a"),
        ] {
            assert!(unsupported_popup_target(u), "{u:?}");
        }
        for u in [
            "about:blankx",
            "https://a.example/",
            "about:srcdoc",
            "data:text/html,x",
        ] {
            assert!(!unsupported_popup_target(Some(u)), "{u}");
        }
    }

    use std::sync::{Arc, Mutex};

    /// An [`Events`] whose sink records each delivered kind, in order.
    fn recording_events() -> (Rc<Events>, Arc<Mutex<Vec<u32>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let sink =
            SendCallback::new(move |e: &WebEventView| log.lock().expect("sink log").push(e.kind));
        (Events::new(sink), seen)
    }

    /// A reply recording how it was finished: `Some` = taken (navigation), `None` = cancelled or loaded.
    fn recording_reply() -> (ReplyAbi, Arc<Mutex<Vec<Option<String>>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        let reply = ReplyAbi::new(move |taken| seen.lock().expect("reply log").push(taken));
        (reply, log)
    }

    /// A view retired between two emits: the first reaches the sink, the late one does not.
    #[test]
    fn retired_view_drops_late_events() {
        let (ev, seen) = recording_events();
        ev.emit(web_event::LOAD_STARTED, "http://a.test/", "", 0, 0, None);
        ev.retire();
        ev.emit(web_event::TITLE, "late", "", 0, 0, None);
        assert_eq!(*seen.lock().expect("sink log"), [web_event::LOAD_STARTED]);
    }

    /// A retired view finishes each reply once with its cancel terminal and delivers nothing: navigation taken (`Some`, the page stays), prompt cancelled (`None`, JS gets null).
    #[test]
    fn retired_view_finishes_replies() {
        let (ev, seen) = recording_events();
        ev.retire();
        let (nav, nav_log) = recording_reply();
        ev.push(Pending {
            kind: web_event::NAVIGATION,
            reply: Some(nav),
            ..Pending::default()
        });
        let (prompt, prompt_log) = recording_reply();
        ev.push(Pending {
            kind: web_event::PROMPT,
            reply: Some(prompt),
            ..Pending::default()
        });
        let nav_log = nav_log.lock().expect("reply log");
        assert_eq!(nav_log.len(), 1, "one terminal per navigation");
        assert!(nav_log[0].is_some(), "navigation ignored, not loaded");
        assert_eq!(*prompt_log.lock().expect("reply log"), [None::<String>]);
        assert!(seen.lock().expect("sink log").is_empty());
    }
}
