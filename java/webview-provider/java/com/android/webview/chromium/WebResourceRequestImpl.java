// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.net.Uri;
import android.webkit.WebResourceRequest;

import java.util.Collections;
import java.util.Map;

/** A WebResourceRequest the natives build for a client callback (a forwarder, AGENTS Languages: JNI cannot implement a Java interface): values only. No request headers: the engine exposes none for a navigation before it is sent. */
final class WebResourceRequestImpl implements WebResourceRequest {
    private final Uri mUrl;
    private final boolean mMainFrame;
    private final boolean mRedirect;
    private final boolean mGesture;
    private final String mMethod;

    // called by the natives
    private WebResourceRequestImpl(String url, boolean mainFrame, boolean redirect, boolean gesture, String method) {
        mUrl = Uri.parse(url);
        mMainFrame = mainFrame;
        mRedirect = redirect;
        mGesture = gesture;
        mMethod = method;
    }

    @Override public Uri getUrl() { return mUrl; }
    @Override public boolean isForMainFrame() { return mMainFrame; }
    @Override public boolean isRedirect() { return mRedirect; }
    @Override public boolean hasGesture() { return mGesture; }
    @Override public String getMethod() { return mMethod; }
    @Override public Map<String, String> getRequestHeaders() { return Collections.emptyMap(); }
}
