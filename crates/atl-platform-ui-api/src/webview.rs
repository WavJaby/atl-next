// SPDX-License-Identifier: MIT
//! The web engine behind `android.webkit.WebView` (webview plan S4.1, M68): shaped by its first implementation, WebKitGTK (atl-webview-linux), and by what the provider needs so far (W1 default UA; W2 views: load, JS, script dialogs, load events).
//! Threads: [`WebView`] and [`WebEngine::create_view`] run on the UI thread (= the GTK main thread, plan README Q6), as Android's WebView methods do; events arrive there too. [`PromptReply`] alone is Send.

/// The process's web engine, reached through its module.
pub trait WebEngine {
    /// One page view.
    type View: WebView;

    /// The engine's own default user agent string, as it would send it with no override (WebKit: `webkit_settings_get_user_agent` of fresh settings). Any thread.
    fn default_user_agent() -> Result<String, WebViewError>;

    /// A new view, not shown anywhere (W0: an unparented WebKit view loads and runs JS); `events` gets every [`WebEvent`] on the UI thread until the view drops. JavaScript starts disabled (Android's default, WebSettings.getJavaScriptEnabled).
    fn create_view(events: Box<dyn FnMut(WebEvent)>) -> Result<Self::View, WebViewError>;
}

/// One page view; dropping it destroys the view.
pub trait WebView {
    /// Navigates to `uri` (any scheme the engine loads: `http(s):`, `file:`, `data:`, `about:`).
    fn load_uri(&self, uri: &str);
    /// Loads `content` as a document of `mime_type` / `encoding` whose URL is `base_uri` (relative links resolve against it).
    fn load_bytes(&self, content: &[u8], mime_type: &str, encoding: &str, base_uri: &str);
    /// Stops the current load.
    fn stop_loading(&self);
    /// Reloads the current page.
    fn reload(&self);
    /// Runs `script` in the main frame; `done` gets the result as JSON (`null` for undefined, a function or a non-serialisable value), or the exception's text.
    fn evaluate_javascript(&self, script: &str, done: Box<dyn FnOnce(Result<String, String>)>);
    /// Replaces the scripts injected at document start into every frame of every later load (an already loaded document keeps what it got).
    fn set_user_scripts(&self, sources: &[String]);
    /// The current URL, if any.
    fn uri(&self) -> Option<String>;
    /// The current title, if any.
    fn title(&self) -> Option<String>;
    /// The estimated progress of the current load, 0..=100.
    fn progress(&self) -> i32;
    /// Enables or disables JavaScript for later loads.
    fn set_javascript_enabled(&self, on: bool);
    /// Whether JavaScript is enabled.
    fn javascript_enabled(&self) -> bool;
    /// Sets the user agent string sent with requests and seen by `navigator.userAgent`.
    fn set_user_agent(&self, ua: &str);
    /// The user agent string in use.
    fn user_agent(&self) -> String;
    /// Enables DOM storage (`localStorage` / `sessionStorage`).
    fn set_dom_storage_enabled(&self, on: bool);
    /// Whether DOM storage is enabled.
    fn dom_storage_enabled(&self) -> bool;
}

/// What a view reports.
pub enum WebEvent {
    /// The main frame started loading `uri`.
    LoadStarted(String),
    /// The main frame received its first data for `uri` (WebKit `LOAD_COMMITTED`).
    LoadCommitted(String),
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
    /// A page called `prompt(message, default)`; JavaScript blocks until `reply` finishes (from any thread). Dropping `reply` unfinished answers `null`.
    Prompt {
        /// prompt's first argument.
        message: String,
        /// prompt's second argument.
        default: String,
        /// The answer channel.
        reply: Box<dyn PromptReply>,
    },
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
