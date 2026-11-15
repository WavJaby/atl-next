// SPDX-License-Identifier: MIT
package com.example.atles1;

import android.graphics.PixelFormat;
import android.util.Log;
import android.view.Surface;
import android.view.SurfaceControl;

/**
 * `xtask test es1-angle`: one layer with a BLASTBufferQueue, as composer's Client makes it, whose Surface the native lib draws on, clear + one quad per frame, swap through libEGL. The compositor's ReportSink digests every buffer; the gate compares.
 * Modes: `spike` / `break` (gl-honesty H1: an ANGLE display over our driver, an ES1 external context on our ES3 carrier, made from outside the driver; `break` clears the carrier first);
 * `driver` (H2a: an ES1 context straight from AOSP libEGL, the driver's own ES1 kind, drawn through the libGLESv1_CM front; then an ES3 context in the same process and a cross-kind share context).
 * Args: native lib, ANGLE libEGL, ANGLE libGLESv1_CM, our driver, mode. Every result is one `AtlEs1: <line>` log line.
 */
public class Client {
    static final String TAG = "AtlEs1";
    static final int SIZE = 64;
    /** Let the compositor latch and report the last buffer, ms. */
    static final long SETTLE_MS = 500;

    static native String draw(Surface surface, String angleEgl, String angleGles1, String driver, String mode);

    static void log(String text) {
        Log.i(TAG, text);
    }

    public static void main(String[] args) throws Exception {
        log("start");
        System.load(args[0]);
        String mode = args.length > 4 ? args[4] : "spike";
        SurfaceControl layer = new SurfaceControl.Builder().setName("atl-es1").setBufferSize(SIZE, SIZE).build();
        Class<?> c = Class.forName("android.graphics.BLASTBufferQueue");
        Object bbq = c.getConstructor(String.class, boolean.class).newInstance("atl-es1-bbq", true);
        c.getMethod("update", SurfaceControl.class, int.class, int.class, int.class).invoke(bbq, layer, SIZE, SIZE, PixelFormat.RGBA_8888);
        Surface surface = (Surface) c.getMethod("createSurface").invoke(bbq);
        new SurfaceControl.Transaction().setVisibility(layer, true).setLayer(layer, 1).apply();
        String result = draw(surface, args[1], args[2], args[3], mode);
        for (String line : result.split("\n")) {
            log(line);
        }
        Thread.sleep(SETTLE_MS);
        surface.release();
        c.getMethod("destroy").invoke(bbq);
        layer.release();
        log("done");
        System.exit(0);
    }
}
