// SPDX-License-Identifier: MIT
//! `libatl_webview.so`, the WebView provider's natives (webview plan S4.1): the Java forwarders in package com.android.webview.chromium call here.
//! Built: `getStatics` (one instance per process, as Chromium's provider keeps one), `Statics.getDefaultUserAgent` ([`ua`], the engine's tokens through `PlatformWebEngine`); W2 `createWebView` and the
//! core of `WebViewProviderImpl` / `WebSettingsImpl` (load, `evaluateJavascript`, [`bridge`], client callbacks). Every other method throws UnsupportedOperationException("<Interface>.<method>"),
//! counted (plan S4.1 refusals) through the Java `Unimplemented.refuse`.
#![deny(unsafe_code)]

pub mod bridge;
#[allow(unsafe_code)]
mod jni;
pub mod ua;

use atl_platform_ui::PlatformWebEngine;
use atl_platform_ui::webview::WebEngine as _;

/// `Statics.getDefaultUserAgent`: the CDD string from the engine's default UA and the app's Build fields. The engine's string is logged once per process (`webview: engine user agent: <ua>`), the evidence the app gate assembles its expected string from.
pub fn default_user_agent(device: &ua::Device) -> Result<String, String> {
    let engine = PlatformWebEngine::default_user_agent().map_err(|e| e.to_string())?;
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| tracing::info!("webview: engine user agent: {engine}"));
    ua::default_user_agent(&engine, device).map_err(|e| e.to_string())
}

/// The message of an unimplemented provider method's UnsupportedOperationException, counted.
pub fn refuse(api: &str) -> &str {
    atl_diag::unimplemented_api!(api);
    api
}
