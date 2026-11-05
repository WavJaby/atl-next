// SPDX-License-Identifier: MIT
package com.example.atlgles2;

import android.app.Activity;
import android.opengl.GLSurfaceView;
import android.os.Bundle;
import android.util.Log;

import javax.microedition.khronos.egl.EGLConfig;
import javax.microedition.khronos.opengles.GL10;

/**
 * GLES2 fixture: GLSurfaceView (framework EGL) on the Java side, every GL call in libatl-gles2.so, as Cut the Rope's libctro does.
 * After the first frame the renderer reads back {@link #PROBES} and logs `AtlGles2: readback pass` or one `AtlGles2: readback FAIL ...` line per wrong pixel.
 */
public class Main extends Activity {
    static final String TAG = "AtlGles2";

    static {
        System.loadLibrary("atl-gles2");
    }

    /** Compiles the shaders, uploads the texture; false = a GL error, logged by the library. */
    static native boolean nativeInit();

    /** Draws the scene (atl_gles2.c) into the 64x64 viewport at the window origin. */
    static native void nativeDraw();

    /** glReadPixels of one RGBA8 pixel at window (x, y), packed r<<24 | g<<16 | b<<8 | a. */
    static native int nativeReadPixel(int x, int y);

    /**
     * Expected pixels, derived from the scene definition in atl_gles2.c's header comment, not by running it.
     * Window pixel (x, y) has its centre at NDC ((x + 0.5) / 32 - 1, (y + 0.5) / 32 - 1) in the 64x64 viewport (GLES 2.0 2.12 viewport transform).
     * - (8, 8): NDC (-0.73, -0.73), x + y < 0 -> inside the triangle (-1,-1) (1,-1) (-1,1) -> its uniform colour (1, 0, 0, 1) -> 255, 0, 0, 255.
     * - (24, 56): NDC (-0.23, 0.77), x + y > 0 and x < 0 -> neither triangle nor quad -> clear colour (64, 128, 192, 255)/255 -> 64, 128, 192, 255.
     * - quad [0,1]^2 samples the 2x2 texture with texcoord = NDC position, GL_NEAREST -> texel (floor(2s), floor(2t)); row 0 = first uploaded row (t = 0):
     *   (40, 40): s = t = 0.27 -> texel (0,0) green; (56, 40): s = 0.77 -> (1,0) blue; (40, 56): (0,1) white; (56, 56): (1,1) yellow.
     * Every centre is >= 0.25 pixel from a primitive edge or texel boundary, so the GLES 2.0 3.5.1 coverage rule and filtering give one answer.
     */
    static final int[][] PROBES = {
        {8, 8, 0xff0000ff},
        {24, 56, 0x4080c0ff},
        {40, 40, 0x00ff00ff},
        {56, 40, 0x0000ffff},
        {40, 56, 0xffffffff},
        {56, 56, 0xffff00ff},
    };

    /**
     * Per-channel slack: the clear colour 64/255 etc. goes float -> unorm8, and GLES 2.0 2.1.2 leaves that rounding to the implementation (64/255 * 255 may land at 63.99...).
     * Texel and 0/1 values convert exactly; one shared slack keeps the check simple.
     */
    static final int SLACK = 1;

    static boolean matches(int got, int want) {
        for (int shift = 0; shift < 32; shift += 8) {
            int g = (got >>> shift) & 0xff;
            int w = (want >>> shift) & 0xff;
            if (Math.abs(g - w) > SLACK) {
                return false;
            }
        }
        return true;
    }

    static final class Renderer implements GLSurfaceView.Renderer {
        private boolean ready;
        private boolean checked;

        @Override
        public void onSurfaceCreated(GL10 unused, EGLConfig config) {
            ready = nativeInit();
        }

        @Override
        public void onSurfaceChanged(GL10 unused, int width, int height) {
            Log.i(TAG, "surface " + width + "x" + height);
        }

        @Override
        public void onDrawFrame(GL10 unused) {
            if (!ready) {
                return;
            }
            nativeDraw();
            if (checked) {
                return;
            }
            checked = true;
            int bad = 0;
            for (int[] p : PROBES) {
                int got = nativeReadPixel(p[0], p[1]);
                if (!matches(got, p[2])) {
                    bad++;
                    Log.e(TAG, String.format("readback FAIL (%d,%d): got %08x want %08x", p[0], p[1], got, p[2]));
                }
            }
            if (bad == 0) {
                Log.i(TAG, "readback pass");
            }
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        GLSurfaceView view = new GLSurfaceView(this);
        view.setEGLContextClientVersion(2);
        // RGBA8, no depth/stencil: the expected values assume 8 bits per channel
        view.setEGLConfigChooser(8, 8, 8, 8, 0, 0);
        view.setRenderer(new Renderer());
        view.setRenderMode(GLSurfaceView.RENDERMODE_WHEN_DIRTY);
        setContentView(view);
    }
}
