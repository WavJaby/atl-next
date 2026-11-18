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
 * native call into libatl_webview.so, which keeps the page view behind {@link #mNative}; an unimplemented method answers its WebView contract's default, failure value or no-op, counted (Unimplemented.accept / answer); only the class-C methods
 * (xtask provider-refusals table: no contract-legal answer) throw Unimplemented.refuse("<Interface>.<method>").
 * The clients are stored here as handed in (the natives read them for each callback); getWebViewClient's default instance is WebView's documented "default client if not yet set".
 */
final class WebViewProviderImpl implements WebViewProvider, WebViewProvider.ViewDelegate, WebViewProvider.ScrollDelegate, android.view.ViewTreeObserver.OnPreDrawListener {
    // read and cleared by the natives: the native view, 0 after destroy
    private long mNative;
    // read by the natives: the WebView passed to every client callback
    private final WebView mWebView;
    private final WebView.PrivateAccess mWebViewPrivate;
    private final WebSettingsImpl mSettings;
    private WebViewClient mWebViewClient = new WebViewClient();
    private WebChromeClient mWebChromeClient;
    private DownloadListener mDownloadListener;

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
    // read by the natives at a download (the engine dropped a navigation it does not show): no listener = nothing happens, the page stays
    @Override public void setDownloadListener(DownloadListener listener) { mDownloadListener = listener; }
    @Override public native void setInitialScale(int scaleInPercent);

    // ---- View-system callbacks on every WebView: counted, not thrown (Unimplemented.accept) ----
    // View state: WebView.setOverScrollMode's super call (WebView.java:2703) keeps it, so getOverScrollMode answers; the page has no overscroll mode of its own
    @Override public void setOverScrollMode(int mode) {}
    @Override public void setScrollBarStyle(int style) { Unimplemented.accept("WebViewProvider.ViewDelegate.setScrollBarStyle"); }

    // ---- engine-backed beyond W2 ----
    @Override public native String getOriginalUrl();
    // isPaused answers; the engine is not paused (no WebKitGTK per-view pause), counted natively
    @Override public native void onPause();
    @Override public native void onResume();
    @Override public native boolean isPaused();

    // ---- class C (xtask provider-refusals table): no engine capability and no answer the WebView contract allows without misleading the app; throw ----
    // a no-op drops the POST navigation (login forms): WebKitGTK's URIRequest carries no body
    @Override public void postUrl(String url, byte[] postData) { throw Unimplemented.refuse("WebViewProvider.postUrl"); }
    // a never-called callback stalls the app's reveal
    @Override public void insertVisualStateCallback(long requestId, VisualStateCallback callback) { throw Unimplemented.refuse("WebViewProvider.insertVisualStateCallback"); }
    @Override public PrintDocumentAdapter createPrintDocumentAdapter(String documentName) { throw Unimplemented.refuse("WebViewProvider.createPrintDocumentAdapter"); }
    @Override public WebBackForwardList copyBackForwardList() { throw Unimplemented.refuse("WebViewProvider.copyBackForwardList"); }
    @Override public WebMessagePort[] createWebMessageChannel() { throw Unimplemented.refuse("WebViewProvider.createWebMessageChannel"); }
    @Override public void postMessageToMainFrame(WebMessage message, Uri targetOrigin) { throw Unimplemented.refuse("WebViewProvider.postMessageToMainFrame"); }

    // ---- class B: no engine capability; the WebView contract's default / failure value / no-op, counted (Unimplemented.accept, Unimplemented.answer) ----
    @Override public void setHorizontalScrollbarOverlay(boolean overlay) { Unimplemented.accept("WebViewProvider.setHorizontalScrollbarOverlay"); }
    @Override public void setVerticalScrollbarOverlay(boolean overlay) { Unimplemented.accept("WebViewProvider.setVerticalScrollbarOverlay"); }
    @Override public boolean overlayHorizontalScrollbar() { Unimplemented.accept("WebViewProvider.overlayHorizontalScrollbar"); return false; }
    @Override public boolean overlayVerticalScrollbar() { Unimplemented.accept("WebViewProvider.overlayVerticalScrollbar"); return false; }
    @Override public int getVisibleTitleHeight() { Unimplemented.accept("WebViewProvider.getVisibleTitleHeight"); return 0; }
    // "null if there is no certificate (the site is not secure)"
    @Override public SslCertificate getCertificate() { Unimplemented.accept("WebViewProvider.getCertificate"); return null; }
    // deprecated: "has no useful effect"
    @Override public void setCertificate(SslCertificate certificate) { Unimplemented.accept("WebViewProvider.setCertificate"); }
    // deprecated: saving passwords "will not be supported"
    @Override public void savePassword(String host, String username, String password) { Unimplemented.accept("WebViewProvider.savePassword"); }
    // nothing stored -> get answers "null if no credentials"
    @Override public void setHttpAuthUsernamePassword(String host, String realm, String username, String password) { Unimplemented.accept("WebViewProvider.setHttpAuthUsernamePassword"); }
    @Override public String[] getHttpAuthUsernamePassword(String host, String realm) { Unimplemented.accept("WebViewProvider.getHttpAuthUsernamePassword"); return null; }
    @Override public void setNetworkAvailable(boolean networkUp) { Unimplemented.accept("WebViewProvider.setNetworkAvailable"); }
    // "null if the method fails"
    @Override public WebBackForwardList saveState(Bundle outState) { Unimplemented.accept("WebViewProvider.saveState"); return null; }
    @Override public WebBackForwardList restoreState(Bundle inState) { Unimplemented.accept("WebViewProvider.restoreState"); return null; }
    @Override public boolean savePicture(Bundle b, File dest) { Unimplemented.accept("WebViewProvider.savePicture"); return false; }
    @Override public boolean restorePicture(Bundle b, File src) { Unimplemented.accept("WebViewProvider.restorePicture"); return false; }
    @Override public void saveWebArchive(String filename) { Unimplemented.accept("WebViewProvider.saveWebArchive"); }
    // callback gets "null if the file could not be saved"
    @Override public void saveWebArchive(String basename, boolean autoname, ValueCallback<String> callback) { Unimplemented.answer("WebViewProvider.saveWebArchive", callback, null); }
    @Override public boolean isPrivateBrowsingEnabled() { Unimplemented.accept("WebViewProvider.isPrivateBrowsingEnabled"); return false; }
    @Override public boolean pageUp(boolean top) { Unimplemented.accept("WebViewProvider.pageUp"); return false; }
    @Override public boolean pageDown(boolean bottom) { Unimplemented.accept("WebViewProvider.pageDown"); return false; }
    @Override public void clearView() { Unimplemented.accept("WebViewProvider.clearView"); }
    @Override public Picture capturePicture() { Unimplemented.accept("WebViewProvider.capturePicture"); return new Picture(); }
    @Override public float getScale() { Unimplemented.accept("WebViewProvider.getScale"); return mWebView.getResources().getDisplayMetrics().density; }
    @Override public void invokeZoomPicker() { Unimplemented.accept("WebViewProvider.invokeZoomPicker"); }
    // @NonNull; the default result is UNKNOWN_TYPE
    @Override public HitTestResult getHitTestResult() { Unimplemented.accept("WebViewProvider.getHitTestResult"); return new HitTestResult(); }
    @Override public void requestFocusNodeHref(Message hrefMsg) { Unimplemented.accept("WebViewProvider.requestFocusNodeHref"); }
    @Override public void requestImageRef(Message msg) { Unimplemented.accept("WebViewProvider.requestImageRef"); }
    @Override public Bitmap getFavicon() { Unimplemented.accept("WebViewProvider.getFavicon"); return null; }
    @Override public String getTouchIconUrl() { Unimplemented.accept("WebViewProvider.getTouchIconUrl"); return null; }
    // the ScrollDelegate's answer: the page fits the view
    @Override public int getContentHeight() { Unimplemented.accept("WebViewProvider.getContentHeight"); return mWebView.getHeight(); }
    @Override public int getContentWidth() { Unimplemented.accept("WebViewProvider.getContentWidth"); return mWebView.getWidth(); }
    // process-wide JavaScript timers: WebKitGTK has no control (page visibility would also hide the page); nothing is paused
    @Override public void pauseTimers() { Unimplemented.accept("WebViewProvider.pauseTimers"); }
    @Override public void resumeTimers() { Unimplemented.accept("WebViewProvider.resumeTimers"); }
    @Override public void freeMemory() { Unimplemented.accept("WebViewProvider.freeMemory"); }
    @Override public void clearCache(boolean includeDiskFiles) { Unimplemented.accept("WebViewProvider.clearCache"); }
    // AOSP: only removes the autocomplete popup, which the engine does not show over the page
    @Override public void clearFormData() { Unimplemented.accept("WebViewProvider.clearFormData"); }
    // divergence: WebKitGTK cannot clear its back-forward list, canGoBack stays true
    @Override public void clearHistory() { Unimplemented.accept("WebViewProvider.clearHistory"); }
    @Override public void clearSslPreferences() { Unimplemented.accept("WebViewProvider.clearSslPreferences"); }
    @Override public void setFindListener(WebView.FindListener listener) { Unimplemented.accept("WebViewProvider.setFindListener"); }
    @Override public void findNext(boolean forward) { Unimplemented.accept("WebViewProvider.findNext"); }
    @Override public int findAll(String find) { Unimplemented.accept("WebViewProvider.findAll"); return 0; }
    @Override public void findAllAsync(String find) { Unimplemented.accept("WebViewProvider.findAllAsync"); }
    @Override public boolean showFindDialog(String text, boolean showIme) { Unimplemented.accept("WebViewProvider.showFindDialog"); return false; }
    @Override public void clearMatches() { Unimplemented.accept("WebViewProvider.clearMatches"); }
    @Override public void notifyFindDialogDismissed() { Unimplemented.accept("WebViewProvider.notifyFindDialogDismissed"); }
    @Override public void documentHasImages(Message response) { Unimplemented.accept("WebViewProvider.documentHasImages"); }
    // "null if WebView is not running in multiprocess mode"
    @Override public WebViewRenderProcess getWebViewRenderProcess() { Unimplemented.accept("WebViewProvider.getWebViewRenderProcess"); return null; }
    @Override public void setWebViewRenderProcessClient(Executor executor, WebViewRenderProcessClient client) { Unimplemented.accept("WebViewProvider.setWebViewRenderProcessClient"); }
    @Override public WebViewRenderProcessClient getWebViewRenderProcessClient() { Unimplemented.accept("WebViewProvider.getWebViewRenderProcessClient"); return null; }
    @Override public void setPictureListener(PictureListener listener) { Unimplemented.accept("WebViewProvider.setPictureListener"); }
    @Override public void setMapTrackballToArrowKeys(boolean setMap) { Unimplemented.accept("WebViewProvider.setMapTrackballToArrowKeys"); }
    @Override public void flingScroll(int vx, int vy) { Unimplemented.accept("WebViewProvider.flingScroll"); }
    @Override public View getZoomControls() { Unimplemented.accept("WebViewProvider.getZoomControls"); return null; }
    @Override public boolean canZoomIn() { Unimplemented.accept("WebViewProvider.canZoomIn"); return false; }
    @Override public boolean canZoomOut() { Unimplemented.accept("WebViewProvider.canZoomOut"); return false; }
    @Override public boolean zoomBy(float zoomFactor) { Unimplemented.accept("WebViewProvider.zoomBy"); return false; }
    @Override public boolean zoomIn() { Unimplemented.accept("WebViewProvider.zoomIn"); return false; }
    @Override public boolean zoomOut() { Unimplemented.accept("WebViewProvider.zoomOut"); return false; }
    @Override public void dumpViewHierarchyWithProperties(BufferedWriter out, int level) { Unimplemented.accept("WebViewProvider.dumpViewHierarchyWithProperties"); }
    @Override public View findHierarchyView(String className, int hashCode) { Unimplemented.accept("WebViewProvider.findHierarchyView"); return null; }
    @Override public void setRendererPriorityPolicy(int rendererRequestedPriority, boolean waivedWhenNotVisible) { Unimplemented.accept("WebViewProvider.setRendererPriorityPolicy"); }
    // the documented defaults: RENDERER_PRIORITY_IMPORTANT, not waived
    @Override public int getRendererRequestedPriority() { Unimplemented.accept("WebViewProvider.getRendererRequestedPriority"); return WebView.RENDERER_PRIORITY_IMPORTANT; }
    @Override public boolean getRendererPriorityWaivedWhenNotVisible() { Unimplemented.accept("WebViewProvider.getRendererPriorityWaivedWhenNotVisible"); return false; }
    @Override public boolean canGoBack() { return canGoBackOrForward(-1); }
    @Override public void goBack() { goBackOrForward(-1); }
    @Override public boolean canGoForward() { return canGoBackOrForward(1); }
    @Override public void goForward() { goBackOrForward(1); }
    @Override public native boolean canGoBackOrForward(int steps);
    @Override public native void goBackOrForward(int steps);

    // ---- ViewDelegate, composition (W3a): the page is drawn by the platform in the hole of the carrier SurfaceView child (the native init adds it), so the View side keeps View's defaults ----
    // WebView.onMeasure ran AbsoluteLayout's measure (the carrier included) before this: kept (Chromium sizes wrap_content by page content, not yet)
    @Override public void onMeasure(int widthMeasureSpec, int heightMeasureSpec) {}
    @Override public boolean setFrame(int left, int top, int right, int bottom) { return mWebViewPrivate.super_setFrame(left, top, right, bottom); }
    @Override public void setLayoutParams(LayoutParams layoutParams) { mWebViewPrivate.super_setLayoutParams(layoutParams); }
    // the carrier is a transient view of the WebView (the native init adds it): no layout pass reaches it, the natives size it to the WebView; the platform sizes the page to the carrier's layer
    @Override public void onSizeChanged(int w, int h, int ow, int oh) { sizeChanged(w, h); }
    private native void sizeChanged(int w, int h);
    // nothing to draw: the carrier's hole shows the page
    @Override public void onDraw(Canvas canvas) {}
    // the View-side events that decide whether the carrier is on screen, logged per view by the natives; window visibility is forwarded to the carrier (transients get no visibility dispatch)
    @Override public void onAttachedToWindow() { viewEvent("attached", 0); mWebView.getViewTreeObserver().addOnPreDrawListener(this); }
    @Override public void onDetachedFromWindow() { viewEvent("detached", 0); mWebView.getViewTreeObserver().removeOnPreDrawListener(this); }
    // each frame: the natives place the carrier when a software layer keeps RenderThread from it (AB2's account panel); never cancels the draw
    @Override public native boolean onPreDraw();
    @Override public void onVisibilityChanged(View changedView, int visibility) { viewEvent("visibility", visibility); }
    @Override public native void onWindowVisibilityChanged(int visibility);
    private native void viewEvent(String what, int value);
    @Override public void onScrollChanged(int l, int t, int oldl, int oldt) {}
    // Chromium's answer (WebViewChromium.shouldDelayChildPressedState)
    @Override public boolean shouldDelayChildPressedState() { return true; }
    @Override public boolean requestFocus(int direction, Rect previouslyFocusedRect) { return mWebViewPrivate.super_requestFocus(direction, previouslyFocusedRect); }
    @Override public void computeScroll() { mWebViewPrivate.super_computeScroll(); }
    // ---- ViewDelegate input (W3b, webview plan S2.2 amended): Android's dispatch reached the View, so the natives let the page have the toolkit events held for it up to this event's time ----
    @Override public native boolean onTouchEvent(MotionEvent ev);
    @Override public native boolean onHoverEvent(MotionEvent event);
    @Override public native boolean onGenericMotionEvent(MotionEvent event);
    @Override public native boolean dispatchKeyEvent(KeyEvent event);
    @Override public native void onFocusChanged(boolean focused, int direction, Rect previouslyFocusedRect);
    // window focus moves the toolkit focus with the window already
    @Override public void onWindowFocusChanged(boolean hasWindowFocus) {}
    // no Android input connection: the page's text input goes through the engine's own input method context, the desktop's (Linux convention); View's answer
    @Override public InputConnection onCreateInputConnection(EditorInfo outAttrs) { return null; }

    // ---- ViewDelegate, class B: View's answer (the page's own accessibility tree, drag and trackball are not wired), counted ----
    @Override public void onProvideVirtualStructure(android.view.ViewStructure structure) { Unimplemented.accept("WebViewProvider.ViewDelegate.onProvideVirtualStructure"); }
    @Override public AccessibilityNodeProvider getAccessibilityNodeProvider() { Unimplemented.accept("WebViewProvider.ViewDelegate.getAccessibilityNodeProvider"); return null; }
    @Override public void onInitializeAccessibilityNodeInfo(AccessibilityNodeInfo info) { Unimplemented.accept("WebViewProvider.ViewDelegate.onInitializeAccessibilityNodeInfo"); }
    @Override public void onInitializeAccessibilityEvent(AccessibilityEvent event) { Unimplemented.accept("WebViewProvider.ViewDelegate.onInitializeAccessibilityEvent"); }
    @Override public boolean performAccessibilityAction(int action, Bundle arguments) { Unimplemented.accept("WebViewProvider.ViewDelegate.performAccessibilityAction"); return mWebViewPrivate.super_performAccessibilityAction(action, arguments); }
    @Override public void onOverScrolled(int scrollX, int scrollY, boolean clampedX, boolean clampedY) { Unimplemented.accept("WebViewProvider.ViewDelegate.onOverScrolled"); }
    @Override public boolean performLongClick() { Unimplemented.accept("WebViewProvider.ViewDelegate.performLongClick"); return mWebViewPrivate.super_performLongClick(); }
    @Override public boolean onDragEvent(DragEvent event) { Unimplemented.accept("WebViewProvider.ViewDelegate.onDragEvent"); return false; }
    @Override public boolean onTrackballEvent(MotionEvent ev) { Unimplemented.accept("WebViewProvider.ViewDelegate.onTrackballEvent"); return false; }
    @Override public void onActivityResult(int requestCode, int resultCode, Intent data) { Unimplemented.accept("WebViewProvider.ViewDelegate.onActivityResult"); }

    // ---- ViewDelegate on the View system's own paths (draw, focus search, post, detach): View's answer; the page-side meaning (Chromium's) counted until a slice needs it ----
    @Override public void onDrawVerticalScrollBar(Canvas canvas, android.graphics.drawable.Drawable scrollBar, int l, int t, int r, int b) { mWebViewPrivate.super_onDrawVerticalScrollBar(canvas, scrollBar, l, t, r, b); }
    @Override public void preDispatchDraw(Canvas canvas) {}
    @Override public Handler getHandler(Handler originalHandler) { return originalHandler; }
    @Override public View findFocus(View originalFocusedView) { return originalFocusedView; }
    @Override public void onStartTemporaryDetach() {}
    @Override public void onFinishTemporaryDetach() {}
    @Override public void setLayerType(int layerType, Paint paint) { viewEvent("layer type", layerType); }
    @Override public void onConfigurationChanged(Configuration newConfig) { Unimplemented.accept("WebViewProvider.ViewDelegate.onConfigurationChanged"); }
    @Override public native void setBackgroundColor(int color);
    @Override public boolean requestChildRectangleOnScreen(View child, Rect rect, boolean immediate) { Unimplemented.accept("WebViewProvider.ViewDelegate.requestChildRectangleOnScreen"); return false; }
    @Override public boolean onKeyMultiple(int keyCode, int repeatCount, KeyEvent event) { Unimplemented.accept("WebViewProvider.ViewDelegate.onKeyMultiple"); return false; }
    @Override public boolean onKeyDown(int keyCode, KeyEvent event) { Unimplemented.accept("WebViewProvider.ViewDelegate.onKeyDown"); return false; }
    @Override public boolean onKeyUp(int keyCode, KeyEvent event) { Unimplemented.accept("WebViewProvider.ViewDelegate.onKeyUp"); return false; }

    // ---- ScrollDelegate: View's defaults (the page fits the view) until the page's scroll extent is read from the engine ----
    @Override public int computeHorizontalScrollRange() { return mWebView.getWidth(); }
    @Override public int computeHorizontalScrollOffset() { return mWebView.getScrollX(); }
    @Override public int computeVerticalScrollRange() { return mWebView.getHeight(); }
    @Override public int computeVerticalScrollOffset() { return mWebView.getScrollY(); }
    @Override public int computeVerticalScrollExtent() { return mWebView.getHeight(); }
}
