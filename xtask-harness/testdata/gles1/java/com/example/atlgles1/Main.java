// SPDX-License-Identifier: MIT
package com.example.atlgles1;

import android.app.Activity;
import android.opengl.GLSurfaceView;
import android.os.Bundle;
import android.util.Log;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.FloatBuffer;

import javax.microedition.khronos.egl.EGLConfig;
import javax.microedition.khronos.opengles.GL10;

/**
 * GLES1 fixture: GLSurfaceView with no setEGLContextClientVersion call, so its context is ES 1 (GLSurfaceView's default), every GL call through GL10.
 * Logs `AtlGles1: version <GL_VERSION>`, then after the first frame reads back {@link #PROBES}: `AtlGles1: readback pass` or one `AtlGles1: readback FAIL ...` line per wrong pixel.
 */
public class Main extends Activity {
    static final String TAG = "AtlGles1";

    /**
     * Expected pixels, derived from the scene in {@link Renderer#onDrawFrame}, not by running it.
     * Viewport 64x64 at the window origin, identity matrices: window pixel (x, y) has its centre at NDC ((x + 0.5) / 32 - 1, (y + 0.5) / 32 - 1) (ES 1.1 2.11 viewport transform).
     * - (8, 8): NDC (-0.73, -0.73), x + y < 0 -> inside the triangle (-1,-1) (1,-1) (-1,1) -> its colour (1, 0, 0, 1) -> 255, 0, 0, 255.
     * - (56, 56): NDC (0.77, 0.77), x + y > 0 -> outside -> the clear colour (64, 128, 192, 255)/255 -> 64, 128, 192, 255.
     * Both centres are >= 0.25 pixel from the triangle's edge, so the ES 1.1 3.5.1 coverage rule gives one answer.
     */
    static final int[][] PROBES = {
        {8, 8, 0xff0000ff},
        {56, 56, 0x4080c0ff},
    };

    /** Per-channel slack: the clear colour goes float -> unorm8, its rounding left to the implementation (ES 1.1 2.12.9). */
    static final int SLACK = 1;

    static boolean matches(int got, int want) {
        for (int shift = 0; shift < 32; shift += 8) {
            if (Math.abs(((got >>> shift) & 0xff) - ((want >>> shift) & 0xff)) > SLACK) {
                return false;
            }
        }
        return true;
    }

    static final class Renderer implements GLSurfaceView.Renderer {
        private final FloatBuffer triangle;
        private final ByteBuffer pixel = ByteBuffer.allocateDirect(4).order(ByteOrder.nativeOrder());
        private boolean checked;

        Renderer() {
            float[] xy = {-1f, -1f, 1f, -1f, -1f, 1f};
            triangle = ByteBuffer.allocateDirect(xy.length * 4).order(ByteOrder.nativeOrder()).asFloatBuffer();
            triangle.put(xy).position(0);
        }

        @Override
        public void onSurfaceCreated(GL10 gl, EGLConfig config) {
            Log.i(TAG, "version " + gl.glGetString(GL10.GL_VERSION));
        }

        @Override
        public void onSurfaceChanged(GL10 gl, int width, int height) {
            Log.i(TAG, "surface " + width + "x" + height);
        }

        @Override
        public void onDrawFrame(GL10 gl) {
            gl.glViewport(0, 0, 64, 64);
            gl.glClearColor(64 / 255f, 128 / 255f, 192 / 255f, 1f);
            gl.glClear(GL10.GL_COLOR_BUFFER_BIT);
            gl.glMatrixMode(GL10.GL_PROJECTION);
            gl.glLoadIdentity();
            gl.glMatrixMode(GL10.GL_MODELVIEW);
            gl.glLoadIdentity();
            gl.glEnableClientState(GL10.GL_VERTEX_ARRAY);
            gl.glVertexPointer(2, GL10.GL_FLOAT, 0, triangle);
            gl.glColor4f(1f, 0f, 0f, 1f);
            gl.glDrawArrays(GL10.GL_TRIANGLES, 0, 3);
            if (checked) {
                return;
            }
            checked = true;
            int error = gl.glGetError();
            if (error != GL10.GL_NO_ERROR) {
                Log.e(TAG, String.format("readback FAIL gl error 0x%x", error));
                return;
            }
            int bad = 0;
            for (int[] p : PROBES) {
                pixel.position(0);
                gl.glReadPixels(p[0], p[1], 1, 1, GL10.GL_RGBA, GL10.GL_UNSIGNED_BYTE, pixel);
                int got = ((pixel.get(0) & 0xff) << 24) | ((pixel.get(1) & 0xff) << 16) | ((pixel.get(2) & 0xff) << 8) | (pixel.get(3) & 0xff);
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
        // no setEGLContextClientVersion: the ES 1 default is what this fixture tests. RGBA8, no depth/stencil: the expected values assume 8 bits per channel
        view.setEGLConfigChooser(8, 8, 8, 8, 0, 0);
        view.setRenderer(new Renderer());
        view.setRenderMode(GLSurfaceView.RENDERMODE_WHEN_DIRTY);
        setContentView(view);
    }
}
