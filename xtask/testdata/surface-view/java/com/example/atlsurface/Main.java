// SPDX-License-Identifier: MIT
package com.example.atlsurface;

import android.app.Activity;
import android.graphics.Canvas;
import android.graphics.PixelFormat;
import android.os.Bundle;
import android.util.Log;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewTreeObserver;
import android.view.WindowManager;
import android.widget.FrameLayout;

/**
 * The SurfaceView gate: `xtask test window --stage surface-view` (xtask/src/window/surface_view.rs). A software window (no hwui renderer) of colour WINDOW holding, at fixed pixel rectangles:
 * - A: a SurfaceView below the window (the default media sublayer), opaque RGBX_8888, filled with A by lockCanvas; an OVERLAY view of the window over part of it, so the window punches A's hole and draws above it;
 * - B: the same with setZOrderOnTop(true) and RGBA_8888: B covers its overlay;
 * - C: a SurfaceView below the window, opaque RGBX_8888, never drawn: its hole shows its background colour layer (SurfaceView.mBackgroundControl, black).
 * Rectangles are in window pixels, set as layout params so no density rounding applies; after the layout the fixture logs each view's rectangle in the window, and each SurfaceView frame it posts.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    static final String WINDOW_TITLE = "AtlSurfaceFixture";
    static final int WINDOW = 0xff204060;
    static final int A = 0xffc08020;
    static final int B = 0xff20c080;
    static final int OVERLAY = 0xfff0f0f0;

    private FrameLayout root;
    private SurfaceView a, b, c;
    private View overA, overB;
    private boolean laidOut;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        WindowManager.LayoutParams attrs = getWindow().getAttributes();
        attrs.setTitle(WINDOW_TITLE);
        getWindow().setAttributes(attrs);
        root = new FrameLayout(this);
        root.setBackgroundColor(WINDOW);
        a = surface(PixelFormat.RGBX_8888, A, "a");
        b = surface(PixelFormat.RGBA_8888, B, "b");
        b.setZOrderOnTop(true);
        c = surface(PixelFormat.RGBX_8888, 0, "c");
        place(a, 40, 40, 200, 150);
        place(b, 300, 40, 200, 150);
        place(c, 560, 40, 120, 100);
        overA = overlay();
        overB = overlay();
        place(overA, 100, 80, 60, 40);
        place(overB, 360, 80, 60, 40);
        setContentView(root);
        root.getViewTreeObserver().addOnGlobalLayoutListener(new ViewTreeObserver.OnGlobalLayoutListener() {
            @Override
            public void onGlobalLayout() {
                if (laidOut) {
                    return;
                }
                laidOut = true;
                Log.i(TAG, "sv layout window=" + root.getWidth() + "x" + root.getHeight() + " a=" + rect(a) + " b=" + rect(b) + " c=" + rect(c)
                        + " overA=" + rect(overA) + " overB=" + rect(overB));
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

    private View overlay() {
        View v = new View(this);
        v.setBackgroundColor(OVERLAY);
        return v;
    }

    /** `left,top,right,bottom` of `v` in the window. */
    private static String rect(View v) {
        int[] at = new int[2];
        v.getLocationInWindow(at);
        return at[0] + "," + at[1] + "," + (at[0] + v.getWidth()) + "," + (at[1] + v.getHeight());
    }

    /** A SurfaceView of `format` that fills each new surface with `colour` (0: never drawn). */
    private SurfaceView surface(int format, final int colour, final String name) {
        SurfaceView v = new SurfaceView(this);
        v.getHolder().setFormat(format);
        v.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {
                Log.i(TAG, "sv " + name + " created");
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int f, int width, int height) {
                if (colour == 0) {
                    Log.i(TAG, "sv " + name + " left undrawn " + width + "x" + height);
                    return;
                }
                Canvas canvas = holder.lockCanvas();
                if (canvas == null) {
                    Log.i(TAG, "sv " + name + " lockCanvas failed");
                    return;
                }
                canvas.drawColor(colour);
                holder.unlockCanvasAndPost(canvas);
                Log.i(TAG, "sv " + name + " drew " + width + "x" + height);
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                Log.i(TAG, "sv " + name + " destroyed");
            }
        });
        return v;
    }
}
