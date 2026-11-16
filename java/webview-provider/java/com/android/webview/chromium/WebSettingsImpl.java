// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.webkit.WebSettings;

/**
 * WebView.getSettings() (webview plan S4.1): WebSettings is an abstract class, which JNI cannot subclass, so this forwarder does. Each property is one native on the view of {@link #mProvider}: the core stores it with AOSP's default and returns the stored value, and pushes the ones WebKit has (atl-webview settings.rs). Enums cross as their AOSP value (ZoomDensity.getValue) or ordinal.
 */
final class WebSettingsImpl extends WebSettings {
    // read by the natives: the view these settings belong to
    private final WebViewProviderImpl mProvider;

    WebSettingsImpl(WebViewProviderImpl provider) {
        mProvider = provider;
    }

    @Override public native void setJavaScriptEnabled(boolean flag);
    @Override public native boolean getJavaScriptEnabled();
    @Override public native void setDomStorageEnabled(boolean flag);
    @Override public native boolean getDomStorageEnabled();
    @Override public native void setUserAgentString(String ua);
    @Override public native String getUserAgentString();
    @Override public native void setNavDump(boolean enabled);
    @Override public native boolean getNavDump();
    @Override public native void setSupportZoom(boolean support);
    @Override public native boolean supportZoom();
    @Override public native void setMediaPlaybackRequiresUserGesture(boolean require);
    @Override public native boolean getMediaPlaybackRequiresUserGesture();
    @Override public native void setBuiltInZoomControls(boolean enabled);
    @Override public native boolean getBuiltInZoomControls();
    @Override public native void setDisplayZoomControls(boolean enabled);
    @Override public native boolean getDisplayZoomControls();
    @Override public native void setAllowFileAccess(boolean allow);
    @Override public native boolean getAllowFileAccess();
    @Override public native void setAllowContentAccess(boolean allow);
    @Override public native boolean getAllowContentAccess();
    @Override public native void setLoadWithOverviewMode(boolean overview);
    @Override public native boolean getLoadWithOverviewMode();
    @Override public native void setEnableSmoothTransition(boolean enable);
    @Override public native boolean enableSmoothTransition();
    @Override public native void setUseWebViewBackgroundForOverscrollBackground(boolean view);
    @Override public native boolean getUseWebViewBackgroundForOverscrollBackground();
    @Override public native void setSaveFormData(boolean save);
    @Override public native boolean getSaveFormData();
    @Override public native void setSavePassword(boolean save);
    @Override public native boolean getSavePassword();
    @Override public native void setTextZoom(int textZoom);
    @Override public native int getTextZoom();
    // with the CookieManager (webview plan W4b): still refused
    @Override public void setAcceptThirdPartyCookies(boolean accept) { throw Unimplemented.refuse("WebSettings.setAcceptThirdPartyCookies"); }
    @Override public boolean getAcceptThirdPartyCookies() { throw Unimplemented.refuse("WebSettings.getAcceptThirdPartyCookies"); }
    @Override public void setDefaultZoom(ZoomDensity zoom) { setDefaultZoomValue(zoom.getValue()); }
    @Override public ZoomDensity getDefaultZoom() { return zoomOf(getDefaultZoomValue()); }
    @Override public native void setLightTouchEnabled(boolean enabled);
    @Override public native boolean getLightTouchEnabled();
    @Override public native void setUserAgent(int ua);
    @Override public native int getUserAgent();
    @Override public native void setUseWideViewPort(boolean use);
    @Override public native boolean getUseWideViewPort();
    @Override public native void setSupportMultipleWindows(boolean support);
    @Override public native boolean supportMultipleWindows();
    @Override public void setLayoutAlgorithm(LayoutAlgorithm l) { setLayoutAlgorithmValue(l.ordinal()); }
    @Override public LayoutAlgorithm getLayoutAlgorithm() { return LayoutAlgorithm.values()[getLayoutAlgorithmValue()]; }
    @Override public native void setStandardFontFamily(String font);
    @Override public native String getStandardFontFamily();
    @Override public native void setFixedFontFamily(String font);
    @Override public native String getFixedFontFamily();
    @Override public native void setSansSerifFontFamily(String font);
    @Override public native String getSansSerifFontFamily();
    @Override public native void setSerifFontFamily(String font);
    @Override public native String getSerifFontFamily();
    @Override public native void setCursiveFontFamily(String font);
    @Override public native String getCursiveFontFamily();
    @Override public native void setFantasyFontFamily(String font);
    @Override public native String getFantasyFontFamily();
    @Override public native void setMinimumFontSize(int size);
    @Override public native int getMinimumFontSize();
    @Override public native void setMinimumLogicalFontSize(int size);
    @Override public native int getMinimumLogicalFontSize();
    @Override public native void setDefaultFontSize(int size);
    @Override public native int getDefaultFontSize();
    @Override public native void setDefaultFixedFontSize(int size);
    @Override public native int getDefaultFixedFontSize();
    @Override public native void setLoadsImagesAutomatically(boolean flag);
    @Override public native boolean getLoadsImagesAutomatically();
    @Override public native void setBlockNetworkImage(boolean flag);
    @Override public native boolean getBlockNetworkImage();
    @Override public native void setBlockNetworkLoads(boolean flag);
    @Override public native boolean getBlockNetworkLoads();
    @Override public native void setAllowUniversalAccessFromFileURLs(boolean flag);
    @Override public native boolean getAllowUniversalAccessFromFileURLs();
    @Override public native void setAllowFileAccessFromFileURLs(boolean flag);
    @Override public native boolean getAllowFileAccessFromFileURLs();
    @Override public native void setPluginsEnabled(boolean flag);
    @Override public native boolean getPluginsEnabled();
    @Override public void setPluginState(PluginState state) { setPluginStateValue(state.ordinal()); }
    @Override public PluginState getPluginState() { return PluginState.values()[getPluginStateValue()]; }
    // WebView.getDatabasePath: a path the implementation manages (WebSettings.java:1151-1160 says a set has no effect)
    @Override public void setDatabasePath(String databasePath) {}
    @Override public native String getDatabasePath();
    @Override public void setGeolocationDatabasePath(String databasePath) {}
    @Override public native void setDatabaseEnabled(boolean flag);
    @Override public native boolean getDatabaseEnabled();
    @Override public native void setGeolocationEnabled(boolean flag);
    @Override public native void setJavaScriptCanOpenWindowsAutomatically(boolean flag);
    @Override public native boolean getJavaScriptCanOpenWindowsAutomatically();
    @Override public native void setDefaultTextEncodingName(String encoding);
    @Override public native String getDefaultTextEncodingName();
    @Override public native void setNeedInitialFocus(boolean flag);
    @Override public void setRenderPriority(RenderPriority priority) { setRenderPriorityValue(priority.ordinal()); }
    @Override public native void setCacheMode(int mode);
    @Override public native int getCacheMode();
    @Override public native void setMixedContentMode(int mode);
    @Override public native int getMixedContentMode();
    @Override public native void setVideoOverlayForEmbeddedEncryptedVideoEnabled(boolean flag);
    @Override public native boolean getVideoOverlayForEmbeddedEncryptedVideoEnabled();
    @Override public native void setOffscreenPreRaster(boolean enabled);
    @Override public native boolean getOffscreenPreRaster();
    @Override public native void setSafeBrowsingEnabled(boolean enabled);
    @Override public native boolean getSafeBrowsingEnabled();
    @Override public native void setDisabledActionModeMenuItems(int menuItems);
    @Override public native int getDisabledActionModeMenuItems();

    // the enum-typed properties' values (the core stores ints)
    private native void setDefaultZoomValue(int value);
    private native int getDefaultZoomValue();
    private native void setLayoutAlgorithmValue(int ordinal);
    private native int getLayoutAlgorithmValue();
    private native void setPluginStateValue(int ordinal);
    private native int getPluginStateValue();
    private native void setRenderPriorityValue(int ordinal);

    // the ZoomDensity whose AOSP value is `value` (WebSettings.java:112-124)
    private static ZoomDensity zoomOf(int value) {
        for (ZoomDensity zoom : ZoomDensity.values()) {
            if (zoom.getValue() == value) {
                return zoom;
            }
        }
        throw new IllegalStateException("ZoomDensity " + value);
    }
}
