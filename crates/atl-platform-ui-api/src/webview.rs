// SPDX-License-Identifier: MIT
//! The web engine behind `android.webkit.WebView` (webview plan S4.1, M68): shaped by its first implementation, WebKitGTK (atl-webview-linux), and by what the provider needs so far (W1: the engine's default user agent). Grows with W2 (views, settings, cookies).

/// The process's web engine. Stateless so far: one engine per process, reached through its module.
pub trait WebEngine {
    /// The engine's own default user agent string, as it would send it with no override (WebKit: `webkit_settings_get_user_agent` of fresh settings). Any thread.
    fn default_user_agent() -> Result<String, WebViewError>;
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
