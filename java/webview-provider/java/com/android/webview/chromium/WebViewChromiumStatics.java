// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.content.Context;
import android.content.Intent;
import android.net.Uri;
import android.webkit.ValueCallback;
import android.webkit.WebViewFactoryProvider;
import java.util.List;

/**
 * WebViewFactoryProvider.Statics as a forwarder (AGENTS Languages): JNI cannot create an object implementing a Java interface. One instance per process, made by the native getStatics.
 * getDefaultUserAgent is real (webview plan S5); the native side throws UnsupportedOperationException("WebViewFactoryProvider.Statics.<method>") for the rest and counts it.
 */
final class WebViewChromiumStatics implements WebViewFactoryProvider.Statics {
    @Override
    public native String findAddress(String addr);

    @Override
    public native String getDefaultUserAgent(Context context);

    @Override
    public native void freeMemoryForTests();

    @Override
    public native void setWebContentsDebuggingEnabled(boolean enable);

    @Override
    public native void clearClientCertPreferences(Runnable onCleared);

    @Override
    public native void enableSlowWholeDocumentDraw();

    @Override
    public native Uri[] parseFileChooserResult(int resultCode, Intent intent);

    @Override
    public native void initSafeBrowsing(Context context, ValueCallback<Boolean> callback);

    @Override
    public native void setSafeBrowsingWhitelist(List<String> hosts, ValueCallback<Boolean> callback);

    @Override
    public native Uri getSafeBrowsingPrivacyPolicyUrl();
}
