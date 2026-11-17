// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.webkit.ValueCallback;

/**
 * The provider's unimplemented methods (webview plan S4.1): libatl_webview.so counts each (unimplemented_api) and names it "<Interface>.<method>".
 * A forwarder (AGENTS Languages): each method body is one native call.
 */
final class Unimplemented {
    private Unimplemented() {}

    /**
     * Counts {@code api} and returns UnsupportedOperationException(api) for the caller to throw (Java needs the throw statement for its return types).
     * Class C only (xtask provider_refusals table, a `cargo xtask check` step): the WebView contract allows no answer that would not mislead the app.
     */
    static native RuntimeException refuse(String api);

    /** Counts {@code api} without throwing: the caller answers the WebView contract's default, failure value or no-op (class B). */
    static native void accept(String api);

    /** Counts {@code api} and hands {@code callback} (may be null) {@code value}, the contract's failure value, on the calling thread (class B). */
    static native void answer(String api, ValueCallback<?> callback, Object value);
}
