// SPDX-License-Identifier: MIT
//! The WebView module's exports (`atl_platform_ui_api::abi::WebViewVtable`): WebKitGTK behind the facade's `PlatformWebEngine`.
//! Default UA: `webkit_settings_get_user_agent` of fresh settings -- the engine's own default. Measured (W1c probe, WebKitGTK 2.54): works with no display, without gtk_init and off the main thread, so a provider call from an app worker thread (AdMob) needs no UI.

use atl_platform_ui_api::abi::{self, BytesOut, WebViewVtable};

static VTABLE: WebViewVtable = WebViewVtable {
    abi: abi::ABI_ID,
    default_user_agent,
};

/// [`abi::WebViewEntry`]: the static vtable; starts nothing.
#[unsafe(no_mangle)]
pub extern "C" fn atl_webview_entry() -> *const WebViewVtable {
    &VTABLE
}

unsafe extern "C" fn default_user_agent(out: *mut BytesOut) -> bool {
    // WebKitSettings' constructor reads the default GDK display's GL support and dereferences a missing display (SIGSEGV, measured in the no-UI app run and a probe with no reachable compositor).
    // The display is GTK's: opened by the UI module's attach (same libgtk object in the process). A process without one gets a refusal, never the crash; GTK is not initialised here, because that would bind GTK's main thread to whichever thread called.
    if webkit6::gdk::Display::default().is_none() {
        // SAFETY: `out` is the sink the caller lent for this call.
        unsafe {
            BytesOut::put_to(
                out,
                b"no display: the process has no GTK display (no-UI mode)",
            )
        };
        return false;
    }
    let ua = webkit6::Settings::new().user_agent().unwrap_or_default();
    // SAFETY: as above.
    unsafe { BytesOut::put_to(out, ua.as_bytes()) };
    true
}
