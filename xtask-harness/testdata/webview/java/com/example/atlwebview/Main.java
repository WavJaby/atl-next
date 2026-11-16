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
            + " onkeydown=\"window.k=(window.k||0)+1;if(event.key=='Escape'&&window.k==1)event.preventDefault();document.title='key'+window.k+event.key+(event.defaultPrevented?'-prevented':'')\"></body></html>";

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
            w.setWebViewClient(new WebViewClient() {
                @Override
                public void onPageFinished(WebView view, String url) {
                    Log.i(TAG, "wv page finished");
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
