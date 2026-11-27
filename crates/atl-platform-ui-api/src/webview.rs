// SPDX-License-Identifier: MIT
//! The web engine behind `android.webkit.WebView` (webview plan S4.1, M68): shaped by its first implementation, WebKitGTK (atl-webview-linux), and by what the provider needs so far (W1 default UA; W2 views: load, JS, script dialogs, load events).
//! Threads (display-thread plan D5): the engine runs on the display thread only (the owner of GLib's default context, the UI module's thread). [`WebEngine::create_view`] and the [`WebView`] commands run
//! on the view's own thread (the WebView's Looper thread, any thread) and are queued to the display thread in call order, returning at once; events and results arrive on the display thread, where
//! the receiver hands them on (the provider posts them to the WebView's Looper). [`WebView::claim_input`] and [`WebView::set_focused`] go to the UI module: the claim's answer is read there under a lock, its replay and the focus are queued to the display thread.
//! No method answers with engine state: what the provider's synchronous getters need comes with the events ([`WebEvent::Location`], `Title`, `Progress`), so the display thread never has to answer one.

/// The process's web engine, reached through its module.
pub trait WebEngine {
    /// One page view.
    type View: WebView;

    /// The engine's own default user agent string, as it would send it with no override (WebKit: `webkit_settings_get_user_agent` of fresh settings). Any thread.
    fn default_user_agent() -> Result<String, WebViewError>;

    /// A new view, not shown anywhere (W0: an unparented WebKit view loads and runs JS), made on the display thread after this returns; its sideband key is allocated here. `events` gets every
    /// [`WebEvent`] on the display thread until the view drops. JavaScript starts disabled (Android's default, WebSettings.getJavaScriptEnabled). Err: no engine module, or no display thread.
    fn create_view(events: Box<dyn FnMut(WebEvent) + Send>) -> Result<Self::View, WebViewError>;

    /// The process's cookie store (shared by every view). Any thread: the engine is touched on the display thread only, so each call is queued there and returns at once; callbacks run on the display thread.
    /// `changed` gets the store's full list now and again after every change, whoever made it (a page's `document.cookie` / `Set-Cookie`, or [`Self::add_cookie`] / [`Self::replace_cookies`]).
    /// Watching twice replaces the first watcher.
    fn watch_cookies(changed: Box<dyn FnMut(Vec<Cookie>) + Send>) -> Result<(), WebViewError>;
    /// Adds or replaces (same name, domain, path) `cookie`; `done(true)` once the store has it.
    fn add_cookie(cookie: &Cookie, done: Box<dyn FnOnce(bool) + Send>) -> Result<(), WebViewError>;
    /// Makes `cookies` the store's whole content; `done(true)` once it is.
    fn replace_cookies(
        cookies: &[Cookie],
        done: Box<dyn FnOnce(bool) + Send>,
    ) -> Result<(), WebViewError>;
    /// Which cookies the network side accepts, for every view (the engine has one policy per store).
    fn set_cookie_policy(policy: CookiePolicy) -> Result<(), WebViewError>;
}

/// One cookie, RFC 6265 storage model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cookie {
    /// The name.
    pub name: String,
    /// The value.
    pub value: String,
    /// The host, lower case: `example.com` = host-only (sent to that host alone), `.example.com` = a Domain cookie (that host and its subdomains).
    pub domain: String,
    /// The path, starting with `/`.
    pub path: String,
    /// Expiry in seconds since the Unix epoch; None = a session cookie.
    pub expires: Option<i64>,
    /// Sent over secure schemes only.
    pub secure: bool,
    /// Not visible to `document.cookie`.
    pub http_only: bool,
}

/// What the cookie store accepts ([`WebEngine::set_cookie_policy`]). The numbers are the vtable's ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum CookiePolicy {
    /// Every cookie.
    Always = 1,
    /// None.
    Never = 2,
    /// First-party cookies only.
    NoThirdParty = 3,
}

/// One page view; dropping it destroys the view (queued to the display thread as the commands are).
pub trait WebView {
    /// Navigates to `uri` (any scheme the engine loads: `http(s):`, `file:`, `data:`, `about:`).
    fn load_uri(&self, uri: &str);
    /// Loads `content` as a document of `mime_type` / `encoding` whose URL is `base_uri` (relative links resolve against it).
    fn load_bytes(&self, content: &[u8], mime_type: &str, encoding: &str, base_uri: &str);
    /// Stops the current load.
    fn stop_loading(&self);
    /// Reloads the current page.
    fn reload(&self);
    /// Goes to the session-history entry `steps` away; none there = nothing happens.
    fn go_back_or_forward(&self, steps: i32);
    /// Runs `script` in the main frame; `done` gets the result as JSON (`null` for undefined, a function or a non-serialisable value), or the exception's text, on the display thread.
    fn evaluate_javascript(
        &self,
        script: &str,
        done: Box<dyn FnOnce(Result<String, String>) + Send>,
    );
    /// Replaces this view's scripts injected at document start into every frame of every later load (an already loaded document keeps what it got). A source must be view-agnostic: the
    /// backend may run one controller for several views (a popup shares its opener's), where a source any view holds runs in all of them.
    fn set_user_scripts(&self, sources: &[String]);
    /// Applies one engine-backed setting to the view; `value` has the kind of `which` ([`WebSetting::kind`]). The core stores every WebSettings value and calls this only for the ones WebKit has.
    fn set_setting(&self, which: WebSetting, value: &SettingValue);
    /// The view's background colour, an Android colour int (ARGB). WebView.setBackgroundColor hands it to the provider only (WebView.java:3072), so the page's background is the only one.
    fn set_background_color(&self, argb: u32);
    /// The page scale: `percent` CSS pixels per 100 device pixels (WebView.setInitialScale's unit: screen density not applied); 0 = the engine's default scale.
    fn set_initial_scale(&self, percent: i32);
    /// The ints of the sideband stream that shows this view (webview plan S2.2 amended): the provider sets them on the layer that carries the view, and every window draws the view where a quad with
    /// that stream lands, at that quad's depth. The same ints for the view's life.
    fn sideband_stream(&self) -> [i32; 2];
    /// Android's dispatch delivered `claim`, stamped `time_ns` (CLOCK_MONOTONIC, the event's own time), to the View: the toolkit events held for the view up to that time reach the page
    /// (webview plan S2.2 amended, hold-and-replay).
    fn claim_input(&self, claim: InputClaim, time_ns: i64) -> ClaimReply;
    /// Android focus: the view takes the toolkit's keyboard focus (and with it the desktop input method, through the engine's own context), or gives it back.
    fn set_focused(&self, on: bool);
    /// Refuses the popup `token` this view (its opener) reported ([`WebEvent::PopupRequested`]): the parked engine view is destroyed, its held first navigation ignored. A token that no longer
    /// names this view's pending popup (cancelled, closed, adopted, or a newer one) is ignored.
    fn cancel_popup(&self, token: PopupToken);
    /// Adopts the popup `token` this view opened into `destination`: the engine's browsing context replaces `destination`'s blank page under `destination`'s key, sink, settings and scripts,
    /// and its held first navigation proceeds. `done` once, on the display thread, in command order: true = adopted; false = the token no longer names a pending popup of this view, or
    /// `destination` is gone (the popup is then cancelled).
    fn adopt_popup(
        &self,
        token: PopupToken,
        destination: &Self,
        done: Box<dyn FnOnce(bool) + Send>,
    ) where
        Self: Sized;
}

/// A pending popup's one-shot capability ([`WebEvent::PopupRequested`]): process-unique, never 0, never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PopupToken(u64);

impl PopupToken {
    /// The ABI form; None for 0 (no token).
    pub fn of_raw(raw: u64) -> Option<Self> {
        (raw != 0).then_some(Self(raw))
    }

    /// The ABI form.
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// What a claim ([`WebView::claim_input`]) answers synchronously; what the claim's replay finds later is reported as a [`WebEvent`] ([`WebEvent::PressNotCancelled`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClaimReply {
    /// A Down claim whose press is a mouse button's (the toolkit event, whatever MotionEvent the desktop adaptation made of it): Chromium's WebView focuses on mouse down
    /// (kFocusRenderWidgetHostViewAndroidOnMouseDown) but not on touch down (kFocusRenderWidgetHostViewAndroidOnActionDown, disabled for WebView). The origin is classified where the window publishes the press.
    pub mouse_press: bool,
}

impl ClaimReply {
    /// Bit 1 was a Cancel's left-pressed report, now [`WebEvent::PressNotCancelled`]; not reused.
    const MOUSE_PRESS: u32 = 2;

    /// The ABI form (UiVtable::sideband_claim's return).
    pub fn bits(self) -> u32 {
        if self.mouse_press {
            Self::MOUSE_PRESS
        } else {
            0
        }
    }

    /// From [`Self::bits`].
    pub fn of_bits(b: u32) -> Self {
        Self {
            mouse_press: b & Self::MOUSE_PRESS != 0,
        }
    }
}

/// What Android delivered to a WebView ([`WebView::claim_input`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum InputClaim {
    /// ACTION_DOWN: a gesture starts at the View.
    Down = 1,
    /// ACTION_MOVE (all its batched samples), or a generic motion (scroll).
    Move = 2,
    /// ACTION_UP.
    Up = 3,
    /// ACTION_CANCEL: a parent took the gesture.
    Cancel = 4,
    /// A hover event.
    Hover = 5,
    /// A KeyEvent.
    Key = 6,
}

impl InputClaim {
    /// The claim of its `repr` number, None for another.
    pub fn of(n: u32) -> Option<Self> {
        Some(match n {
            1 => Self::Down,
            2 => Self::Move,
            3 => Self::Up,
            4 => Self::Cancel,
            5 => Self::Hover,
            6 => Self::Key,
            _ => return None,
        })
    }
}

/// The first int of a WebView's sideband stream ([`WebView::sideband_stream`]): a sideband quad with another tag is not a WebView (a TV tuner's stream) and shows nothing. ASCII "WebV".
pub const SIDEBAND_WEB_VIEW: i32 = 0x5765_6256;

/// What a view reports. Within one load `LoadStarted` comes first; `Title` / `Progress` come as the engine notifies them, before or after `LoadFinished` (AOSP fixes no title order either).
pub enum WebEvent {
    /// The main frame started loading `uri`.
    LoadStarted(String),
    /// The main frame finished loading `uri` (also after a failure, as WebKit reports it).
    LoadFinished(String),
    /// The main frame's load of `uri` failed.
    LoadFailed {
        /// The failing URL.
        uri: String,
        /// The engine's error text.
        message: String,
        /// The engine's error kind.
        kind: LoadError,
    },
    /// The title changed.
    Title(String),
    /// The estimated load progress changed, 0..=100.
    Progress(i32),
    /// The view's location changed: its URL, its session history around the current entry. Reported before the load event it belongs to, so a receiver keeping the last one answers the
    /// synchronous getters (getUrl, getOriginalUrl, canGoBackOrForward) as of the events it has seen (display-thread plan D5: no getter waits for the engine's thread).
    Location {
        /// The current URL, if any.
        uri: Option<String>,
        /// The URL the current history entry was first requested as (before redirects); the current URL for a page with no history entry.
        original_uri: Option<String>,
        /// Session-history entries behind the current one.
        back: u32,
        /// Session-history entries ahead of the current one.
        forward: u32,
    },
    /// A page called `prompt(message, default)`; JavaScript blocks until `reply` finishes (from any thread). Dropping `reply` unfinished answers `null`.
    Prompt {
        /// prompt's first argument.
        message: String,
        /// prompt's second argument.
        default: String,
        /// The answer channel.
        reply: Box<dyn PromptReply>,
    },
    /// The page left the key Android delivered stamped this monotonic ns (`KeyEvent.getEventTimeNanos`, the time [`WebView::claim_input`] got) unhandled: no handler took it, the engine's default did nothing.
    /// Only key presses are reported (WebKitGTK puts back an unhandled press, not a release). The provider reports it to the app (WebViewClient.onUnhandledKeyEvent).
    UnhandledKey(i64),
    /// A Cancel claim ([`InputClaim::Cancel`]: a parent intercepted) found the page mid-press: its Down's press reached the page, and no toolkit API sends the page a cancel (webview plan S2.2), a divergence
    /// the provider counts. Reported while the claim's held events are processed, before any later claim's.
    PressNotCancelled,
    /// A navigation's response is one the engine does not show (a type it cannot render, or `Content-Disposition: attachment`): the navigation was dropped, nothing is saved.
    Download {
        /// The response's URL.
        url: String,
        /// The `Content-Disposition` header, empty without one.
        content_disposition: String,
        /// The response's MIME type.
        mime_type: String,
        /// The body length, -1 when unknown.
        content_length: i64,
    },
    /// A navigation the page, the user or a redirect started is about to start; it waits until `reply` finishes (no `LoadStarted` before). Not reported: navigations the provider asked for
    /// ([`WebView::load_uri`], [`WebView::load_bytes`], [`WebView::reload`], [`WebView::go_back_or_forward`]; a redirect of one is reported), POST navigations, and loads the engine takes for
    /// a subframe's first load ([`NavigationRequest::main_frame`]).
    Navigation {
        /// What is about to load.
        request: NavigationRequest,
        /// The answer channel.
        reply: Box<dyn NavigationReply>,
    },
    /// The page opened a window (`window.open`, a `target=_blank` link) and multiple windows are on ([`WebSetting::SupportMultipleWindows`]): the engine made the new browsing context and
    /// parks it, its first navigation held, until [`WebView::adopt_popup`] or [`WebView::cancel_popup`]. At most one per view at a time; a further open meanwhile is refused (null to the page).
    PopupRequested {
        /// The capability naming it.
        token: PopupToken,
        /// Opened by a user gesture (the open's own, not its navigation's).
        gesture: bool,
    },
    /// The engine dropped the pending popup `token` before adoption (the page closed it): it names nothing any more.
    PopupCancelled {
        /// The cancelled popup.
        token: PopupToken,
    },
    /// The page asked this (adopted popup) view to close (`window.close()`): reported once; never after the view is dropped.
    PopupClosed,
}

/// A [`WebEvent::Navigation`]'s request (WebResourceRequest's fields).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavigationRequest {
    /// The URL to load.
    pub uri: String,
    /// The HTTP method (`GET`).
    pub method: String,
    /// For the main frame. WebKitGTK's UI-process API names no target frame: a load of a scheme the engine loads itself (http(s), about:, data:, ...) with no gesture, not a redirect, of navigation
    /// type "other", while the main frame is loading, is taken for a subframe's first load (an `<iframe>` of the page) and not reported; every other navigation is reported as the main frame's.
    pub main_frame: bool,
    /// A server redirect of a navigation.
    pub redirect: bool,
    /// Started by a user gesture (a click, a key).
    pub gesture: bool,
}

/// The answer to a [`WebEvent::Navigation`]; any thread.
pub trait NavigationReply: Send {
    /// `true`: the app took the URL, the engine does not load it (the page stays). `false`: the engine loads it; a scheme the engine cannot load ends in
    /// [`WebEvent::LoadFailed`] of kind [`LoadError::UnsupportedScheme`], the page stays. Dropping `reply` unfinished = `false`.
    fn finish(self: Box<Self>, take: bool);
}

/// A load failure's kind, coarse: what Android's WebViewClient error codes need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum LoadError {
    /// The load was cancelled (a new navigation replaced it).
    Cancelled = 1,
    /// The host could not be resolved or reached, or another network failure.
    Network = 2,
    /// The file or URL was not found.
    NotFound = 3,
    /// Any other failure.
    Other = 4,
    /// A URL whose scheme the engine has no loader for (a custom scheme, `intent:`, `market:`): nothing was loaded, the page stays.
    UnsupportedScheme = 5,
}

impl LoadError {
    /// The kind of its `repr` number; an unknown number = [`Self::Other`].
    pub fn of(n: u32) -> Self {
        match n {
            1 => Self::Cancelled,
            2 => Self::Network,
            3 => Self::NotFound,
            5 => Self::UnsupportedScheme,
            _ => Self::Other,
        }
    }
}

/// The answer to a [`WebEvent::Prompt`]; any thread.
pub trait PromptReply: Send {
    /// Answers the prompt: `Some(text)` as if typed and confirmed, `None` as if cancelled (JavaScript gets `null`).
    fn finish(self: Box<Self>, text: Option<String>);
}

/// Why a [`WebEngine`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum WebViewError {
    /// The engine's module cannot be loaded (missing host library, a module of another build).
    #[error("web engine unavailable: {0}")]
    Unavailable(String),
    /// The engine needs a display the process does not have (WebKitGTK reads the GDK display's GL support when its settings are made).
    #[error("web engine: {0}")]
    NoDisplay(String),
}

/// The engine-backed WebSettings properties: the engine's own setting per variant (the linux module maps them). The numbers are the vtable's ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum WebSetting {
    /// enable-javascript.
    JavaScript = 1,
    /// enable-html5-local-storage (DOM storage).
    DomStorage = 2,
    /// media-playback-requires-user-gesture.
    MediaPlaybackRequiresUserGesture = 3,
    /// allow-universal-access-from-file-urls.
    AllowUniversalAccessFromFileUrls = 4,
    /// allow-file-access-from-file-urls.
    AllowFileAccessFromFileUrls = 5,
    /// javascript-can-open-windows-automatically.
    JavaScriptCanOpenWindowsAutomatically = 6,
    /// auto-load-images.
    AutoLoadImages = 7,
    /// minimum-font-size.
    MinimumFontSize = 8,
    /// default-font-size.
    DefaultFontSize = 9,
    /// default-monospace-font-size.
    DefaultMonospaceFontSize = 10,
    /// user-agent; empty = the engine's own default.
    UserAgent = 11,
    /// default-font-family (WebSettings' standard font).
    DefaultFontFamily = 12,
    /// monospace-font-family (WebSettings' fixed font).
    MonospaceFontFamily = 13,
    /// sans-serif-font-family.
    SansSerifFontFamily = 14,
    /// serif-font-family.
    SerifFontFamily = 15,
    /// cursive-font-family.
    CursiveFontFamily = 16,
    /// fantasy-font-family.
    FantasyFontFamily = 17,
    /// default-charset.
    DefaultCharset = 18,
    /// WebSettings.setSupportMultipleWindows: no engine setting; the backend reports [`WebEvent::PopupRequested`] only while on (default off).
    SupportMultipleWindows = 19,
}

/// The value kinds of settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    /// On or off.
    Bool,
    /// A number (font sizes, enum ordinals, ints of WebSettings).
    Int,
    /// Text.
    Str,
}

impl WebSetting {
    /// The kind of this setting's value; fixed per variant.
    pub fn kind(self) -> SettingKind {
        match self {
            Self::JavaScript
            | Self::DomStorage
            | Self::MediaPlaybackRequiresUserGesture
            | Self::AllowUniversalAccessFromFileUrls
            | Self::AllowFileAccessFromFileUrls
            | Self::JavaScriptCanOpenWindowsAutomatically
            | Self::AutoLoadImages
            | Self::SupportMultipleWindows => SettingKind::Bool,
            Self::MinimumFontSize | Self::DefaultFontSize | Self::DefaultMonospaceFontSize => {
                SettingKind::Int
            }
            Self::UserAgent
            | Self::DefaultFontFamily
            | Self::MonospaceFontFamily
            | Self::SansSerifFontFamily
            | Self::SerifFontFamily
            | Self::CursiveFontFamily
            | Self::FantasyFontFamily
            | Self::DefaultCharset => SettingKind::Str,
        }
    }
}

/// A setting's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingValue {
    /// A boolean setting.
    Bool(bool),
    /// An int setting.
    Int(i32),
    /// A string setting.
    Str(String),
}

impl SettingValue {
    /// The kind of this value.
    pub fn kind(&self) -> SettingKind {
        match self {
            Self::Bool(_) => SettingKind::Bool,
            Self::Int(_) => SettingKind::Int,
            Self::Str(_) => SettingKind::Str,
        }
    }

    /// The boolean, None for another kind.
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// The int, None for another kind.
    pub fn as_int(&self) -> Option<i32> {
        match *self {
            Self::Int(i) => Some(i),
            _ => None,
        }
    }

    /// The text, None for another kind.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
}

impl From<bool> for SettingValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

impl From<i32> for SettingValue {
    fn from(i: i32) -> Self {
        Self::Int(i)
    }
}

impl From<&str> for SettingValue {
    fn from(s: &str) -> Self {
        Self::Str(s.to_owned())
    }
}

impl From<String> for SettingValue {
    fn from(s: String) -> Self {
        Self::Str(s)
    }
}
