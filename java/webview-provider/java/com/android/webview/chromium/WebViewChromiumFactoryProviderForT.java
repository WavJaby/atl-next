// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.content.Context;
import android.webkit.CookieManager;
import android.webkit.GeolocationPermissions;
import android.webkit.ServiceWorkerController;
import android.webkit.TokenBindingService;
import android.webkit.TracingController;
import android.webkit.WebIconDatabase;
import android.webkit.WebStorage;
import android.webkit.WebView;
import android.webkit.WebViewDatabase;
import android.webkit.WebViewDelegate;
import android.webkit.WebViewFactoryProvider;
import android.webkit.WebViewProvider;

/**
 * The provider WebViewFactory instantiates (WebViewFactoryProvider.getWebViewFactoryClassName without use_b_entry_point; static create(WebViewDelegate), WebViewFactory.java:355-360).
 * A forwarder (AGENTS Languages): WebViewFactory needs an object implementing WebViewFactoryProvider, which JNI cannot create. Every method forwards to libatl_webview.so (the manifest's
 * WebViewLibrary); the native side throws UnsupportedOperationException("WebViewFactoryProvider.<method>") for what is not built yet and counts it (unimplemented_api).
 * getPacProcessor / createPacProcessor keep the interface's defaults (UnsupportedOperationException), as AOSP's own provider contract states.
 */
public final class WebViewChromiumFactoryProviderForT implements WebViewFactoryProvider {
    private final WebViewDelegate mDelegate;

    private WebViewChromiumFactoryProviderForT(WebViewDelegate delegate) {
        mDelegate = delegate;
    }

    /** WebViewFactory's entry point (CHROMIUM_WEBVIEW_FACTORY_METHOD). */
    public static WebViewChromiumFactoryProviderForT create(WebViewDelegate delegate) {
        return new WebViewChromiumFactoryProviderForT(delegate);
    }

    @Override
    public native Statics getStatics();

    @Override
    public native WebViewProvider createWebView(WebView webView, WebView.PrivateAccess privateAccess);

    @Override
    public native GeolocationPermissions getGeolocationPermissions();

    @Override
    public native CookieManager getCookieManager();

    @Override
    public native TokenBindingService getTokenBindingService();

    @Override
    public native TracingController getTracingController();

    @Override
    public native ServiceWorkerController getServiceWorkerController();

    @Override
    public native WebIconDatabase getWebIconDatabase();

    @Override
    public native WebStorage getWebStorage();

    @Override
    public native WebViewDatabase getWebViewDatabase(Context context);

    @Override
    public native ClassLoader getWebViewClassLoader();
}
