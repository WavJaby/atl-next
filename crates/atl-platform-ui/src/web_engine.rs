// SPDX-License-Identifier: MIT
//! The web engine's module (webview plan S2.3): `libatl_webview_linux.so`, loaded `RTLD_NOW | RTLD_LOCAL` on the first [`PlatformWebEngine`] call, from any thread; a failure is kept and every later call gets the same text. Identity check as the UI module's.

use std::ffi::c_void;
use std::sync::OnceLock;

use atl_platform_ui_api::abi::{self, BytesOut, WebViewEntry, WebViewVtable};
use atl_platform_ui_api::webview::{WebEngine, WebViewError};

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
}
