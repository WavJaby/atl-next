// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.webkit.CookieManager;
import android.webkit.ValueCallback;
import android.webkit.WebView;

/**
 * The process's CookieManager (WebViewFactoryProvider.getCookieManager; one instance, made by the native getCookieManager). A forwarder (AGENTS Languages): libatl_webview.so keeps the cookie
 * copy (webview plan D5), so every read answers on the calling thread without waiting for the engine. The deprecated and privateBrowsing overloads forward to their current forms, as the
 * javadoc defines them.
 */
final class CookieManagerImpl extends CookieManager {
    // called by the native getCookieManager, on any thread: the main Looper's poster is handed to the natives from the main thread (a Looper-less caller's callbacks go there, CookieReply)
    private CookieManagerImpl() {
        new android.os.Handler(android.os.Looper.getMainLooper()).post(new Runnable() {
            @Override public void run() { onMainLooper(); }
        });
    }

    private static native void onMainLooper();

    @Override public native void setAcceptCookie(boolean accept);
    @Override public native boolean acceptCookie();
    @Override public native void setAcceptThirdPartyCookies(WebView webview, boolean accept);
    @Override public native boolean acceptThirdPartyCookies(WebView webview);
    @Override public void setCookie(String url, String value) { setCookieNative(url, value, null); }
    @Override public void setCookie(String url, String value, ValueCallback<Boolean> callback) { setCookieNative(url, value, CookieReply.of(callback)); }
    @Override public native String getCookie(String url);
    @Override public String getCookie(String url, boolean privateBrowsing) { return getCookie(url); }
    @Override public void removeSessionCookie() { removeNative(false, null); }
    @Override public void removeSessionCookies(ValueCallback<Boolean> callback) { removeNative(false, CookieReply.of(callback)); }
    @Override public void removeAllCookie() { removeNative(true, null); }
    @Override public void removeAllCookies(ValueCallback<Boolean> callback) { removeNative(true, CookieReply.of(callback)); }
    @Override public native boolean hasCookies();
    @Override public boolean hasCookies(boolean privateBrowsing) { return hasCookies(); }
    @Override public native void removeExpiredCookie();
    // the engine writes its store itself (WebKit's persistent cookie storage): nothing is held back
    @Override public void flush() {}
    @Override protected native boolean allowFileSchemeCookiesImpl();
    @Override protected native void setAcceptFileSchemeCookiesImpl(boolean accept);

    private static native void setCookieNative(String url, String value, CookieReply reply);
    private static native void removeNative(boolean all, CookieReply reply);
}
