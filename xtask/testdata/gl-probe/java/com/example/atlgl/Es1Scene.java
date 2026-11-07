// SPDX-License-Identifier: MIT
package com.example.atlgl;

import android.graphics.Bitmap;
import android.graphics.Color;
import android.opengl.GLUtils;

import java.nio.Buffer;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.FloatBuffer;
import java.nio.ShortBuffer;
import java.util.ArrayList;
import java.util.List;

import javax.microedition.khronos.opengles.GL10;
import javax.microedition.khronos.opengles.GL11;

/// The ES1 scene's Java half: runs es1-scene.toml's program lines through GL10, as GLSurfaceView's renderer would (xtask/src/gl_scene.rs writes the lines).
/// Each line is one GL call by name; the enum names map to their ES 1.1 values in [`gl`]. `resolve` belongs to the native half and is skipped. Readbacks print as `java row <y> <hex>`.
final class Es1Scene {
    static final int POINTS = 0x0000;
    static final int TRIANGLES = 0x0004;
    static final int FLOAT = 0x1406;
    static final int UNSIGNED_BYTE = 0x1401;
    static final int UNSIGNED_SHORT = 0x1403;
    static final int RGBA = 0x1908;
    static final int COLOR_BUFFER_BIT = 0x4000;
    static final int TEXTURE_2D = 0x0DE1;
    static final int TEXTURE_MIN_FILTER = 0x2801;
    static final int TEXTURE_MAG_FILTER = 0x2800;
    static final int NEAREST = 0x2600;
    private static final char[] HEX = "0123456789abcdef".toCharArray();

    private final GL10 gl;
    /// Buffers the GL reads at draw time: referenced until the scene ends.
    private final List<Buffer> keep = new ArrayList<>();

    private Es1Scene(GL10 gl) {
        this.gl = gl;
    }

    static void run(GL10 gl, String[] ops) {
        Es1Scene s = new Es1Scene(gl);
        for (String op : ops) {
            String[] t = op.trim().split("\\s+");
            if (!t[0].equals("resolve")) s.step(t);
        }
    }

    private void step(String[] t) {
        switch (t[0]) {
            case "clear":
                gl.glClearColor(c(t, 1), c(t, 2), c(t, 3), c(t, 4));
                gl.glClear(COLOR_BUFFER_BIT);
                break;
            case "color":
                gl.glColor4f(c(t, 1), c(t, 2), c(t, 3), c(t, 4));
                break;
            case "enable":
                gl.glEnable(gl(t[1]));
                break;
            case "disable":
                gl.glDisable(gl(t[1]));
                break;
            case "client_enable":
                gl.glEnableClientState(gl(t[1]));
                break;
            case "client_disable":
                gl.glDisableClientState(gl(t[1]));
                break;
            case "scissor":
                gl.glScissor(i(t, 1), i(t, 2), i(t, 3), i(t, 4));
                break;
            case "matrix":
                gl.glMatrixMode(gl(t[1]));
                break;
            case "load_identity":
                gl.glLoadIdentity();
                break;
            case "ortho":
                gl.glOrthof(f(t, 1), f(t, 2), f(t, 3), f(t, 4), f(t, 5), f(t, 6));
                break;
            case "push":
                gl.glPushMatrix();
                break;
            case "pop":
                gl.glPopMatrix();
                break;
            case "translate":
                gl.glTranslatef(f(t, 1), f(t, 2), f(t, 3));
                break;
            case "rotate":
                gl.glRotatef(f(t, 1), f(t, 2), f(t, 3), f(t, 4));
                break;
            case "scale":
                gl.glScalef(f(t, 1), f(t, 2), f(t, 3));
                break;
            case "vertices":
                gl.glVertexPointer(2, FLOAT, 0, floats(t));
                break;
            case "colors":
                gl.glColorPointer(4, UNSIGNED_BYTE, 0, bytes(t));
                break;
            case "texcoords":
                gl.glTexCoordPointer(2, FLOAT, 0, floats(t));
                break;
            case "point_sizes":
                ((GL11) gl).glPointSizePointerOES(FLOAT, 0, floats(t));
                break;
            case "elements": {
                ShortBuffer idx = shorts(t);
                gl.glDrawElements(TRIANGLES, idx.limit(), UNSIGNED_SHORT, idx);
                break;
            }
            case "points":
                gl.glDrawArrays(POINTS, i(t, 1), i(t, 2));
                break;
            case "blend_func":
                gl.glBlendFunc(gl(t[1]), gl(t[2]));
                break;
            case "bind_texture":
                gl.glBindTexture(TEXTURE_2D, i(t, 1));
                break;
            case "texture":
                texture(t);
                break;
            case "texture_filter":
                gl.glTexParameterf(TEXTURE_2D, TEXTURE_MIN_FILTER, NEAREST);
                gl.glTexParameterf(TEXTURE_2D, TEXTURE_MAG_FILTER, NEAREST);
                break;
            case "read":
                read(i(t, 1), i(t, 2));
                break;
            default:
                throw new IllegalArgumentException("unknown step " + t[0]);
        }
    }

    /// ES 1.1 enum values (gl.h, gl2ext.h) by the names the scene uses.
    static int gl(String name) {
        switch (name) {
            case "SCISSOR_TEST": return 0x0C11;
            case "BLEND": return 0x0BE2;
            case "TEXTURE_2D": return 0x0DE1;
            case "VERTEX_ARRAY": return 0x8074;
            case "COLOR_ARRAY": return 0x8076;
            case "TEXTURE_COORD_ARRAY": return 0x8078;
            case "POINT_SIZE_ARRAY_OES": return 0x8B9C;
            case "MODELVIEW": return 0x1700;
            case "PROJECTION": return 0x1701;
            case "SRC_ALPHA": return 0x0302;
            case "ONE_MINUS_SRC_ALPHA": return 0x0303;
            default: throw new IllegalArgumentException("GL name " + name);
        }
    }

    private static float f(String[] t, int i) {
        return Float.parseFloat(t[i]);
    }

    private static int i(String[] t, int i) {
        return Integer.parseInt(t[i]);
    }

    /// A 0-255 channel as the GL's float in [0, 1]: k / 255, the scene's convention.
    private static float c(String[] t, int i) {
        return Integer.parseInt(t[i]) / 255f;
    }

    /// Client-array data: kept referenced, because the GL reads a pointer at every later draw until it is replaced.
    private FloatBuffer floats(String[] t) {
        FloatBuffer b = ByteBuffer.allocateDirect(4 * (t.length - 1)).order(ByteOrder.nativeOrder()).asFloatBuffer();
        for (int k = 1; k < t.length; k++) b.put(Float.parseFloat(t[k]));
        b.position(0);
        keep.add(b);
        return b;
    }

    private ByteBuffer bytes(String[] t) {
        ByteBuffer b = ByteBuffer.allocateDirect(t.length - 1);
        for (int k = 1; k < t.length; k++) b.put((byte) Integer.parseInt(t[k]));
        b.position(0);
        keep.add(b);
        return b;
    }

    private ShortBuffer shorts(String[] t) {
        ShortBuffer b = ByteBuffer.allocateDirect(2 * (t.length - 1)).order(ByteOrder.nativeOrder()).asShortBuffer();
        for (int k = 1; k < t.length; k++) b.put((short) Integer.parseInt(t[k]));
        b.position(0);
        keep.add(b);
        return b;
    }

    /// The texture is a Bitmap uploaded by GLUtils.texImage2D (the path a Java renderer takes); texels run row 0 first, as glTexImage2D reads them.
    private void texture(String[] t) {
        int w = i(t, 1), h = i(t, 2);
        Bitmap bmp = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888);
        for (int y = 0; y < h; y++) {
            for (int x = 0; x < w; x++) {
                int k = 3 + 4 * (y * w + x);
                bmp.setPixel(x, y, Color.argb(i(t, k + 3), i(t, k), i(t, k + 1), i(t, k + 2)));
            }
        }
        GLUtils.texImage2D(TEXTURE_2D, 0, bmp, 0);
        bmp.recycle();
    }

    /// glReadPixels of the whole surface, one `java row <y> <hex>` line per row (row 0 first, RGBA bytes).
    private void read(int w, int h) {
        ByteBuffer buf = ByteBuffer.allocateDirect(w * h * 4).order(ByteOrder.nativeOrder());
        keep.add(buf);
        gl.glReadPixels(0, 0, w, h, RGBA, UNSIGNED_BYTE, buf);
        for (int y = 0; y < h; y++) {
            StringBuilder row = new StringBuilder();
            for (int k = 0; k < w * 4; k++) {
                int v = buf.get(y * w * 4 + k) & 0xff;
                row.append(HEX[v >> 4]).append(HEX[v & 15]);
            }
            Probe.say("java row " + y + " " + row);
        }
    }
}
