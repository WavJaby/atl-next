// SPDX-License-Identifier: MIT
package com.example.atlcomposer;

import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.PixelFormat;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;
import android.view.Choreographer;
import android.view.Surface;
import android.view.SurfaceControl;

import java.io.File;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executor;
import java.util.concurrent.TimeUnit;

/**
 * The composer gate's client: `xtask test composer` (xtask/src/composer.rs) runs this class as an app process, so libgui (the framework's SurfaceControl, Choreographer, BLASTBufferQueue) finds the process's own
 * in-app SurfaceFlinger. It does what a window does: layers (SurfaceControl.Builder -> createSurface), a BLASTBufferQueue whose Surface is drawn on with a software Canvas (buffers from the gralloc HAL) and posted once per
 * vsync (setTransactionState with the buffer), transactions that move the layer, a transaction-committed callback (the compositor's call back into libgui's TransactionCompletedListener) and Choreographer's vsync
 * (createDisplayEventConnection, the BitTube). Every result is one `AtlComposer: <what>` line; the gate reads them, and the compositor's own report on stderr (`composer: counters`, `composer: quad`).
 *
 * The pixels of frame N are a formula the gate knows too (xtask/src/composer.rs `expected_pixels`): with (r, g, b) = (37 N, 91 N + 17, 255 - 5 N) mod 256, the top-left quadrant of the 64 x 64 buffer is (r, g, b),
 * the top-right (g, b, r), the bottom-left (b, r, g) and the bottom-right (255 - r, 255 - g, 255 - b), all opaque. Frame 0 is the warm-up's.
 */
public class Client {
    static final String TAG = "AtlComposer";
    /** How many vsync frames to post: the gate wants 20 or more. */
    static final int FRAMES = 30;
    /** The buffer's side, pixels. */
    static final int SIZE = 64;
    /** How long to wait for the frames, ms: 30 frames take half a second at 60 Hz; the rest is for a loaded machine. */
    static final long FRAMES_TIMEOUT_MS = 10_000;
    /** How long to wait for the committed callback, ms. */
    static final long CALLBACK_TIMEOUT_MS = 5_000;
    /** Let the compositor's clock thread apply, release and close what it holds before looking, ms. */
    static final long SETTLE_MS = 500;

    static void log(String text) {
        Log.i(TAG, text);
    }

    /** The open descriptors of this process (the listing's own included, the same every time). */
    static int fdCount() {
        String[] fds = new File("/proc/self/fd").list();
        return fds == null ? -1 : fds.length;
    }

    /** Draws frame {@code n}'s four quadrants and posts the buffer; the milliseconds lockCanvas took. */
    static long post(Surface surface, int n) {
        long start = System.nanoTime();
        Canvas canvas = surface.lockCanvas(null);
        long lockMs = (System.nanoTime() - start) / 1_000_000;
        int r = (37 * n) & 255;
        int g = (91 * n + 17) & 255;
        int b = (255 - 5 * n) & 255;
        Paint paint = new Paint();
        int half = SIZE / 2;
        int[][] quadrants = {
            {r, g, b}, {g, b, r}, {b, r, g}, {255 - r, 255 - g, 255 - b},
        };
        for (int q = 0; q < 4; q++) {
            paint.setColor(Color.rgb(quadrants[q][0], quadrants[q][1], quadrants[q][2]));
            int left = (q % 2) * half;
            int top = (q / 2) * half;
            canvas.drawRect(left, top, left + half, top + half, paint);
        }
        surface.unlockCanvasAndPost(canvas);
        return lockMs;
    }

    /** A BLASTBufferQueue (hidden API, by reflection: the class is the platform's and this process runs in the boot class path) on {@code layer}. */
    static Object newQueue(SurfaceControl layer) throws Exception {
        Class<?> c = Class.forName("android.graphics.BLASTBufferQueue");
        Object bbq = c.getConstructor(String.class, boolean.class).newInstance("atl-bbq", true);
        c.getMethod("update", SurfaceControl.class, int.class, int.class, int.class)
                .invoke(bbq, layer, SIZE, SIZE, PixelFormat.RGBA_8888);
        return bbq;
    }

    static Surface surfaceOf(Object bbq) throws Exception {
        return (Surface) Class.forName("android.graphics.BLASTBufferQueue").getMethod("createSurface").invoke(bbq);
    }

    static void destroy(Object bbq) throws Exception {
        Class.forName("android.graphics.BLASTBufferQueue").getMethod("destroy").invoke(bbq);
    }

    public static void main(String[] args) throws Exception {
        log("start");
        // one vsync thread for the whole run: its Choreographer's BitTube is open before the baseline
        HandlerThread thread = new HandlerThread("atl-vsync-client");
        thread.start();
        final Handler handler = new Handler(thread.getLooper());
        final CountDownLatch ready = new CountDownLatch(1);
        final Choreographer[] choreographer = new Choreographer[1];
        handler.post(new Runnable() {
            @Override
            public void run() {
                choreographer[0] = Choreographer.getInstance();
                ready.countDown();
            }
        });
        ready.await();

        // the warm-up: one whole cycle with one frame, so the connections that stay open for the process (the allocator, the mapper, libgui's services) are open before the baseline
        SurfaceControl warmParent = new SurfaceControl.Builder().setName("atl-warm-parent").setBufferSize(128, 128).build();
        SurfaceControl warmChild = new SurfaceControl.Builder().setName("atl-warm-child").setParent(warmParent).setBufferSize(SIZE, SIZE).build();
        Object warmBbq = newQueue(warmChild);
        Surface warmSurface = surfaceOf(warmBbq);
        new SurfaceControl.Transaction().setVisibility(warmChild, true).apply();
        post(warmSurface, 0);
        Thread.sleep(SETTLE_MS);
        warmSurface.release();
        destroy(warmBbq);
        warmChild.release();
        warmParent.release();
        Thread.sleep(SETTLE_MS);
        int baseline = fdCount();
        log("fds baseline=" + baseline);

        SurfaceControl parent = new SurfaceControl.Builder().setName("atl-parent").setBufferSize(128, 128).build();
        final SurfaceControl child = new SurfaceControl.Builder().setName("atl-child").setParent(parent).setBufferSize(SIZE, SIZE).build();
        log("layers created valid=" + (parent.isValid() && child.isValid()));
        Object bbq = null;
        Surface surface = null;
        try {
            bbq = newQueue(child);
            surface = surfaceOf(bbq);
            log("bbq created surface valid=" + surface.isValid());
        } catch (Throwable t) {
            log("bbq failed: " + t);
        }

        // vsync: FRAMES callbacks, each draws and posts a frame and moves the layer
        final Surface frameSurface = surface;
        final CountDownLatch frames = new CountDownLatch(FRAMES);
        final List<Long> frameTimes = new ArrayList<>();
        final List<Long> wallTimes = new ArrayList<>();
        final long[] maxLockMs = new long[1];
        final CountDownLatch committed = new CountDownLatch(1);
        // no lambdas or method references: the SDK's android.jar lacks LambdaMetafactory.metafactory for javac
        handler.post(new Runnable() {
            @Override
            public void run() {
                final Choreographer c = choreographer[0];
                c.postFrameCallback(new Choreographer.FrameCallback() {
                    @Override
                    public void doFrame(long frameTimeNanos) {
                        frameTimes.add(frameTimeNanos);
                        wallTimes.add(System.nanoTime());
                        int n = frameTimes.size();
                        if (frameSurface != null) {
                            maxLockMs[0] = Math.max(maxLockMs[0], post(frameSurface, n));
                        }
                        SurfaceControl.Transaction t = new SurfaceControl.Transaction();
                        t.setLayer(child, n).setPosition(child, n, 2 * n).setAlpha(child, 1f).setVisibility(child, true);
                        if (n == FRAMES) {
                            t.addTransactionCommittedListener(new Executor() {
                                @Override
                                public void execute(Runnable r) {
                                    r.run();
                                }
                            }, new SurfaceControl.TransactionCommittedListener() {
                                @Override
                                public void onTransactionCommitted() {
                                    committed.countDown();
                                }
                            });
                        }
                        t.apply();
                        t.close();
                        frames.countDown();
                        if (n < FRAMES) {
                            c.postFrameCallback(this);
                        }
                    }
                });
            }
        });
        boolean gotFrames = frames.await(FRAMES_TIMEOUT_MS, TimeUnit.MILLISECONDS);
        log("vsync frames=" + frameTimes.size() + " complete=" + gotFrames);
        log("frames posted=" + (frameSurface != null ? frameTimes.size() : 0) + " max_lock_ms=" + maxLockMs[0]);
        if (frameTimes.size() > 1) {
            int last = frameTimes.size() - 1;
            long frameSpan = frameTimes.get(last) - frameTimes.get(0);
            long wallSpan = wallTimes.get(last) - wallTimes.get(0);
            log("vsync mean_frame_interval_us=" + (frameSpan / last / 1000) + " mean_wall_interval_us=" + (wallSpan / last / 1000));
        }
        boolean gotCallback = committed.await(CALLBACK_TIMEOUT_MS, TimeUnit.MILLISECONDS);
        log("committed callback fired=" + gotCallback);
        // the last frame is shown for a few ticks before the window closes
        Thread.sleep(SETTLE_MS);

        // teardown, the way a window's goes: the Surface and its queue first, then the layers
        if (surface != null) {
            surface.release();
        }
        if (bbq != null) {
            destroy(bbq);
        }
        child.release();
        parent.release();
        Thread.sleep(SETTLE_MS);
        log("fds after=" + fdCount());
        log("done");
        thread.quitSafely();
        System.exit(0);
    }
}
