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
 * getDefaultUserAgent is real (webview plan S5); the rest answer the contract's default / failure value, counted (Unimplemented), or refuse (class C).
 */
final class WebViewChromiumStatics implements WebViewFactoryProvider.Statics {
    @Override public native String getDefaultUserAgent(Context context);

    // "null if no address" (no address detector)
    @Override public String findAddress(String addr) { Unimplemented.accept("WebViewFactoryProvider.Statics.findAddress"); return null; }
    @Override public void freeMemoryForTests() { Unimplemented.accept("WebViewFactoryProvider.Statics.freeMemoryForTests"); }
    @Override public void setWebContentsDebuggingEnabled(boolean enable) { Unimplemented.accept("WebViewFactoryProvider.Statics.setWebContentsDebuggingEnabled"); }
    // a no-op never runs onCleared: class C until a native runs it
    @Override public void clearClientCertPreferences(Runnable onCleared) { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.clearClientCertPreferences"); }
    @Override public void enableSlowWholeDocumentDraw() { Unimplemented.accept("WebViewFactoryProvider.Statics.enableSlowWholeDocumentDraw"); }
    // "null if the result is invalid" (no file chooser)
    @Override public Uri[] parseFileChooserResult(int resultCode, Intent intent) { Unimplemented.accept("WebViewFactoryProvider.Statics.parseFileChooserResult"); return null; }
    // "false otherwise": no Safe Browsing
    @Override public void initSafeBrowsing(Context context, ValueCallback<Boolean> callback) { Unimplemented.answer("WebViewFactoryProvider.Statics.initSafeBrowsing", callback, Boolean.FALSE); }
    // the failure value: hosts not added
    @Override public void setSafeBrowsingWhitelist(List<String> hosts, ValueCallback<Boolean> callback) { Unimplemented.answer("WebViewFactoryProvider.Statics.setSafeBrowsingWhitelist", callback, Boolean.FALSE); }
    // @NonNull; no Safe Browsing -> no policy to name
    @Override public Uri getSafeBrowsingPrivacyPolicyUrl() { throw Unimplemented.refuse("WebViewFactoryProvider.Statics.getSafeBrowsingPrivacyPolicyUrl"); }
}
