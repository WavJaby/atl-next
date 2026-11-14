// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.webkit.WebSettings;

/**
 * WebView.getSettings() (webview plan S4.1): WebSettings is an abstract class, which JNI cannot subclass, so this forwarder does. The natives act on the view of {@link #mProvider}.
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

    @Override public void setNavDump(boolean enabled) { throw Unimplemented.refuse("WebSettings.setNavDump"); }
    @Override public boolean getNavDump() { throw Unimplemented.refuse("WebSettings.getNavDump"); }
    @Override public void setSupportZoom(boolean support) { throw Unimplemented.refuse("WebSettings.setSupportZoom"); }
    @Override public boolean supportZoom() { throw Unimplemented.refuse("WebSettings.supportZoom"); }
    @Override public void setMediaPlaybackRequiresUserGesture(boolean require) { throw Unimplemented.refuse("WebSettings.setMediaPlaybackRequiresUserGesture"); }
    @Override public boolean getMediaPlaybackRequiresUserGesture() { throw Unimplemented.refuse("WebSettings.getMediaPlaybackRequiresUserGesture"); }
    @Override public void setBuiltInZoomControls(boolean enabled) { throw Unimplemented.refuse("WebSettings.setBuiltInZoomControls"); }
    @Override public boolean getBuiltInZoomControls() { throw Unimplemented.refuse("WebSettings.getBuiltInZoomControls"); }
    @Override public void setDisplayZoomControls(boolean enabled) { throw Unimplemented.refuse("WebSettings.setDisplayZoomControls"); }
    @Override public boolean getDisplayZoomControls() { throw Unimplemented.refuse("WebSettings.getDisplayZoomControls"); }
    @Override public void setAllowFileAccess(boolean allow) { throw Unimplemented.refuse("WebSettings.setAllowFileAccess"); }
    @Override public boolean getAllowFileAccess() { throw Unimplemented.refuse("WebSettings.getAllowFileAccess"); }
    @Override public void setAllowContentAccess(boolean allow) { throw Unimplemented.refuse("WebSettings.setAllowContentAccess"); }
    @Override public boolean getAllowContentAccess() { throw Unimplemented.refuse("WebSettings.getAllowContentAccess"); }
    @Override public void setLoadWithOverviewMode(boolean overview) { throw Unimplemented.refuse("WebSettings.setLoadWithOverviewMode"); }
    @Override public boolean getLoadWithOverviewMode() { throw Unimplemented.refuse("WebSettings.getLoadWithOverviewMode"); }
    @Override public void setEnableSmoothTransition(boolean enable) { throw Unimplemented.refuse("WebSettings.setEnableSmoothTransition"); }
    @Override public boolean enableSmoothTransition() { throw Unimplemented.refuse("WebSettings.enableSmoothTransition"); }
    @Override public void setUseWebViewBackgroundForOverscrollBackground(boolean view) { throw Unimplemented.refuse("WebSettings.setUseWebViewBackgroundForOverscrollBackground"); }
    @Override public boolean getUseWebViewBackgroundForOverscrollBackground() { throw Unimplemented.refuse("WebSettings.getUseWebViewBackgroundForOverscrollBackground"); }
    @Override public void setSaveFormData(boolean save) { throw Unimplemented.refuse("WebSettings.setSaveFormData"); }
    @Override public boolean getSaveFormData() { throw Unimplemented.refuse("WebSettings.getSaveFormData"); }
    @Override public void setSavePassword(boolean save) { throw Unimplemented.refuse("WebSettings.setSavePassword"); }
    @Override public boolean getSavePassword() { throw Unimplemented.refuse("WebSettings.getSavePassword"); }
    @Override public void setTextZoom(int textZoom) { throw Unimplemented.refuse("WebSettings.setTextZoom"); }
    @Override public int getTextZoom() { throw Unimplemented.refuse("WebSettings.getTextZoom"); }
    @Override public void setAcceptThirdPartyCookies(boolean accept) { throw Unimplemented.refuse("WebSettings.setAcceptThirdPartyCookies"); }
    @Override public boolean getAcceptThirdPartyCookies() { throw Unimplemented.refuse("WebSettings.getAcceptThirdPartyCookies"); }
    @Override public void setDefaultZoom(ZoomDensity zoom) { throw Unimplemented.refuse("WebSettings.setDefaultZoom"); }
    @Override public ZoomDensity getDefaultZoom() { throw Unimplemented.refuse("WebSettings.getDefaultZoom"); }
    @Override public void setLightTouchEnabled(boolean enabled) { throw Unimplemented.refuse("WebSettings.setLightTouchEnabled"); }
    @Override public boolean getLightTouchEnabled() { throw Unimplemented.refuse("WebSettings.getLightTouchEnabled"); }
    @Override public void setUserAgent(int ua) { throw Unimplemented.refuse("WebSettings.setUserAgent"); }
    @Override public int getUserAgent() { throw Unimplemented.refuse("WebSettings.getUserAgent"); }
    @Override public void setUseWideViewPort(boolean use) { throw Unimplemented.refuse("WebSettings.setUseWideViewPort"); }
    @Override public boolean getUseWideViewPort() { throw Unimplemented.refuse("WebSettings.getUseWideViewPort"); }
    @Override public void setSupportMultipleWindows(boolean support) { throw Unimplemented.refuse("WebSettings.setSupportMultipleWindows"); }
    @Override public boolean supportMultipleWindows() { throw Unimplemented.refuse("WebSettings.supportMultipleWindows"); }
    @Override public void setLayoutAlgorithm(LayoutAlgorithm l) { throw Unimplemented.refuse("WebSettings.setLayoutAlgorithm"); }
    @Override public LayoutAlgorithm getLayoutAlgorithm() { throw Unimplemented.refuse("WebSettings.getLayoutAlgorithm"); }
    @Override public void setStandardFontFamily(String font) { throw Unimplemented.refuse("WebSettings.setStandardFontFamily"); }
    @Override public String getStandardFontFamily() { throw Unimplemented.refuse("WebSettings.getStandardFontFamily"); }
    @Override public void setFixedFontFamily(String font) { throw Unimplemented.refuse("WebSettings.setFixedFontFamily"); }
    @Override public String getFixedFontFamily() { throw Unimplemented.refuse("WebSettings.getFixedFontFamily"); }
    @Override public void setSansSerifFontFamily(String font) { throw Unimplemented.refuse("WebSettings.setSansSerifFontFamily"); }
    @Override public String getSansSerifFontFamily() { throw Unimplemented.refuse("WebSettings.getSansSerifFontFamily"); }
    @Override public void setSerifFontFamily(String font) { throw Unimplemented.refuse("WebSettings.setSerifFontFamily"); }
    @Override public String getSerifFontFamily() { throw Unimplemented.refuse("WebSettings.getSerifFontFamily"); }
    @Override public void setCursiveFontFamily(String font) { throw Unimplemented.refuse("WebSettings.setCursiveFontFamily"); }
    @Override public String getCursiveFontFamily() { throw Unimplemented.refuse("WebSettings.getCursiveFontFamily"); }
    @Override public void setFantasyFontFamily(String font) { throw Unimplemented.refuse("WebSettings.setFantasyFontFamily"); }
    @Override public String getFantasyFontFamily() { throw Unimplemented.refuse("WebSettings.getFantasyFontFamily"); }
    @Override public void setMinimumFontSize(int size) { throw Unimplemented.refuse("WebSettings.setMinimumFontSize"); }
    @Override public int getMinimumFontSize() { throw Unimplemented.refuse("WebSettings.getMinimumFontSize"); }
    @Override public void setMinimumLogicalFontSize(int size) { throw Unimplemented.refuse("WebSettings.setMinimumLogicalFontSize"); }
    @Override public int getMinimumLogicalFontSize() { throw Unimplemented.refuse("WebSettings.getMinimumLogicalFontSize"); }
    @Override public void setDefaultFontSize(int size) { throw Unimplemented.refuse("WebSettings.setDefaultFontSize"); }
    @Override public int getDefaultFontSize() { throw Unimplemented.refuse("WebSettings.getDefaultFontSize"); }
    @Override public void setDefaultFixedFontSize(int size) { throw Unimplemented.refuse("WebSettings.setDefaultFixedFontSize"); }
    @Override public int getDefaultFixedFontSize() { throw Unimplemented.refuse("WebSettings.getDefaultFixedFontSize"); }
    @Override public void setLoadsImagesAutomatically(boolean flag) { throw Unimplemented.refuse("WebSettings.setLoadsImagesAutomatically"); }
    @Override public boolean getLoadsImagesAutomatically() { throw Unimplemented.refuse("WebSettings.getLoadsImagesAutomatically"); }
    @Override public void setBlockNetworkImage(boolean flag) { throw Unimplemented.refuse("WebSettings.setBlockNetworkImage"); }
    @Override public boolean getBlockNetworkImage() { throw Unimplemented.refuse("WebSettings.getBlockNetworkImage"); }
    @Override public void setBlockNetworkLoads(boolean flag) { throw Unimplemented.refuse("WebSettings.setBlockNetworkLoads"); }
    @Override public boolean getBlockNetworkLoads() { throw Unimplemented.refuse("WebSettings.getBlockNetworkLoads"); }
    @Override public void setAllowUniversalAccessFromFileURLs(boolean flag) { throw Unimplemented.refuse("WebSettings.setAllowUniversalAccessFromFileURLs"); }
    @Override public void setAllowFileAccessFromFileURLs(boolean flag) { throw Unimplemented.refuse("WebSettings.setAllowFileAccessFromFileURLs"); }
    @Override public void setPluginsEnabled(boolean flag) { throw Unimplemented.refuse("WebSettings.setPluginsEnabled"); }
    @Override public void setPluginState(PluginState state) { throw Unimplemented.refuse("WebSettings.setPluginState"); }
    @Override public void setDatabasePath(String databasePath) { throw Unimplemented.refuse("WebSettings.setDatabasePath"); }
    @Override public void setGeolocationDatabasePath(String databasePath) { throw Unimplemented.refuse("WebSettings.setGeolocationDatabasePath"); }
    @Override public void setDatabaseEnabled(boolean flag) { throw Unimplemented.refuse("WebSettings.setDatabaseEnabled"); }
    @Override public String getDatabasePath() { throw Unimplemented.refuse("WebSettings.getDatabasePath"); }
    @Override public boolean getDatabaseEnabled() { throw Unimplemented.refuse("WebSettings.getDatabaseEnabled"); }
    @Override public void setGeolocationEnabled(boolean flag) { throw Unimplemented.refuse("WebSettings.setGeolocationEnabled"); }
    @Override public boolean getAllowUniversalAccessFromFileURLs() { throw Unimplemented.refuse("WebSettings.getAllowUniversalAccessFromFileURLs"); }
    @Override public boolean getAllowFileAccessFromFileURLs() { throw Unimplemented.refuse("WebSettings.getAllowFileAccessFromFileURLs"); }
    @Override public boolean getPluginsEnabled() { throw Unimplemented.refuse("WebSettings.getPluginsEnabled"); }
    @Override public PluginState getPluginState() { throw Unimplemented.refuse("WebSettings.getPluginState"); }
    @Override public void setJavaScriptCanOpenWindowsAutomatically(boolean flag) { throw Unimplemented.refuse("WebSettings.setJavaScriptCanOpenWindowsAutomatically"); }
    @Override public boolean getJavaScriptCanOpenWindowsAutomatically() { throw Unimplemented.refuse("WebSettings.getJavaScriptCanOpenWindowsAutomatically"); }
    @Override public void setDefaultTextEncodingName(String encoding) { throw Unimplemented.refuse("WebSettings.setDefaultTextEncodingName"); }
    @Override public String getDefaultTextEncodingName() { throw Unimplemented.refuse("WebSettings.getDefaultTextEncodingName"); }
    @Override public void setNeedInitialFocus(boolean flag) { throw Unimplemented.refuse("WebSettings.setNeedInitialFocus"); }
    @Override public void setRenderPriority(RenderPriority priority) { throw Unimplemented.refuse("WebSettings.setRenderPriority"); }
    @Override public void setCacheMode(int mode) { throw Unimplemented.refuse("WebSettings.setCacheMode"); }
    @Override public int getCacheMode() { throw Unimplemented.refuse("WebSettings.getCacheMode"); }
    @Override public void setMixedContentMode(int mode) { throw Unimplemented.refuse("WebSettings.setMixedContentMode"); }
    @Override public int getMixedContentMode() { throw Unimplemented.refuse("WebSettings.getMixedContentMode"); }
    @Override public void setVideoOverlayForEmbeddedEncryptedVideoEnabled(boolean flag) { throw Unimplemented.refuse("WebSettings.setVideoOverlayForEmbeddedEncryptedVideoEnabled"); }
    @Override public boolean getVideoOverlayForEmbeddedEncryptedVideoEnabled() { throw Unimplemented.refuse("WebSettings.getVideoOverlayForEmbeddedEncryptedVideoEnabled"); }
    @Override public void setOffscreenPreRaster(boolean enabled) { throw Unimplemented.refuse("WebSettings.setOffscreenPreRaster"); }
    @Override public boolean getOffscreenPreRaster() { throw Unimplemented.refuse("WebSettings.getOffscreenPreRaster"); }
    @Override public void setSafeBrowsingEnabled(boolean enabled) { throw Unimplemented.refuse("WebSettings.setSafeBrowsingEnabled"); }
    @Override public boolean getSafeBrowsingEnabled() { throw Unimplemented.refuse("WebSettings.getSafeBrowsingEnabled"); }
    @Override public void setDisabledActionModeMenuItems(int menuItems) { throw Unimplemented.refuse("WebSettings.setDisabledActionModeMenuItems"); }
    @Override public int getDisabledActionModeMenuItems() { throw Unimplemented.refuse("WebSettings.getDisabledActionModeMenuItems"); }
}
