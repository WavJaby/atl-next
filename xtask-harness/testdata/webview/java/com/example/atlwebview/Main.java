// SPDX-License-Identifier: MIT
package com.example.atlwebview;

import android.app.Activity;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.Message;
import android.util.Log;
import android.view.View;
import android.view.ViewTreeObserver;
import android.view.WindowManager;
import android.webkit.JavascriptInterface;
import android.webkit.WebChromeClient;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * The WebView gate: `xtask test window --stage webview` (xtask-harness/src/window/webview.rs). A software window of colour WINDOW holding, at fixed pixel rectangles, a WebView whose page
 * is one colour (PAGE) and an OVERLAY view of the window over part of it: the page shows in the WebView's rectangle at the WebView's depth, the overlay above it.
 * Input (W3b): every click the page gets sets its title to `tap<n>` (onReceivedTitle logs it); a click on the overlay logs `wv overlay tap`; the WebView's parent (Intercept) takes a gesture at its
 * first move, as a ScrollView does, so a drag on the page gets the WebView ACTION_DOWN, then ACTION_CANCEL.
 * Keys: each key the page gets sets its title to `key<n><key>`; the page's handler takes the first Escape (preventDefault, title suffix `-prevented`) and leaves later ones, so an unhandled Esc reaches
 * the app (WebViewClient.onUnhandledKeyEvent -> the key map's fallback, BACK with esc-back on): the back callback logs `wv back` and keeps the activity.
 * Rectangles are in window pixels, set as layout params so no density rounding applies; after the layout the fixture logs each view's rectangle in the window, and the page's onPageFinished.
 * A WebView that cannot be made (no provider, no engine) is logged and left out, so the gate's picture shows the window there.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    static final String WINDOW_TITLE = "AtlWebViewFixture";
    static final int WINDOW = 0xff204060;
    static final int OVERLAY = 0xfff0f0f0;
    /**
     * PAGE's colour 0x3070c0 as the page's CSS, and the click counter. Not `#3070c0`: loadData takes the content as a URL, so a bare '#' starts its fragment and cuts the page
     * (WebView.loadData doc: escape it as %23); for the same reason no '%' or '?'.
     */
    static final String PAGE_HTML = "<html><body style=\"margin:0;height:100vh;background:rgb(48,112,192)\" onclick=\"window.n=(window.n||0)+1;document.title='tap'+window.n\""
            + " onkeydown=\"window.k=(window.k||0)+1;if(event.key=='Escape'&&window.k==1)event.preventDefault();document.title='key'+window.k+event.key+(event.defaultPrevented?'-prevented':'')\">"
            + soundScript() + "</body></html>";

    /** The page's tone (webview.rs TONE_PEAK): TONE_HZ at TONE_PEAK of full scale for TONE_MS, 16-bit mono PCM at TONE_RATE. */
    static final int TONE_HZ = 1000, TONE_RATE = 48000, TONE_MS = 1000;
    static final double TONE_PEAK = 0.5;

    /**
     * The sound sub-gate's page part: builds the tone as a WAV and plays it through an `<audio>` (no controls: draws nothing) from a blob: URL, at load, without a gesture
     * (setMediaPlaybackRequiresUserGesture(false) below); no network, no asset, no title change (the input sub-gates read the titles). Same character limits as PAGE_HTML.
     */
    static String soundScript() {
        // in a closure: the page's own handlers keep their globals (window.n, window.k)
        return "<audio id=\"tone\"></audio><script>(function() {"
                + "var rate = " + TONE_RATE + ", n = rate * " + TONE_MS + " / 1000, v = new DataView(new ArrayBuffer(44 + 2 * n));"
                + "function s(o, t) { for (var i = 0; i < 4; i++) v.setUint8(o + i, t.charCodeAt(i)); }"
                + "s(0, 'RIFF'); v.setUint32(4, 36 + 2 * n, true); s(8, 'WAVE'); s(12, 'fmt '); v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);"
                + "v.setUint32(24, rate, true); v.setUint32(28, rate * 2, true); v.setUint16(32, 2, true); v.setUint16(34, 16, true); s(36, 'data'); v.setUint32(40, 2 * n, true);"
                + "for (var i = 0; i < n; i++) v.setInt16(44 + 2 * i, Math.round(" + TONE_PEAK + " * 32767 * Math.sin(2 * Math.PI * " + TONE_HZ + " * i / rate)), true);"
                + "var a = document.getElementById('tone'); a.src = URL.createObjectURL(new Blob([v.buffer], {type: 'audio/wav'})); a.play();"
                + "})();</script>";
    }

    /** How long after the first onPageFinished the tone's state is logged: its TONE_MS plus the web process's media start (seconds on a loaded host). */
    static final int SOUND_STATE_MS = TONE_MS + 4000;

    /**
     * The `wv sound` line (webview.rs sound sub-gate prints it on failure): the tone element's readyState, networkState, paused, ended, currentTime and error code SOUND_STATE_MS after the
     * first load; a played tone reads `4 1 true true 1 0`.
     */
    private void soundState(final WebView view) {
        view.postDelayed(new Runnable() {
            @Override
            public void run() {
                view.evaluateJavascript("(function(a){return [a.readyState,a.networkState,a.paused,a.ended,a.currentTime,a.error?a.error.code:0].join(' ')})(document.getElementById('tone'))",
                        new android.webkit.ValueCallback<String>() {
                            @Override
                            public void onReceiveValue(String v) {
                                Log.i(TAG, "wv sound " + v);
                            }
                        });
            }
        }, SOUND_STATE_MS);
    }

    /** The WebView's parent: intercepts a gesture at its first move (as a scrolling parent does), so the WebView gets ACTION_CANCEL. */
    static final class Intercept extends FrameLayout {
        Intercept(android.content.Context c) {
            super(c);
        }

        @Override
        public boolean onInterceptTouchEvent(android.view.MotionEvent ev) {
            if (ev.getActionMasked() == android.view.MotionEvent.ACTION_MOVE) {
                Log.i(TAG, "wv intercepted");
                return true;
            }
            return false;
        }
    }

    /**
     * The popup gate (webview.rs drive_popup, popup_check, script_check). preparePopupGate runs in onCreate; startPopupGate runs when the keyboard cases end (title key2Escape).
     * Phases: control (the control route's script clicks its anchor with popups disabled: no create), real (a compositor click on the opener's anchor: one create, gesture true; the
     * destination is attached, gets UA and bridge, two Handler posts, then the transport), script (evaluateJavascript window.open with popups enabled: gesture false; refused).
     * Lines `wv popup ...`; a failure names its assertion (`wv popup failed <name>`), each bounded by POPUP_TIMEOUT_MS.
     */
    static final String UA = "A34-UA/1.0";
    static final int[] OPENER_RECT = {400, 40, 720, 240};
    static final int[] CHILD_RECT = {400, 280, 720, 480};
    static final int POPUP_TIMEOUT_MS = 10000;
    static final int PHASE_CONTROL = 0, PHASE_REAL = 1, PHASE_SCRIPT = 2;
    private Handler main;
    private WebView opener;
    private volatile int port;
    private int phase = -1;
    private int creates;
    private boolean gesture;
    private boolean attachedBeforeSend;
    private boolean openerReady;
    private boolean reloadPending;
    private boolean childReady;
    private String anchorBounds = "";
    private String childBounds = "";
    /** Reports from the child's page (the injection marker: one) and from the opener's page (none). */
    private final AtomicInteger bridgeReports = new AtomicInteger();
    private final AtomicInteger openerReports = new AtomicInteger();
    private final AtomicInteger taps = new AtomicInteger();
    private volatile String childUa = "";
    private volatile String childOpenerNull = "";

    private FrameLayout root;
    private View web;
    private View over;
    private boolean laidOut;
    private boolean apiChecked;

    /**
     * The calls ad / consent SDKs make on the main thread (Temple Run's Pangle ad: resumeTimers; Battle Cats' consent view: onPause): AOSP throws none of them.
     * One line: `wv api ok ...` with the answers, or `wv api threw <exception>`.
     */
    private static void callApis(WebView w) {
        try {
            w.pauseTimers();
            w.resumeTimers();
            w.onPause();
            w.onResume();
            w.clearCache(true);
            w.clearHistory();
            w.clearFormData();
            Log.i(TAG, "wv api ok progress=" + w.getProgress() + " url=" + w.getUrl() + " original=" + w.getOriginalUrl() + " js=" + w.getSettings().getJavaScriptEnabled()
                    + " hit=" + w.getHitTestResult().getType() + " state=" + w.saveState(new Bundle()));
        } catch (Throwable t) {
            Log.i(TAG, "wv api threw " + t);
        }
        offThread(w);
    }

    /**
     * A targetSdk < 18 app's off-thread call: WebView.checkThread only logs for it and calls the provider (WebView.java, sEnforceThreadChecking false below JELLY_BEAN_MR2), so the flag is cleared
     * for the call. One line: `wv offthread url=<what getUrl answered or threw> settings=<ok or what threw>`.
     */
    private static void offThread(final WebView w) {
        // an anonymous class: the fixture compiles against android.jar, which has no LambdaMetafactory
        Thread t = new Thread(new Runnable() { @Override public void run() {
            String url;
            String settings = "ok";
            try {
                java.lang.reflect.Field f = WebView.class.getDeclaredField("sEnforceThreadChecking");
                f.setAccessible(true);
                f.setBoolean(null, false);
                try {
                    try {
                        url = String.valueOf(w.getUrl());
                    } catch (Throwable e) {
                        url = e.getClass().getName();
                    }
                    try {
                        android.webkit.WebSettings s = w.getSettings();
                        s.setJavaScriptEnabled(true);
                        if (!s.getJavaScriptEnabled()) settings = "lost";
                    } catch (Throwable e) {
                        settings = e.getClass().getName();
                    }
                } finally {
                    f.setBoolean(null, true);
                }
            } catch (Throwable e) {
                url = "reflection " + e;
            }
            Log.i(TAG, "wv offthread url=" + url + " settings=" + settings);
        }}, "wv-worker");
        t.start();
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        WindowManager.LayoutParams attrs = getWindow().getAttributes();
        attrs.setTitle(WINDOW_TITLE);
        getWindow().setAttributes(attrs);
        root = new FrameLayout(this);
        root.setBackgroundColor(WINDOW);
        Intercept parent = new Intercept(this);
        try {
            WebView w = new WebView(this);
            w.getSettings().setJavaScriptEnabled(true);
            w.getSettings().setMediaPlaybackRequiresUserGesture(false);
            w.setWebViewClient(new WebViewClient() {
                @Override
                public void onPageFinished(WebView view, String url) {
                    Log.i(TAG, "wv page finished");
                    if (!apiChecked) {
                        apiChecked = true;
                        callApis(view);
                        soundState(view);
                    }
                }
            });
            w.setWebChromeClient(new android.webkit.WebChromeClient() {
                @Override
                public void onReceivedTitle(WebView view, String title) {
                    Log.i(TAG, "wv title " + title);
                    // the last keyboard case's title: the existing input and readback cases are done, the popup gate starts
                    if ("key2Escape".equals(title) && phase == -1) {
                        startPopupGate();
                    }
                }
            });
            w.loadData(PAGE_HTML, "text/html", null);
            // the focused WebView (a desktop is not in touch mode) would get View's default focus highlight, a faint white over its whole rectangle (measured: page 48,112,192 read as
            // 54,116,194): AOSP draws it there too, but this picture is about composition
            w.setDefaultFocusHighlightEnabled(false);
            web = w;
        } catch (Throwable t) {
            Log.i(TAG, "wv no webview: " + t);
            web = new View(this);
        }
        parent.addView(web, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT));
        place(parent, 40, 40, 300, 200);
        over = new View(this);
        over.setBackgroundColor(OVERLAY);
        over.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                Log.i(TAG, "wv overlay tap");
            }
        });
        place(over, 100, 80, 60, 40);
        setContentView(root);
        // BACK through the predictive-back path (what a targetSdk 36 app gets): logged, the activity stays for the rest of the gate
        getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                android.window.OnBackInvokedDispatcher.PRIORITY_DEFAULT,
                new android.window.OnBackInvokedCallback() {
                    @Override
                    public void onBackInvoked() {
                        Log.i(TAG, "wv back");
                    }
                });
        root.getViewTreeObserver().addOnGlobalLayoutListener(new ViewTreeObserver.OnGlobalLayoutListener() {
            @Override
            public void onGlobalLayout() {
                if (laidOut) {
                    return;
                }
                laidOut = true;
                Log.i(TAG, "wv layout window=" + root.getWidth() + "x" + root.getHeight() + " web=" + rect(web) + " over=" + rect(over));
            }
        });
        preparePopupGate();
    }

    /** Initializes the popup scenario: the handler, the loopback server, the opener WebView (placed by startPopupGate). */
    private void preparePopupGate() {
        main = new Handler(Looper.getMainLooper());
        startServer();
        try {
            WebView o = new WebView(this);
            WebSettings s = o.getSettings();
            s.setJavaScriptEnabled(true);
            s.setSupportMultipleWindows(true);
            s.setJavaScriptCanOpenWindowsAutomatically(false);
            o.setWebViewClient(new WebViewClient() {
                @Override
                public void onPageFinished(WebView view, String url) {
                    // the reload after the child's tap: its document start asked the bridge list again (WebView.java:1888); its report is the opener's last word
                    if (reloadPending && url.endsWith("/opener")) {
                        reloadPending = false;
                        summary();
                        script();
                        return;
                    }
                    if (phase == PHASE_REAL && !openerReady && url.endsWith("/opener")) {
                        openerReady = true;
                        anchorBounds = rect(view);
                        Log.i(TAG, "wv popup opener ready anchorBounds=" + anchorBounds);
                        main.postDelayed(new Runnable() {
                            @Override
                            public void run() {
                                if (creates == 0) {
                                    Log.i(TAG, "wv popup failed create");
                                }
                            }
                        }, POPUP_TIMEOUT_MS);
                    }
                }
            });
            o.setWebChromeClient(new WebChromeClient() {
                @Override
                public void onReceivedTitle(WebView view, String title) {
                    if (phase == PHASE_CONTROL && "control-clicked".equals(title)) {
                        Log.i(TAG, "wv popup control clicked creates=" + creates);
                        if (creates != 0) {
                            Log.i(TAG, "wv popup failed control-creates");
                            return;
                        }
                        phase = PHASE_REAL;
                        view.loadUrl(base() + "/opener");
                    }
                }

                @Override
                public boolean onCreateWindow(WebView view, boolean isDialog, boolean isUserGesture, Message resultMsg) {
                    return onPopup(isUserGesture, resultMsg);
                }
            });
            opener = o;
        } catch (Throwable t) {
            Log.i(TAG, "wv popup no webview: " + t);
        }
    }

    /** WebChromeClient.onCreateWindow of the opener, on the main thread. Real phase: the destination is attached, gets UA and bridge, two posts, then the transport. Others refused. */
    private boolean onPopup(boolean isUserGesture, final Message resultMsg) {
        creates++;
        Log.i(TAG, "wv popup create n=" + creates + " gesture=" + isUserGesture + " phase=" + phase);
        if (phase == PHASE_SCRIPT) {
            Log.i(TAG, "wv popup script-open gesture=" + isUserGesture + " creates=" + creates);
            return false;
        }
        if (phase != PHASE_REAL || creates != 1) {
            if (phase == PHASE_CONTROL) {
                Log.i(TAG, "wv popup failed control-creates");
            }
            return false;
        }
        gesture = isUserGesture;
        final WebView c = new WebView(this);
        c.getSettings().setJavaScriptEnabled(true);
        c.setWebViewClient(new WebViewClient() {
            @Override
            public void onPageFinished(WebView view, String url) {
                if (childReady || !url.endsWith("/popup")) {
                    return;
                }
                childReady = true;
                childBounds = rect(view);
                if (bridgeReports.get() == 0) {
                    Log.i(TAG, "wv popup failed child-bridge");
                } else if (openerReports.get() != 0) {
                    Log.i(TAG, "wv popup failed opener-bridge");
                } else if (!UA.equals(childUa)) {
                    Log.i(TAG, "wv popup failed child-ua");
                }
                Log.i(TAG, "wv popup child ready childBounds=" + childBounds);
                main.postDelayed(new Runnable() {
                    @Override
                    public void run() {
                        if (taps.get() == 0) {
                            Log.i(TAG, "wv popup failed child-tap");
                        }
                    }
                }, POPUP_TIMEOUT_MS);
            }
        });
        place(c, CHILD_RECT[0], CHILD_RECT[1], CHILD_RECT[2] - CHILD_RECT[0], CHILD_RECT[3] - CHILD_RECT[1]);
        c.getSettings().setUserAgentString(UA);
        c.addJavascriptInterface(new PopupBridge(), "atlBridge");
        final WebView.WebViewTransport transport = (WebView.WebViewTransport) resultMsg.obj;
        main.post(new Runnable() {
            @Override
            public void run() {
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        boolean attached = c.getParent() != null;
                        attachedBeforeSend = attached;
                        if (!attached) {
                            Log.i(TAG, "wv popup failed attached-before-send");
                        }
                        transport.setWebView(c);
                        resultMsg.sendToTarget();
                        Log.i(TAG, "wv popup sent attached=" + attached);
                    }
                });
            }
        });
        return true;
    }

    /** Starts the popup scenario (after the keyboard cases): the opener is placed and loads the control route, whose script clicks the anchor with popups disabled. */
    private void startPopupGate() {
        if (opener == null) {
            Log.i(TAG, "wv popup failed no-opener");
            return;
        }
        if (port == 0) {
            Log.i(TAG, "wv popup failed server");
            return;
        }
        phase = PHASE_CONTROL;
        place(opener, OPENER_RECT[0], OPENER_RECT[1], OPENER_RECT[2] - OPENER_RECT[0], OPENER_RECT[3] - OPENER_RECT[1]);
        opener.loadUrl(base() + "/control");
    }

    /** After the child's tap: the opener reloads (a new document start asks its bridge list again); onPageFinished then writes the summary and runs the script open. */
    private void reloadOpener() {
        if (opener == null) {
            return;
        }
        reloadPending = true;
        opener.reload();
        main.postDelayed(new Runnable() {
            @Override
            public void run() {
                if (reloadPending) {
                    Log.i(TAG, "wv popup failed opener-reload");
                }
            }
        }, POPUP_TIMEOUT_MS);
    }

    /** The script open: window.open from evaluateJavascript with popups enabled (AOSP: no user gesture). Refused by onPopup. */
    private void script() {
        if (opener == null) {
            return;
        }
        phase = PHASE_SCRIPT;
        opener.getSettings().setJavaScriptCanOpenWindowsAutomatically(true);
        opener.evaluateJavascript("window.open('" + base() + "/popup?script')", null);
    }

    /** One line: the popup's fields once its target was tapped (the bridge's answer). */
    private void summary() {
        Log.i(TAG, "wv popup summary anchorBounds=" + anchorBounds + " childBounds=" + childBounds + " createCount=" + creates + " gesture=" + gesture
                + " attachedBeforeSend=" + attachedBeforeSend + " childUa=" + childUa + " openerIsNull=" + childOpenerNull + " bridgeCount=" + bridgeReports.get()
                + " openerMarks=" + openerReports.get() + " childTapCount=" + taps.get());
    }

    private String base() {
        return "http://127.0.0.1:" + port;
    }

    /** The loopback server: one thread, one request per connection, `wv popup request <route>` per request. */
    private void startServer() {
        new Thread(new Runnable() {
            @Override
            public void run() {
                try {
                    ServerSocket ss = new ServerSocket(0, 8, InetAddress.getByName("127.0.0.1"));
                    port = ss.getLocalPort();
                    Log.i(TAG, "wv popup server port=" + port);
                    while (true) {
                        serveOne(ss.accept());
                    }
                } catch (Exception e) {
                    Log.i(TAG, "wv popup server failed " + e);
                }
            }
        }, "wv-popup-server").start();
    }

    private static void serveOne(Socket s) {
        try {
            s.setSoTimeout(5000);
            BufferedReader in = new BufferedReader(new InputStreamReader(s.getInputStream(), "US-ASCII"));
            String request = in.readLine();
            String[] parts = request == null ? new String[0] : request.split(" ");
            String path = parts.length > 1 ? parts[1] : "";
            int q = path.indexOf('?');
            String route = q < 0 ? path : path.substring(0, q);
            String ua = "-";
            String header;
            do {
                header = in.readLine();
                if (header != null && header.regionMatches(true, 0, "User-Agent:", 0, 11)) {
                    ua = header.substring(11).trim();
                }
            } while (header != null && header.length() > 0);
            Log.i(TAG, "wv popup request " + route + " ua=" + ua);
            String body = page(route);
            byte[] bytes = (body == null ? "" : body).getBytes("UTF-8");
            String status = body == null ? "404 Not Found" : "200 OK";
            OutputStream out = s.getOutputStream();
            out.write(("HTTP/1.1 " + status + "\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: " + bytes.length + "\r\nConnection: close\r\n\r\n")
                    .getBytes("US-ASCII"));
            out.write(bytes);
            out.flush();
        } catch (Exception e) {
            Log.i(TAG, "wv popup request failed " + e);
        } finally {
            try {
                s.close();
            } catch (Exception e) {
                Log.i(TAG, "wv popup close failed " + e);
            }
        }
    }

    /** The route pages. Opener and control carry one anchor (target=_blank rel=noopener) over the whole page; the popup is solid #ffd800 (CHILD background) and taps through its bridge. */
    private static String page(String route) {
        String anchor = "<a id='a' href='/popup' target='_blank' rel='noopener' style='position:absolute;left:0;top:0;right:0;bottom:0;display:block;background:#2080ff;color:#fff'>open</a>";
        if ("/control".equals(route)) {
            return "<html><head><title>control</title></head><body style='margin:0;background:#204060'>" + anchor
                    + "<script>window.onload=function(){document.getElementById('a').click();document.title='control-clicked';};</script></body></html>";
        }
        if ("/opener".equals(route)) {
            return "<html><head><title>opener</title></head><body style='margin:0;background:#204060'>" + anchor
                    + "<script>if (window.atlBridge) atlBridge.report('opener', navigator.userAgent, String(window.opener === null));</script></body></html>";
        }
        if ("/popup".equals(route)) {
            return "<html><head><title>popup</title></head><body style='margin:0;background:#ffd800' onclick='atlBridge.tap()'>"
                    + "<script>atlBridge.report('child', navigator.userAgent, String(window.opener === null));</script></body></html>";
        }
        return null;
    }

    /** The child's bridge: the page reports once at load (the injection marker, UA, window.opener), and each tap on its target. */
    final class PopupBridge {
        @JavascriptInterface
        public void report(String who, String ua, String openerNull) {
            Log.i(TAG, "wv popup bridge who=" + who + " ua=" + ua + " openerNull=" + openerNull);
            if ("child".equals(who)) {
                childUa = ua;
                childOpenerNull = openerNull;
                bridgeReports.incrementAndGet();
            } else {
                openerReports.incrementAndGet();
            }
        }

        @JavascriptInterface
        public void tap() {
            int n = taps.incrementAndGet();
            Log.i(TAG, "wv popup child tap n=" + n);
            if (n == 1) {
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        reloadOpener();
                    }
                });
            }
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "onResume reached");
    }

    private void place(View v, int left, int top, int width, int height) {
        FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(width, height);
        lp.leftMargin = left;
        lp.topMargin = top;
        root.addView(v, lp);
    }

    /** `left,top,right,bottom` of `v` in the window. */
    private static String rect(View v) {
        int[] at = new int[2];
        v.getLocationInWindow(at);
        return at[0] + "," + at[1] + "," + (at[0] + v.getWidth()) + "," + (at[1] + v.getHeight());
    }
}
