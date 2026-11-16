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
 * The WebView composition gate: `xtask test window --stage webview` (xtask-harness/src/window/webview.rs). A software window of colour WINDOW holding, at fixed pixel rectangles, a WebView whose page
 * is one colour (PAGE) and an OVERLAY view of the window over part of it: the page shows in the WebView's rectangle at the WebView's depth, the overlay above it.
 * Rectangles are in window pixels, set as layout params so no density rounding applies; after the layout the fixture logs each view's rectangle in the window, and the page's onPageFinished.
 * A WebView that cannot be made (no provider, no engine) is logged and left out, so the gate's picture shows the window there.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    static final String WINDOW_TITLE = "AtlWebViewFixture";
    static final int WINDOW = 0xff204060;
    static final int OVERLAY = 0xfff0f0f0;
    /** PAGE's colour 0x3070c0 as the page's CSS. Not as `#3070c0`: loadData takes the content as a URL, so a bare '#' starts its fragment and cuts the page (WebView.loadData doc: escape it as %23). */
    static final String PAGE_HTML = "<html><body style=\"margin:0;background:rgb(48,112,192)\"></body></html>";

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
        try {
            WebView w = new WebView(this);
            w.setWebViewClient(new WebViewClient() {
                @Override
                public void onPageFinished(WebView view, String url) {
                    Log.i(TAG, "wv page finished");
                }
            });
            w.loadData(PAGE_HTML, "text/html", null);
            web = w;
        } catch (Throwable t) {
            Log.i(TAG, "wv no webview: " + t);
            web = new View(this);
        }
        place(web, 40, 40, 300, 200);
        over = new View(this);
        over.setBackgroundColor(OVERLAY);
        place(over, 100, 80, 60, 40);
        setContentView(root);
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
