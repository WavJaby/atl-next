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
 * The UI-block gate: `xtask test window --stage ui-block` (xtask-harness/src/window/ui_block.rs). A SurfaceView whose own thread posts frames with lockCanvas as fast as buffers come back. The app's UI thread is
 * the window's thread, so what it does sets how fast the window takes frames. Once WARMUP frames are posted:
 * 1. slow phase: for SLOW_MS the UI thread is busy BUSY_MS, then idle IDLE_MS, in turn: the window takes at most one frame per idle gap, slower than the producer, which must be held to the window's rate (composer backpressure), not have its frames replaced unseen;
 * 2. block: a 2 s sleep on the UI thread (ordinary app behaviour: a slow click handler): the window takes nothing, and the producer must not stay blocked: after the composer's bound it latches every vsync.
 * Logs `ub slow start` / `ub slow end` and `ub block start` / `ub block end` around the phases, then `ub result` with the frames posted inside each, the busy cycles run, the longest gap between two posts
 * inside the block and the phases' measured lengths.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    /** Frames posted before the slow phase: the producer runs at its steady rate by then. */
    static final int WARMUP = 60;
    /** The slow phase: long enough for at least two of the composer's once-a-second counters lines inside it. */
    static final long SLOW_MS = 4000;
    /** One busy cycle: the window's thread is busy BUSY_MS and idle IDLE_MS, so it takes at most one frame per 50 ms (20/s) against a 60 Hz producer. */
    static final long BUSY_MS = 40;
    static final long IDLE_MS = 10;
    static final long BLOCK_MS = 2000;
    /** After the block the thread posts this long more, so a producer stuck at the block's end resumes before the result. */
    static final long TAIL_MS = 1000;

    private final Handler ui = new Handler();
    private volatile long slowStart = -1, slowEnd = -1;
    private volatile int slowCycles;
    private volatile long blockStart = -1, blockEnd = -1;
    private volatile boolean slowPosted;
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

    /** One busy cycle of the slow phase; the next is posted after the idle gap, and the block follows the last. */
    private final Runnable busy = new Runnable() {
        @Override
        public void run() {
            long now = SystemClock.uptimeMillis();
            if (now - slowStart >= SLOW_MS) {
                slowEnd = now;
                Log.i(TAG, "ub slow end");
                ui.post(new Runnable() {
                    @Override
                    public void run() {
                        block();
                    }
                });
                return;
            }
            slowCycles++;
            SystemClock.sleep(BUSY_MS);
            ui.postDelayed(this, IDLE_MS);
        }
    };

    private void slow() {
        slowStart = SystemClock.uptimeMillis();
        Log.i(TAG, "ub slow start");
        busy.run();
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
        int posted = 0, during = 0, slowPosts = 0;
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
            if (inside(now, slowStart, slowEnd)) {
                slowPosts++;
            }
            long start = blockStart, end = blockEnd;
            if (inside(now, start, end)) {
                during++;
                if (last >= start) {
                    maxGap = Math.max(maxGap, now - last);
                }
            }
            last = now;
            if (posted == WARMUP && !slowPosted) {
                slowPosted = true;
                ui.post(new Runnable() {
                    @Override
                    public void run() {
                        slow();
                    }
                });
            }
            if (end >= 0 && now - end >= TAIL_MS) {
                Log.i(TAG, "ub result posts_during=" + during + " max_gap_ms=" + maxGap + " block_ms=" + (end - start) + " slow_posts=" + slowPosts + " slow_cycles=" + slowCycles
                        + " slow_ms=" + (slowEnd - slowStart) + " posted=" + posted);
                return;
            }
        }
    }
}
