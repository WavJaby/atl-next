// SPDX-License-Identifier: MIT
package com.example.atlwebviewoverlay;

import android.app.Activity;
import android.opengl.GLES20;
import android.opengl.GLSurfaceView;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.ViewTreeObserver;
import android.webkit.WebChromeClient;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import android.widget.RelativeLayout;

import javax.microedition.khronos.egl.EGLConfig;
import javax.microedition.khronos.opengles.GL10;

/**
 * The WebView-over-a-game gate: `xtask test window --stage webview-overlay` (xtask-harness/src/window/webview_overlay.rs). The arrangement of an in-game survey (Angry Birds 2, maintainer 2026-11-18):
 * one activity whose content is an animating, focused GLSurfaceView; after its first frame a full-window WebView is added over it at runtime.
 * Page: one full-page button over 3 window heights of content (the survey is taller than the window); each click sets the title `tap<n>` and navigates to a fragment (a history entry), so the
 * WebView can go back; the first scroll sets the title `scrolled`; each viewport resize sets `viewport<innerWidth>x<innerHeight>` (CSS px, device-width viewport). Each key the page gets is reported through the bridge ({@link Keys}): `wvo page key <n><key>`.
 * Back (Activity.onBackPressed, as an SDK of this age overrides it): WebView can go back -> goBack, logs `wvo back page`; else the WebView is removed and destroyed, logs `wvo back dismissed`;
 * with no WebView logs `wvo back game`. The activity never finishes, so every row runs in one process.
 * The WebView gets one child of the app's (a consent SDK's loading indicator), removed with removeViewAt(0) at the first onPageFinished; `wvo web children <n>` logs the count just before.
 * Right after it, a second WebView is preloaded under an INVISIBLE parent and never shown (addPreload: Angry Birds 2's survey SDK).
 * Logs: `wvo focus <gl|web|none|class>` at every Android focus move, `wvo gl frame` once, `wvo web added`, `wvo page finished`, `wvo title <t>`, `wvo page key <n><key>`, `wvo layout window=<w>x<h> web=<l,t,r,b>` at every layout change, `wvo gl key <code>` for a key the GL view gets.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    /**
     * The page: a button over the whole page; a click navigates to a fragment (a same-document history entry, as a survey's next page). loadData, not loadDataWithBaseURL: the provider maps the
     * latter to WebKit's load_bytes, whose page gets no back-forward entry (measured 2026-11-17: list length 0 after the click). No '#', '%' or '?' in the text (loadData takes it as a URL).
     */
    static final String PAGE_HTML = "<html><head><meta name=\"viewport\" content=\"width=device-width\"></head><body style=\"margin:0\"><button id=\"b\" style=\"width:100vw;height:100vh;font-size:40px\""
            + " onclick=\"window.n=(window.n||0)+1;location.hash='s'+window.n;document.title='tap'+window.n\">survey</button><div style=\"height:200vh\">more</div>"
            + "<script>document.onkeydown=function(e){window.k=(window.k||0)+1;atl.key(window.k+e.key)};"
            + "window.onscroll=function(){if(!window.s){window.s=1;document.title='scrolled'}};"
            + "window.onresize=function(){document.title='viewport'+innerWidth+'x'+innerHeight}</script></body></html>";
    /** The hidden preload's page ({@link #addPreload}): a full-page button whose click sets the title `ptap`. */
    static final String PRELOAD_HTML = "<html><body style=\"margin:0\"><button style=\"width:100vw;height:100vh\" onclick=\"document.title='ptap'\">preload</button></body></html>";
    /** After the GL view's first frame, before the WebView comes: the game is running when the survey opens. */
    static final long WEB_DELAY_MS = 500;

    /**
     * The page's key report (`atl`): a bridge call returns before the page's keydown handler does (WebView.addJavascriptInterface methods are synchronous for the page), so the line is in the log
     * before the key's unhandled answer and any back it causes. Not the title: onReceivedTitle is posted on its own path, unordered with onUnhandledKeyEvent, and the dismissal's destroy() drops a
     * title still on its way (measured 2026-11-21: Esc 2's title lost in 6 of 20 runs).
     */
    static final class Keys {
        @android.webkit.JavascriptInterface
        public void key(String k) {
            Log.i(TAG, "wvo page key " + k);
        }
    }

    private FrameLayout root;
    private WebView web;
    private String lastLayout = "";
    private final Handler ui = new Handler(Looper.getMainLooper());

    final class Renderer implements GLSurfaceView.Renderer {
        private int frame;

        @Override
        public void onSurfaceCreated(GL10 gl, EGLConfig config) {}

        @Override
        public void onSurfaceChanged(GL10 gl, int width, int height) {
            GLES20.glViewport(0, 0, width, height);
        }

        @Override
        public void onDrawFrame(GL10 gl) {
            // animating: the green channel cycles, so the GL layer presents every frame under the WebView
            GLES20.glClearColor(0.2f, (frame % 60) / 60f, 0.3f, 1f);
            GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT);
            if (frame++ == 0) {
                Log.i(TAG, "wvo gl frame");
                ui.postDelayed(new Runnable() {
                    @Override
                    public void run() {
                        addWeb();
                    }
                }, WEB_DELAY_MS);
            }
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        root = new FrameLayout(this);
        GLSurfaceView gl = new GLSurfaceView(this);
        gl.setEGLContextClientVersion(2);
        gl.setRenderer(new Renderer());
        // a game's view takes the keys: focusable, focused
        gl.setFocusable(true);
        gl.setFocusableInTouchMode(true);
        gl.setOnKeyListener(new View.OnKeyListener() {
            @Override
            public boolean onKey(View v, int keyCode, KeyEvent event) {
                if (event.getAction() == KeyEvent.ACTION_DOWN) {
                    Log.i(TAG, "wvo gl key " + keyCode);
                }
                return false;
            }
        });
        root.addView(gl, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT));
        setContentView(root);
        // every Android focus move: `wvo focus gl|web|none|<class>`
        root.getViewTreeObserver().addOnGlobalFocusChangeListener(new ViewTreeObserver.OnGlobalFocusChangeListener() {
            @Override
            public void onGlobalFocusChanged(View oldFocus, View newFocus) {
                String name = newFocus == null ? "none" : newFocus instanceof WebView ? "web" : newFocus instanceof GLSurfaceView ? "gl" : newFocus.getClass().getSimpleName();
                Log.i(TAG, "wvo focus " + name);
            }
        });
        gl.requestFocus();
        root.addOnLayoutChangeListener(relayout);
    }

    private final View.OnLayoutChangeListener relayout = new View.OnLayoutChangeListener() {
        @Override
        public void onLayoutChange(View v, int l, int t, int r, int b, int ol, int ot, int or, int ob) {
            logLayout();
        }
    };

    private void addWeb() {
        WebView w = new WebView(this);
        w.getSettings().setJavaScriptEnabled(true);
        w.addJavascriptInterface(new Keys(), "atl");
        // a consent SDK's loading indicator (Battle Cats TW, maintainer 2026-11-17): a child the app adds to the WebView (an AbsoluteLayout), removed by index when the page has loaded
        w.addView(new View(this), new FrameLayout.LayoutParams(1, 1));
        w.setWebViewClient(new WebViewClient() {
            private boolean spinnerGone;

            @Override
            public void onPageFinished(WebView view, String url) {
                if (!spinnerGone) {
                    spinnerGone = true;
                    // AOSP: a WebView's children are the app's own, so this is the indicator
                    Log.i(TAG, "wvo web children " + view.getChildCount());
                    view.removeViewAt(0);
                }
                Log.i(TAG, "wvo page finished");
            }
        });
        w.setWebChromeClient(new WebChromeClient() {
            @Override
            public void onReceivedTitle(WebView view, String title) {
                Log.i(TAG, "wvo title " + title);
                Log.i(TAG, "wvo history back=" + view.canGoBack() + " url=" + view.getUrl());
            }
        });
        w.addOnLayoutChangeListener(relayout);
        w.loadData(PAGE_HTML, "text/html", null);
        root.addView(w, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT));
        web = w;
        Log.i(TAG, "wvo web added");
        addPreload();
        addSoftwareLayer();
    }

    /** The software-layer WebView's place in its holder: a corner, clear of the centre the gate clicks; px. */
    static final int SW_LEFT = 40, SW_TOP = 40, SW_WIDTH = 160, SW_HEIGHT = 120;

    /**
     * A WebView under an ancestor with a software layer, as Angry Birds 2's account panel (Rovio AdsWebView, layer type software): AOSP draws the whole subtree into the layer's bitmap, so the
     * page must still show where the WebView is. Logs `wvo swlayer web=<l,t,r,b>` (window coordinates) at each layout change.
     */
    private void addSoftwareLayer() {
        FrameLayout holder = new FrameLayout(this);
        holder.setLayerType(View.LAYER_TYPE_SOFTWARE, null);
        addContentView(holder, new ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        WebView sw = new WebView(this);
        FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(SW_WIDTH, SW_HEIGHT);
        lp.leftMargin = SW_LEFT;
        lp.topMargin = SW_TOP;
        sw.loadData("<html><body style=\"background:#3060c0\">sw</body></html>", "text/html", null);
        sw.addOnLayoutChangeListener(new View.OnLayoutChangeListener() {
            private String last = "";

            @Override
            public void onLayoutChange(View v, int l, int t, int r, int b, int ol, int ot, int or, int ob) {
                String line = "wvo swlayer web=" + rect(v);
                if (!line.equals(last)) {
                    last = line;
                    Log.i(TAG, line);
                }
            }
        });
        holder.addView(sw, lp);
    }

    /**
     * A survey preloaded hidden, as Angry Birds 2's ad SDK does (Rovio Beacon WebViewAd.createUI, read from the APK 2026-11-18): Activity.addContentView(RelativeLayout) holds a FrameLayout set
     * INVISIBLE, the WebView inside it loads by loadDataWithBaseURL and waits for a show() that this fixture never makes. AOSP: an INVISIBLE parent is not drawn, so the WebView's SurfaceView layer
     * (created hidden, shown once its RenderNode is drawn) never shows, and its window area takes no touch (ViewGroup skips INVISIBLE children): the page under it gets the click.
     * Logs `wvo preload finished`, `wvo preload title <t>`; a click on the preload page sets `ptap`.
     */
    private void addPreload() {
        RelativeLayout holder = new RelativeLayout(this);
        addContentView(holder, new ViewGroup.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        FrameLayout hidden = new FrameLayout(this);
        hidden.setLayoutParams(new RelativeLayout.LayoutParams(RelativeLayout.LayoutParams.MATCH_PARENT, RelativeLayout.LayoutParams.MATCH_PARENT));
        hidden.setVisibility(View.INVISIBLE);
        WebView pre = new WebView(this);
        pre.getSettings().setJavaScriptEnabled(true);
        pre.setWebViewClient(new WebViewClient() {
            @Override
            public void onPageFinished(WebView view, String url) {
                Log.i(TAG, "wvo preload finished");
            }
        });
        pre.setWebChromeClient(new WebChromeClient() {
            @Override
            public void onReceivedTitle(WebView view, String title) {
                Log.i(TAG, "wvo preload title " + title);
            }
        });
        pre.loadDataWithBaseURL(null, PRELOAD_HTML, "text/html", null, null);
        hidden.addView(pre, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT));
        holder.addView(hidden);
    }

    private void logLayout() {
        String line = "window=" + root.getWidth() + "x" + root.getHeight() + " web=" + (web == null ? "none" : rect(web));
        if (!line.equals(lastLayout)) {
            lastLayout = line;
            Log.i(TAG, "wvo layout " + line);
        }
    }

    @Override
    public void onBackPressed() {
        if (web == null) {
            Log.i(TAG, "wvo back game");
        } else if (web.canGoBack()) {
            web.goBack();
            Log.i(TAG, "wvo back page");
        } else {
            root.removeView(web);
            web.destroy();
            web = null;
            Log.i(TAG, "wvo back dismissed");
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "onResume reached");
    }

    /** `left,top,right,bottom` of `v` in the window. */
    private static String rect(View v) {
        int[] at = new int[2];
        v.getLocationInWindow(at);
        return at[0] + "," + at[1] + "," + (at[0] + v.getWidth()) + "," + (at[1] + v.getHeight());
    }
}
