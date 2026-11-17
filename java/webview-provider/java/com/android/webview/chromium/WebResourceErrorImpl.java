// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.webkit.WebResourceError;

/** A WebResourceError the natives build for WebViewClient.onReceivedError (a forwarder, AGENTS Languages: JNI cannot subclass a Java class): values only. */
final class WebResourceErrorImpl extends WebResourceError {
    private final int mCode;
    private final String mDescription;

    // called by the natives
    private WebResourceErrorImpl(int code, String description) {
        mCode = code;
        mDescription = description;
    }

    @Override public int getErrorCode() { return mCode; }
    @Override public CharSequence getDescription() { return mDescription; }
}
