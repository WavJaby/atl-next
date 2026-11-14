// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

/**
 * Refusals of the provider's unimplemented methods (webview plan S4.1): libatl_webview.so counts each (unimplemented_api) and names it "<Interface>.<method>".
 * A forwarder (AGENTS Languages): each method body is one native call.
 */
final class Unimplemented {
    private Unimplemented() {}

    /** Counts {@code api} and returns UnsupportedOperationException(api) for the caller to throw (Java needs the throw statement for its return types). */
    static native RuntimeException refuse(String api);

    /**
     * Counts {@code api} without throwing: only for callbacks the View system itself makes on every WebView (constructor chain, layout), where a throw would fail
     * every WebView; they take effect once the page is composed (W3).
     */
    static native void accept(String api);
}
