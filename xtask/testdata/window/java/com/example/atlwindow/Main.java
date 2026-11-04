// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Activity;
import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.os.Bundle;
import android.util.Log;
import android.view.Choreographer;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.View;
import android.view.WindowManager;

/**
 * The window gate: `xtask test window` (xtask/src/window.rs) waits for the onResume line.
 * The content view paints four solid quadrants and no text: top-left red, top-right green, bottom-left blue, bottom-right white (the frames stage checks these pixels).
 * After onResume the activity takes FRAME_CALLBACKS Choreographer frame callbacks, each logging its frame time (the frames stage checks the cadence).
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
        // the title reaches the window's attributes only through them: Window.setTitle leaves the default (the component name)
        WindowManager.LayoutParams attrs = getWindow().getAttributes();
        attrs.setTitle(WINDOW_TITLE);
        getWindow().setAttributes(attrs);
        setContentView(new Quadrants(this));
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
                Log.i(TAG, "onDraw reached");
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
    }
}
