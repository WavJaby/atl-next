// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Activity;
import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Rect;
import android.os.Bundle;
import android.util.Log;
import android.view.Choreographer;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;
import android.view.WindowManager;

/**
 * The window gate: `xtask test window` (xtask/src/window.rs) waits for the onResume line.
 * The content view paints four solid quadrants and no text: top-left red, top-right green, bottom-left blue, bottom-right white (the frames stage checks these pixels).
 * After onResume the activity takes FRAME_CALLBACKS Choreographer frame callbacks, each logging its frame time (the frames stage checks the cadence).
 * Each touch the content view gets is logged (the input stage checks it against the injected pointer script).
 * Each layout of the content view logs its size, the window's and the display's bounds and screenWidthDp (the resize stage checks them against the configures it injects).
 * Each marker is logged once, when it is first reached.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    /** The window's title: the compositor's layer name, by which the frames stage finds the first frame (xtask/src/window.rs WINDOW_TITLE). */
    static final String WINDOW_TITLE = "AtlWindowFixture";
    /** Frame callbacks per run (xtask/src/window.rs FRAME_CALLBACKS). */
    static final int FRAME_CALLBACKS = 31;

    private int frameCallbacks;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "onCreate reached");
        logDensity("created", getResources().getConfiguration());
        // the title reaches the window's attributes only through them: Window.setTitle leaves the default (the component name)
        WindowManager.LayoutParams attrs = getWindow().getAttributes();
        attrs.setTitle(WINDOW_TITLE);
        getWindow().setAttributes(attrs);
        setContentView(new Quadrants(this));
    }

    /** A start that reused this instance (singleTop in window-start): the start-finish stage's new-intent sub-gate reads this line. */
    @Override
    protected void onNewIntent(android.content.Intent intent) {
        super.onNewIntent(intent);
        Log.i(TAG, "onNewIntent reached again=" + intent.getBooleanExtra("again", false));
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "onResume reached");
        // the virtual keyboard's ground truth (PhoneWindow.preparePanel loads this map on every resume)
        KeyCharacterMap map = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD);
        int keyA = map.get(KeyEvent.KEYCODE_A, 0);
        Log.i(TAG, "keymap keyboardType=" + map.getKeyboardType() + " keyA=" + (char) keyA
                + (keyA == 'a' ? " ok" : " wrong"));
        postFrame();
    }

    /** A configuration change delivered without a relaunch (ActivityConfigurationChangeItem): one line each, with the screenWidthDp it now has. */
    @Override
    public void onConfigurationChanged(android.content.res.Configuration newConfig) {
        super.onConfigurationChanged(newConfig);
        Log.i(TAG, "configuration changed screenWidthDp=" + newConfig.screenWidthDp);
        logDensity("changed", newConfig);
    }

    /** The density this instance sees (xtask/src/window.rs move_checks): the Configuration's and the resources' DisplayMetrics, which must agree. */
    private void logDensity(String when, android.content.res.Configuration config) {
        Log.i(TAG, "density " + when + " config=" + config.densityDpi
                + " metrics=" + getResources().getDisplayMetrics().densityDpi);
    }

    /** One frame callback per vsync, FRAME_CALLBACKS in all; each logs the frame time it was given (CLOCK_MONOTONIC ns). */
    private void postFrame() {
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                frameCallbacks++;
                Log.i(TAG, "frame " + frameCallbacks + " time=" + frameTimeNanos);
                if (frameCallbacks < FRAME_CALLBACKS) {
                    postFrame();
                }
            }
        });
    }

    /** Four solid colours, one per quadrant; no text. */
    static class Quadrants extends View {
        private final Paint paint = new Paint();
        private boolean drawn;

        Quadrants(Context context) {
            super(context);
        }

        @Override
        protected void onDraw(Canvas canvas) {
            super.onDraw(canvas);
            if (!drawn) {
                drawn = true;
                // hardware=true: a RecordingCanvas, so hwui's RenderThread draws it (stage hwui checks this)
                Log.i(TAG, "onDraw reached hardware=" + canvas.isHardwareAccelerated());
            }
            int halfWidth = getWidth() / 2;
            int halfHeight = getHeight() / 2;
            int right = getWidth();
            int bottom = getHeight();
            paint.setStyle(Paint.Style.FILL);
            paint.setColor(Color.RED);
            canvas.drawRect(0, 0, halfWidth, halfHeight, paint);
            paint.setColor(Color.GREEN);
            canvas.drawRect(halfWidth, 0, right, halfHeight, paint);
            paint.setColor(Color.BLUE);
            canvas.drawRect(0, halfHeight, halfWidth, bottom, paint);
            paint.setColor(Color.WHITE);
            canvas.drawRect(halfWidth, halfHeight, right, bottom, paint);
        }

        /** One line per size change (xtask/src/window.rs resize_checks): the content size, screenWidthDp, the window's bounds and the display's. */
        @Override
        protected void onSizeChanged(int w, int h, int oldw, int oldh) {
            super.onSizeChanged(w, h, oldw, oldh);
            WindowManager wm = getContext().getSystemService(WindowManager.class);
            Rect window = wm.getCurrentWindowMetrics().getBounds();
            Rect display = wm.getMaximumWindowMetrics().getBounds();
            Log.i(TAG, "size=" + w + "x" + h + " screenWidthDp=" + getResources().getConfiguration().screenWidthDp
                    + " window=" + window.width() + "x" + window.height()
                    + " display=" + display.width() + "x" + display.height());
        }

        /** Logs each touch: the action, the pointer's x and y, the pointer count (xtask/src/window.rs input_check). An UP past BLOCK_MIN_X sleeps BLOCK_MS on the UI thread: the gtk stage's liveness probe expects no pong meanwhile. */
        @Override
        public boolean onTouchEvent(MotionEvent event) {
            Log.i(TAG, "touch action=" + event.getActionMasked() + " x=" + event.getX() + " y=" + event.getY() + " pointers=" + event.getPointerCount());
            if (event.getActionMasked() == MotionEvent.ACTION_UP && event.getX() > BLOCK_MIN_X) {
                try {
                    Thread.sleep(BLOCK_MS);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
            return true;
        }

        static final float BLOCK_MIN_X = 500f;
        static final long BLOCK_MS = 2000L;
    }
}
