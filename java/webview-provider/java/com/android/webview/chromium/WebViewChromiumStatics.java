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
 * getDefaultUserAgent is real (webview plan S5); the rest throw Unimplemented.refuse("WebViewFactoryProvider.Statics.<method>").
 */
final class WebViewChromiumStatics implements WebViewFactoryProvider.Statics {
    @Override public native String getDefaultUserAgent(Context context);

    @Override public String findAddress(String addr) { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.findAddress"); }
    @Override public void freeMemoryForTests() { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.freeMemoryForTests"); }
    @Override public void setWebContentsDebuggingEnabled(boolean enable) { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.setWebContentsDebuggingEnabled"); }
    @Override public void clearClientCertPreferences(Runnable onCleared) { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.clearClientCertPreferences"); }
    @Override public void enableSlowWholeDocumentDraw() { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.enableSlowWholeDocumentDraw"); }
    @Override public Uri[] parseFileChooserResult(int resultCode, Intent intent) { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.parseFileChooserResult"); }
    @Override public void initSafeBrowsing(Context context, ValueCallback<Boolean> callback) { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.initSafeBrowsing"); }
    @Override public void setSafeBrowsingWhitelist(List<String> hosts, ValueCallback<Boolean> callback) { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.setSafeBrowsingWhitelist"); }
    @Override public Uri getSafeBrowsingPrivacyPolicyUrl() { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.getSafeBrowsingPrivacyPolicyUrl"); }
}
