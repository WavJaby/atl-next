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
 * WebViewLibrary); what is not built yet throws Unimplemented.refuse("WebViewFactoryProvider.<method>"), counted natively. createWebView refuses the same way when the engine cannot start.
 * getPacProcessor / createPacProcessor keep the interface's defaults (UnsupportedOperationException), as AOSP's own provider contract states.
 */
public final class WebViewChromiumFactoryProviderForT implements WebViewFactoryProvider {
    // the provider's natives: a platform library on the runtime library path (Soong's /system/lib64 role), not in the APK
    static {
        System.loadLibrary("atl_webview");
    }

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

    @Override public GeolocationPermissions getGeolocationPermissions() { throw Unimplemented.refuse("WebViewFactoryProvider.getGeolocationPermissions"); }
    @Override public native CookieManager getCookieManager();
    @Override public TokenBindingService getTokenBindingService() { throw Unimplemented.refuse("WebViewFactoryProvider.getTokenBindingService"); }
    @Override public TracingController getTracingController() { throw Unimplemented.refuse("WebViewFactoryProvider.getTracingController"); }
    @Override public ServiceWorkerController getServiceWorkerController() { throw Unimplemented.refuse("WebViewFactoryProvider.getServiceWorkerController"); }
    @Override public WebIconDatabase getWebIconDatabase() { throw Unimplemented.refuse("WebViewFactoryProvider.getWebIconDatabase"); }
    @Override public WebStorage getWebStorage() { throw Unimplemented.refuse("WebViewFactoryProvider.getWebStorage"); }
    @Override public WebViewDatabase getWebViewDatabase(Context context) { throw Unimplemented.refuse("WebViewFactoryProvider.getWebViewDatabase"); }
    @Override public ClassLoader getWebViewClassLoader() { throw Unimplemented.refuse("WebViewFactoryProvider.getWebViewClassLoader"); }
}
