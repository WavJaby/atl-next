// SPDX-License-Identifier: MIT
package com.example.atles1;

import android.graphics.PixelFormat;
import android.util.Log;
import android.view.Surface;
import android.view.SurfaceControl;

/**
 * `xtask test es1-angle` (gl-honesty H1 spike): one layer with a BLASTBufferQueue, as composer's Client makes it, whose Surface the native lib draws on: our driver's window surface + ES3 context through AOSP libEGL,
 * then an ANGLE display over the driver, an ES1 external context and external surface on top, clear + one quad per frame, swap through libEGL. The compositor's ReportSink digests every buffer; the gate compares.
 * Args: native lib, ANGLE libEGL, ANGLE libGLESv1_CM, our driver, [ "break" ]. Every result is one `AtlEs1: <line>` log line.
 */
public class Client {
    static final String TAG = "AtlEs1";
    static final int SIZE = 64;
    /** Let the compositor latch and report the last buffer, ms. */
    static final long SETTLE_MS = 500;

    static native String draw(Surface surface, String angleEgl, String angleGles1, String driver, boolean broken);

    static void log(String text) {
        Log.i(TAG, text);
    }

    public static void main(String[] args) throws Exception {
        log("start");
        System.load(args[0]);
        boolean broken = args.length > 4 && args[4].equals("break");
        SurfaceControl layer = new SurfaceControl.Builder().setName("atl-es1").setBufferSize(SIZE, SIZE).build();
        Class<?> c = Class.forName("android.graphics.BLASTBufferQueue");
        Object bbq = c.getConstructor(String.class, boolean.class).newInstance("atl-es1-bbq", true);
        c.getMethod("update", SurfaceControl.class, int.class, int.class, int.class).invoke(bbq, layer, SIZE, SIZE, PixelFormat.RGBA_8888);
        Surface surface = (Surface) c.getMethod("createSurface").invoke(bbq);
        new SurfaceControl.Transaction().setVisibility(layer, true).setLayer(layer, 1).apply();
        String result = draw(surface, args[1], args[2], args[3], broken);
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
