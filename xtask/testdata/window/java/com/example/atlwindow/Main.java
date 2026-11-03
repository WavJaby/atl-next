// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Activity;
import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.os.Bundle;
import android.util.Log;
import android.view.View;

/**
 * The window gate: `xtask test window` (xtask/src/window.rs) waits for the onResume line.
 * The content view paints four solid quadrants and no text: top-left red, top-right green, bottom-left blue, bottom-right white (later stages check these pixels).
 * Each marker is logged once, when it is first reached.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "onCreate reached");
        setContentView(new Quadrants(this));
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "onResume reached");
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
