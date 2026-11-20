// SPDX-License-Identifier: MIT
package com.example.atlblock;

import android.app.Activity;
import android.graphics.Canvas;
import android.graphics.PixelFormat;
import android.os.Bundle;
import android.os.Handler;
import android.os.SystemClock;
import android.util.Log;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.widget.FrameLayout;

/**
 * The fixture of `xtask test window --stage ui-block` and `--stage slow-display` (xtask-harness/src/window/{ui_block,slow_display}.rs). A SurfaceView whose own thread posts frames with lockCanvas as
 * fast as buffers come back. Once WARMUP frames are posted:
 * 1. steady phase: for STEADY_MS the UI thread is idle; the producer runs at the display's rate (slow-display: the harness slows the display, and the producer must be held to it, not have its frames
 *    replaced unseen);
 * 2. block: a 2 s sleep on the UI thread (ordinary app behaviour: a slow click handler): the producer must not stay blocked: after the composer's bound it latches every vsync.
 * Logs `ub steady start` / `ub steady end` and `ub block start` / `ub block end` around the phases, then `ub result` with the frames posted inside each, the longest gap between two posts inside the block
 * and the phases' measured lengths.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    /** Frames posted before the slow phase: the producer runs at its steady rate by then. */
    static final int WARMUP = 60;
    /** The steady phase: long enough for at least two of the composer's once-a-second counters lines inside it. */
    static final long STEADY_MS = 4000;
    static final long BLOCK_MS = 2000;
    /** After the block the thread posts this long more, so a producer stuck at the block's end resumes before the result. */
    static final long TAIL_MS = 1000;

    private final Handler ui = new Handler();
    private volatile long steadyStart = -1, steadyEnd = -1;
    private volatile long blockStart = -1, blockEnd = -1;
    private volatile boolean steadyPosted;
    private Thread drawer;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        FrameLayout root = new FrameLayout(this);
        root.setBackgroundColor(0xff204060);
        SurfaceView sv = new SurfaceView(this);
        sv.getHolder().setFormat(PixelFormat.RGBX_8888);
        sv.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(final SurfaceHolder holder) {
                drawer = new Thread(new Runnable() {
                    @Override
                    public void run() {
                        draw(holder);
                    }
                }, "ub-drawer");
                drawer.start();
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int f, int width, int height) {
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
            }
        });
        FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(200, 150);
        lp.leftMargin = 40;
        lp.topMargin = 40;
        root.addView(sv, lp);
        setContentView(root);
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "onResume reached");
    }

    /** The steady phase's end; the block follows. */
    private final Runnable steadyEndRun = new Runnable() {
        @Override
        public void run() {
            steadyEnd = SystemClock.uptimeMillis();
            Log.i(TAG, "ub steady end");
            block();
        }
    };

    private void steady() {
        steadyStart = SystemClock.uptimeMillis();
        Log.i(TAG, "ub steady start");
        ui.postDelayed(steadyEndRun, STEADY_MS);
    }

    private void block() {
        blockStart = SystemClock.uptimeMillis();
        Log.i(TAG, "ub block start");
        SystemClock.sleep(BLOCK_MS);
        blockEnd = SystemClock.uptimeMillis();
        Log.i(TAG, "ub block end");
    }

    private static boolean inside(long now, long start, long end) {
        return start >= 0 && now >= start && (end < 0 || now <= end);
    }

    private void draw(SurfaceHolder holder) {
        int posted = 0, during = 0, steadyPosts = 0;
        long last = -1, maxGap = 0;
        while (true) {
            Canvas canvas = holder.lockCanvas();
            if (canvas == null) {
                Log.i(TAG, "ub lockCanvas failed after " + posted);
                return;
            }
            canvas.drawColor((posted & 1) == 0 ? 0xffc08020 : 0xff20c080);
            holder.unlockCanvasAndPost(canvas);
            posted++;
            long now = SystemClock.uptimeMillis();
            if (inside(now, steadyStart, steadyEnd)) {
                steadyPosts++;
            }
            long start = blockStart, end = blockEnd;
            if (inside(now, start, end)) {
                during++;
                if (last >= start) {
                    maxGap = Math.max(maxGap, now - last);
                }
            }
            last = now;
            if (posted == WARMUP && !steadyPosted) {
                steadyPosted = true;
                ui.post(new Runnable() {
                    @Override
                    public void run() {
                        steady();
                    }
                });
            }
            if (end >= 0 && now - end >= TAIL_MS) {
                Log.i(TAG, "ub result posts_during=" + during + " max_gap_ms=" + maxGap + " block_ms=" + (end - start) + " steady_posts=" + steadyPosts
                        + " steady_ms=" + (steadyEnd - steadyStart) + " posted=" + posted);
                return;
            }
        }
    }
}
