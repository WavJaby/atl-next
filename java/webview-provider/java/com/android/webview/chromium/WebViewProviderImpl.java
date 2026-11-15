// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.content.Intent;
import android.content.res.Configuration;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.Picture;
import android.graphics.Rect;
import android.net.Uri;
import android.net.http.SslCertificate;
import android.os.Bundle;
import android.os.Handler;
import android.os.Message;
import android.print.PrintDocumentAdapter;
import android.view.DragEvent;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewGroup.LayoutParams;
import android.view.accessibility.AccessibilityEvent;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityNodeProvider;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.webkit.DownloadListener;
import android.webkit.ValueCallback;
import android.webkit.WebBackForwardList;
import android.webkit.WebChromeClient;
import android.webkit.WebMessage;
import android.webkit.WebMessagePort;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebView.HitTestResult;
import android.webkit.WebView.PictureListener;
import android.webkit.WebView.VisualStateCallback;
import android.webkit.WebViewClient;
import android.webkit.WebViewProvider;
import android.webkit.WebViewRenderProcess;
import android.webkit.WebViewRenderProcessClient;

import java.io.BufferedWriter;
import java.io.File;
import java.util.Map;
import java.util.concurrent.Executor;

/**
 * The provider of one WebView (WebViewFactoryProvider.createWebView, webview plan S4.1), also its ViewDelegate and ScrollDelegate. A forwarder (AGENTS Languages): every method is one
 * native call into libatl_webview.so, which keeps the page view behind {@link #mNative}; unimplemented methods throw Unimplemented.refuse("<Interface>.<method>").
 * The clients are stored here as handed in (the natives read them for each callback); getWebViewClient's default instance is WebView's documented "default client if not yet set".
 */
final class WebViewProviderImpl implements WebViewProvider, WebViewProvider.ViewDelegate, WebViewProvider.ScrollDelegate {
    // read and cleared by the natives: the native view, 0 after destroy
    private long mNative;
    // read by the natives: the WebView passed to every client callback
    private final WebView mWebView;
    private final WebView.PrivateAccess mWebViewPrivate;
    private final WebSettingsImpl mSettings;
    private WebViewClient mWebViewClient = new WebViewClient();
    private WebChromeClient mWebChromeClient;

    // called by the native createWebView, which made the view
    private WebViewProviderImpl(long nativeView, WebView webView, WebView.PrivateAccess privateAccess) {
        mNative = nativeView;
        mWebView = webView;
        mWebViewPrivate = privateAccess;
        mSettings = new WebSettingsImpl(this);
    }

    // ---- implemented (W2) ----
    @Override public native void init(Map<String, Object> javaScriptInterfaces, boolean privateBrowsing);
    @Override public native void destroy();
    @Override public native void loadUrl(String url, Map<String, String> additionalHttpHeaders);
    @Override public void loadUrl(String url) { loadUrl(url, null); }
    @Override public native void loadData(String data, String mimeType, String encoding);
    @Override public native void loadDataWithBaseURL(String baseUrl, String data, String mimeType, String encoding, String historyUrl);
    @Override public native void evaluateJavaScript(String script, ValueCallback<String> resultCallback);
    @Override public native void stopLoading();
    @Override public native void reload();
    @Override public native String getUrl();
    @Override public native String getTitle();
    @Override public native int getProgress();
    @Override public native void addJavascriptInterface(Object obj, String interfaceName);
    @Override public native void removeJavascriptInterface(String interfaceName);
    @Override public WebSettings getSettings() { return mSettings; }
    @Override public void setWebViewClient(WebViewClient client) { mWebViewClient = client; }
    @Override public WebViewClient getWebViewClient() { return mWebViewClient; }
    @Override public void setWebChromeClient(WebChromeClient client) { mWebChromeClient = client; }
    @Override public WebChromeClient getWebChromeClient() { return mWebChromeClient; }
    @Override public ViewDelegate getViewDelegate() { return this; }
    @Override public ScrollDelegate getScrollDelegate() { return this; }

    // ---- View-system callbacks on every WebView: counted, not thrown (Unimplemented.accept) ----
    @Override public void setOverScrollMode(int mode) { Unimplemented.accept("WebViewProvider.ViewDelegate.setOverScrollMode"); }
    @Override public void setScrollBarStyle(int style) { Unimplemented.accept("WebViewProvider.ViewDelegate.setScrollBarStyle"); }

    // ---- WebViewProvider, not yet ----
    @Override public void setHorizontalScrollbarOverlay(boolean overlay) { throw Unimplemented.refuse("WebViewProvider.setHorizontalScrollbarOverlay"); }
    @Override public void setVerticalScrollbarOverlay(boolean overlay) { throw Unimplemented.refuse("WebViewProvider.setVerticalScrollbarOverlay"); }
    @Override public boolean overlayHorizontalScrollbar() { throw Unimplemented.refuse("WebViewProvider.overlayHorizontalScrollbar"); }
    @Override public boolean overlayVerticalScrollbar() { throw Unimplemented.refuse("WebViewProvider.overlayVerticalScrollbar"); }
    @Override public int getVisibleTitleHeight() { throw Unimplemented.refuse("WebViewProvider.getVisibleTitleHeight"); }
    @Override public SslCertificate getCertificate() { throw Unimplemented.refuse("WebViewProvider.getCertificate"); }
    @Override public void setCertificate(SslCertificate certificate) { throw Unimplemented.refuse("WebViewProvider.setCertificate"); }
    @Override public void savePassword(String host, String username, String password) { throw Unimplemented.refuse("WebViewProvider.savePassword"); }
    @Override public void setHttpAuthUsernamePassword(String host, String realm, String username, String password) { throw Unimplemented.refuse("WebViewProvider.setHttpAuthUsernamePassword"); }
    @Override public String[] getHttpAuthUsernamePassword(String host, String realm) { throw Unimplemented.refuse("WebViewProvider.getHttpAuthUsernamePassword"); }
    @Override public void setNetworkAvailable(boolean networkUp) { throw Unimplemented.refuse("WebViewProvider.setNetworkAvailable"); }
    @Override public WebBackForwardList saveState(Bundle outState) { throw Unimplemented.refuse("WebViewProvider.saveState"); }
    @Override public boolean savePicture(Bundle b, File dest) { throw Unimplemented.refuse("WebViewProvider.savePicture"); }
    @Override public boolean restorePicture(Bundle b, File src) { throw Unimplemented.refuse("WebViewProvider.restorePicture"); }
    @Override public WebBackForwardList restoreState(Bundle inState) { throw Unimplemented.refuse("WebViewProvider.restoreState"); }
    @Override public void postUrl(String url, byte[] postData) { throw Unimplemented.refuse("WebViewProvider.postUrl"); }
    @Override public void saveWebArchive(String filename) { throw Unimplemented.refuse("WebViewProvider.saveWebArchive"); }
    @Override public void saveWebArchive(String basename, boolean autoname, ValueCallback<String> callback) { throw Unimplemented.refuse("WebViewProvider.saveWebArchive"); }
    @Override public boolean canGoBack() { throw Unimplemented.refuse("WebViewProvider.canGoBack"); }
    @Override public void goBack() { throw Unimplemented.refuse("WebViewProvider.goBack"); }
    @Override public boolean canGoForward() { throw Unimplemented.refuse("WebViewProvider.canGoForward"); }
    @Override public void goForward() { throw Unimplemented.refuse("WebViewProvider.goForward"); }
    @Override public boolean canGoBackOrForward(int steps) { throw Unimplemented.refuse("WebViewProvider.canGoBackOrForward"); }
    @Override public void goBackOrForward(int steps) { throw Unimplemented.refuse("WebViewProvider.goBackOrForward"); }
    @Override public boolean isPrivateBrowsingEnabled() { throw Unimplemented.refuse("WebViewProvider.isPrivateBrowsingEnabled"); }
    @Override public boolean pageUp(boolean top) { throw Unimplemented.refuse("WebViewProvider.pageUp"); }
    @Override public boolean pageDown(boolean bottom) { throw Unimplemented.refuse("WebViewProvider.pageDown"); }
    @Override public void insertVisualStateCallback(long requestId, VisualStateCallback callback) { throw Unimplemented.refuse("WebViewProvider.insertVisualStateCallback"); }
    @Override public void clearView() { throw Unimplemented.refuse("WebViewProvider.clearView"); }
    @Override public Picture capturePicture() { throw Unimplemented.refuse("WebViewProvider.capturePicture"); }
    @Override public PrintDocumentAdapter createPrintDocumentAdapter(String documentName) { throw Unimplemented.refuse("WebViewProvider.createPrintDocumentAdapter"); }
    @Override public float getScale() { throw Unimplemented.refuse("WebViewProvider.getScale"); }
    @Override public void setInitialScale(int scaleInPercent) { throw Unimplemented.refuse("WebViewProvider.setInitialScale"); }
    @Override public void invokeZoomPicker() { throw Unimplemented.refuse("WebViewProvider.invokeZoomPicker"); }
    @Override public HitTestResult getHitTestResult() { throw Unimplemented.refuse("WebViewProvider.getHitTestResult"); }
    @Override public void requestFocusNodeHref(Message hrefMsg) { throw Unimplemented.refuse("WebViewProvider.requestFocusNodeHref"); }
    @Override public void requestImageRef(Message msg) { throw Unimplemented.refuse("WebViewProvider.requestImageRef"); }
    @Override public String getOriginalUrl() { throw Unimplemented.refuse("WebViewProvider.getOriginalUrl"); }
    @Override public Bitmap getFavicon() { throw Unimplemented.refuse("WebViewProvider.getFavicon"); }
    @Override public String getTouchIconUrl() { throw Unimplemented.refuse("WebViewProvider.getTouchIconUrl"); }
    @Override public int getContentHeight() { throw Unimplemented.refuse("WebViewProvider.getContentHeight"); }
    @Override public int getContentWidth() { throw Unimplemented.refuse("WebViewProvider.getContentWidth"); }
    @Override public void pauseTimers() { throw Unimplemented.refuse("WebViewProvider.pauseTimers"); }
    @Override public void resumeTimers() { throw Unimplemented.refuse("WebViewProvider.resumeTimers"); }
    @Override public void onPause() { throw Unimplemented.refuse("WebViewProvider.onPause"); }
    @Override public void onResume() { throw Unimplemented.refuse("WebViewProvider.onResume"); }
    @Override public boolean isPaused() { throw Unimplemented.refuse("WebViewProvider.isPaused"); }
    @Override public void freeMemory() { throw Unimplemented.refuse("WebViewProvider.freeMemory"); }
    @Override public void clearCache(boolean includeDiskFiles) { throw Unimplemented.refuse("WebViewProvider.clearCache"); }
    @Override public void clearFormData() { throw Unimplemented.refuse("WebViewProvider.clearFormData"); }
    @Override public void clearHistory() { throw Unimplemented.refuse("WebViewProvider.clearHistory"); }
    @Override public void clearSslPreferences() { throw Unimplemented.refuse("WebViewProvider.clearSslPreferences"); }
    @Override public WebBackForwardList copyBackForwardList() { throw Unimplemented.refuse("WebViewProvider.copyBackForwardList"); }
    @Override public void setFindListener(WebView.FindListener listener) { throw Unimplemented.refuse("WebViewProvider.setFindListener"); }
    @Override public void findNext(boolean forward) { throw Unimplemented.refuse("WebViewProvider.findNext"); }
    @Override public int findAll(String find) { throw Unimplemented.refuse("WebViewProvider.findAll"); }
    @Override public void findAllAsync(String find) { throw Unimplemented.refuse("WebViewProvider.findAllAsync"); }
    @Override public boolean showFindDialog(String text, boolean showIme) { throw Unimplemented.refuse("WebViewProvider.showFindDialog"); }
    @Override public void clearMatches() { throw Unimplemented.refuse("WebViewProvider.clearMatches"); }
    @Override public void documentHasImages(Message response) { throw Unimplemented.refuse("WebViewProvider.documentHasImages"); }
    @Override public WebViewRenderProcess getWebViewRenderProcess() { throw Unimplemented.refuse("WebViewProvider.getWebViewRenderProcess"); }
    @Override public void setWebViewRenderProcessClient(Executor executor, WebViewRenderProcessClient client) { throw Unimplemented.refuse("WebViewProvider.setWebViewRenderProcessClient"); }
    @Override public WebViewRenderProcessClient getWebViewRenderProcessClient() { throw Unimplemented.refuse("WebViewProvider.getWebViewRenderProcessClient"); }
    @Override public void setDownloadListener(DownloadListener listener) { throw Unimplemented.refuse("WebViewProvider.setDownloadListener"); }
    @Override public void setPictureListener(PictureListener listener) { throw Unimplemented.refuse("WebViewProvider.setPictureListener"); }
    @Override public WebMessagePort[] createWebMessageChannel() { throw Unimplemented.refuse("WebViewProvider.createWebMessageChannel"); }
    @Override public void postMessageToMainFrame(WebMessage message, Uri targetOrigin) { throw Unimplemented.refuse("WebViewProvider.postMessageToMainFrame"); }
    @Override public void setMapTrackballToArrowKeys(boolean setMap) { throw Unimplemented.refuse("WebViewProvider.setMapTrackballToArrowKeys"); }
    @Override public void flingScroll(int vx, int vy) { throw Unimplemented.refuse("WebViewProvider.flingScroll"); }
    @Override public View getZoomControls() { throw Unimplemented.refuse("WebViewProvider.getZoomControls"); }
    @Override public boolean canZoomIn() { throw Unimplemented.refuse("WebViewProvider.canZoomIn"); }
    @Override public boolean canZoomOut() { throw Unimplemented.refuse("WebViewProvider.canZoomOut"); }
    @Override public boolean zoomBy(float zoomFactor) { throw Unimplemented.refuse("WebViewProvider.zoomBy"); }
    @Override public boolean zoomIn() { throw Unimplemented.refuse("WebViewProvider.zoomIn"); }
    @Override public boolean zoomOut() { throw Unimplemented.refuse("WebViewProvider.zoomOut"); }
    @Override public void dumpViewHierarchyWithProperties(BufferedWriter out, int level) { throw Unimplemented.refuse("WebViewProvider.dumpViewHierarchyWithProperties"); }
    @Override public View findHierarchyView(String className, int hashCode) { throw Unimplemented.refuse("WebViewProvider.findHierarchyView"); }
    @Override public void setRendererPriorityPolicy(int rendererRequestedPriority, boolean waivedWhenNotVisible) { throw Unimplemented.refuse("WebViewProvider.setRendererPriorityPolicy"); }
    @Override public int getRendererRequestedPriority() { throw Unimplemented.refuse("WebViewProvider.getRendererRequestedPriority"); }
    @Override public boolean getRendererPriorityWaivedWhenNotVisible() { throw Unimplemented.refuse("WebViewProvider.getRendererPriorityWaivedWhenNotVisible"); }
    @Override public void notifyFindDialogDismissed() { throw Unimplemented.refuse("WebViewProvider.notifyFindDialogDismissed"); }

    // ---- ViewDelegate, not yet (composition and input are W3) ----
    @Override public boolean shouldDelayChildPressedState() { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.shouldDelayChildPressedState"); }
    @Override public void onProvideVirtualStructure(android.view.ViewStructure structure) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onProvideVirtualStructure"); }
    @Override public AccessibilityNodeProvider getAccessibilityNodeProvider() { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.getAccessibilityNodeProvider"); }
    @Override public void onInitializeAccessibilityNodeInfo(AccessibilityNodeInfo info) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onInitializeAccessibilityNodeInfo"); }
    @Override public void onInitializeAccessibilityEvent(AccessibilityEvent event) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onInitializeAccessibilityEvent"); }
    @Override public boolean performAccessibilityAction(int action, Bundle arguments) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.performAccessibilityAction"); }
    @Override public void onOverScrolled(int scrollX, int scrollY, boolean clampedX, boolean clampedY) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onOverScrolled"); }
    @Override public void onWindowVisibilityChanged(int visibility) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onWindowVisibilityChanged"); }
    @Override public void onDraw(Canvas canvas) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onDraw"); }
    @Override public void onDrawVerticalScrollBar(Canvas canvas, android.graphics.drawable.Drawable scrollBar, int l, int t, int r, int b) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onDrawVerticalScrollBar"); }
    @Override public void setLayoutParams(LayoutParams layoutParams) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.setLayoutParams"); }
    @Override public boolean performLongClick() { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.performLongClick"); }
    @Override public void onConfigurationChanged(Configuration newConfig) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onConfigurationChanged"); }
    @Override public InputConnection onCreateInputConnection(EditorInfo outAttrs) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onCreateInputConnection"); }
    @Override public boolean onDragEvent(DragEvent event) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onDragEvent"); }
    @Override public boolean onKeyMultiple(int keyCode, int repeatCount, KeyEvent event) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onKeyMultiple"); }
    @Override public boolean onKeyDown(int keyCode, KeyEvent event) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onKeyDown"); }
    @Override public boolean onKeyUp(int keyCode, KeyEvent event) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onKeyUp"); }
    @Override public void onAttachedToWindow() { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onAttachedToWindow"); }
    @Override public void onDetachedFromWindow() { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onDetachedFromWindow"); }
    @Override public void onVisibilityChanged(View changedView, int visibility) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onVisibilityChanged"); }
    @Override public void onWindowFocusChanged(boolean hasWindowFocus) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onWindowFocusChanged"); }
    @Override public void onFocusChanged(boolean focused, int direction, Rect previouslyFocusedRect) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onFocusChanged"); }
    @Override public boolean setFrame(int left, int top, int right, int bottom) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.setFrame"); }
    @Override public void onSizeChanged(int w, int h, int ow, int oh) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onSizeChanged"); }
    @Override public void onScrollChanged(int l, int t, int oldl, int oldt) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onScrollChanged"); }
    @Override public boolean dispatchKeyEvent(KeyEvent event) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.dispatchKeyEvent"); }
    @Override public boolean onTouchEvent(MotionEvent ev) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onTouchEvent"); }
    @Override public boolean onHoverEvent(MotionEvent event) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onHoverEvent"); }
    @Override public boolean onGenericMotionEvent(MotionEvent event) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onGenericMotionEvent"); }
    @Override public boolean onTrackballEvent(MotionEvent ev) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onTrackballEvent"); }
    @Override public boolean requestFocus(int direction, Rect previouslyFocusedRect) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.requestFocus"); }
    @Override public void onMeasure(int widthMeasureSpec, int heightMeasureSpec) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onMeasure"); }
    @Override public boolean requestChildRectangleOnScreen(View child, Rect rect, boolean immediate) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.requestChildRectangleOnScreen"); }
    @Override public void setBackgroundColor(int color) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.setBackgroundColor"); }
    @Override public void setLayerType(int layerType, Paint paint) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.setLayerType"); }
    @Override public void preDispatchDraw(Canvas canvas) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.preDispatchDraw"); }
    @Override public void onStartTemporaryDetach() { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onStartTemporaryDetach"); }
    @Override public void onFinishTemporaryDetach() { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onFinishTemporaryDetach"); }
    @Override public void onActivityResult(int requestCode, int resultCode, Intent data) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.onActivityResult"); }
    @Override public Handler getHandler(Handler originalHandler) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.getHandler"); }
    @Override public View findFocus(View originalFocusedView) { throw Unimplemented.refuse("WebViewProvider.ViewDelegate.findFocus"); }

    // ---- ScrollDelegate, not yet ----
    @Override public int computeHorizontalScrollRange() { throw Unimplemented.refuse("WebViewProvider.ScrollDelegate.computeHorizontalScrollRange"); }
    @Override public int computeHorizontalScrollOffset() { throw Unimplemented.refuse("WebViewProvider.ScrollDelegate.computeHorizontalScrollOffset"); }
    @Override public int computeVerticalScrollRange() { throw Unimplemented.refuse("WebViewProvider.ScrollDelegate.computeVerticalScrollRange"); }
    @Override public int computeVerticalScrollOffset() { throw Unimplemented.refuse("WebViewProvider.ScrollDelegate.computeVerticalScrollOffset"); }
    @Override public int computeVerticalScrollExtent() { throw Unimplemented.refuse("WebViewProvider.ScrollDelegate.computeVerticalScrollExtent"); }
    @Override public void computeScroll() { throw Unimplemented.refuse("WebViewProvider.ScrollDelegate.computeScroll"); }
}
