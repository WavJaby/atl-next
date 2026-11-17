// SPDX-License-Identifier: MIT
package com.example.atlwebview;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.view.View;
import android.view.ViewTreeObserver;
import android.view.WindowManager;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;

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
