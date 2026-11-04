// SPDX-License-Identifier: MIT
package com.example.atlcomposer;

import android.graphics.PixelFormat;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;
import android.view.Choreographer;
import android.view.Surface;
import android.view.SurfaceControl;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executor;
import java.util.concurrent.TimeUnit;

/**
 * The composer gate's client: `xtask test composer` (xtask/src/composer.rs) runs this class as an app process, so libgui (the framework's SurfaceControl, Choreographer) finds the process's own
 * in-app SurfaceFlinger. It uses only what a window does: layers (SurfaceControl.Builder -> createSurface), transactions (apply -> setTransactionState), a transaction-committed callback
 * (the compositor's call back into libgui's TransactionCompletedListener) and Choreographer's vsync (createDisplayEventConnection, the BitTube).
 * Every result is one `AtlComposer: <what>` line; the gate reads them and the compositor's own counters line (`composer: counters ...`, the process's stderr).
 */
public class Client {
    static final String TAG = "AtlComposer";
    /** How many vsync frames to observe: the gate wants 20 or more. */
    static final int FRAMES = 30;
    /** How long to wait for the frames, ms: 30 frames take half a second at 60 Hz; the rest is for a loaded machine. */
    static final long FRAMES_TIMEOUT_MS = 10_000;
    /** How long to wait for the committed callback, ms. */
    static final long CALLBACK_TIMEOUT_MS = 5_000;
    /** Let the compositor's clock thread go idle and print its counters before the process ends, ms. */
    static final long SETTLE_MS = 500;

    static void log(String text) {
        Log.i(TAG, text);
    }

    public static void main(String[] args) throws Exception {
        log("start");
        SurfaceControl parent = new SurfaceControl.Builder().setName("atl-parent").setBufferSize(128, 128).build();
        SurfaceControl child = new SurfaceControl.Builder().setName("atl-child").setParent(parent).setBufferSize(64, 64).build();
        log("layers created valid=" + (parent.isValid() && child.isValid()));

        // BLASTBufferQueue (hidden API, reached by reflection: the class is the platform's and this process runs in the boot class path): its native side asks the compositor for
        // getMaxAcquiredBufferCount, opens a BufferReleaseChannel and applies its own transactions. It stops there: the first Surface.lockCanvas would allocate a buffer, which needs a gralloc allocator (M3 slice 2).
        Object bbq = null;
        Surface surface = null;
        try {
            Class<?> bbqClass = Class.forName("android.graphics.BLASTBufferQueue");
            bbq = bbqClass.getConstructor(String.class, boolean.class).newInstance("atl-bbq", true);
            bbqClass.getMethod("update", SurfaceControl.class, int.class, int.class, int.class)
                    .invoke(bbq, child, 64, 64, PixelFormat.RGBA_8888);
            surface = (Surface) bbqClass.getMethod("createSurface").invoke(bbq);
            log("bbq created surface valid=" + surface.isValid());
        } catch (Throwable t) {
            log("bbq failed: " + t);
        }

        // vsync: Choreographer on a looper thread, FRAMES callbacks, each also a transaction that moves the child
        HandlerThread thread = new HandlerThread("atl-vsync-client");
        thread.start();
        final CountDownLatch frames = new CountDownLatch(FRAMES);
        final List<Long> frameTimes = new ArrayList<>();
        final List<Long> wallTimes = new ArrayList<>();
        final CountDownLatch committed = new CountDownLatch(1);
        // no lambdas or method references: the SDK's android.jar lacks LambdaMetafactory.metafactory for javac
        new Handler(thread.getLooper()).post(new Runnable() {
            @Override
            public void run() {
                final Choreographer choreographer = Choreographer.getInstance();
                choreographer.postFrameCallback(new Choreographer.FrameCallback() {
                    @Override
                    public void doFrame(long frameTimeNanos) {
                        frameTimes.add(frameTimeNanos);
                        wallTimes.add(System.nanoTime());
                        int n = frameTimes.size();
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
                            choreographer.postFrameCallback(this);
                        }
                    }
                });
            }
        });
        boolean gotFrames = frames.await(FRAMES_TIMEOUT_MS, TimeUnit.MILLISECONDS);
        log("vsync frames=" + frameTimes.size() + " complete=" + gotFrames);
        if (frameTimes.size() > 1) {
            int last = frameTimes.size() - 1;
            long frameSpan = frameTimes.get(last) - frameTimes.get(0);
            long wallSpan = wallTimes.get(last) - wallTimes.get(0);
            log("vsync mean_frame_interval_us=" + (frameSpan / last / 1000) + " mean_wall_interval_us=" + (wallSpan / last / 1000));
        }
        boolean gotCallback = committed.await(CALLBACK_TIMEOUT_MS, TimeUnit.MILLISECONDS);
        log("committed callback fired=" + gotCallback);

        // the Surface holds the queue, and the queue the layer: let them go first, as a window's teardown does
        if (surface != null) {
            surface.release();
        }
        if (bbq != null) {
            Class.forName("android.graphics.BLASTBufferQueue").getMethod("destroy").invoke(bbq);
        }
        child.release();
        parent.release();
        Thread.sleep(SETTLE_MS);
        log("done");
        thread.quitSafely();
        System.exit(0);
    }
}
