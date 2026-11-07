// SPDX-License-Identifier: MIT
package com.example.atlgl;

import android.opengl.EGL14;
import android.opengl.EGLConfig;
import android.opengl.EGLContext;
import android.opengl.EGLDisplay;
import android.opengl.EGLExt;
import android.opengl.EGLSurface;
import android.opengl.GLES10;
import android.opengl.GLES20;
import android.util.Log;

import javax.microedition.khronos.egl.EGL10;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.FloatBuffer;

/// `xtask test gl`'s driver, two modes. clear: a GLES3 pbuffer through the framework's EGL14 / GLES20 (AOSP libEGL + libGLESv2 -> the driver), cleared to the colour given as four bytes and read back;
/// then glGetInteger64vEXT, a name outside AOSP's entries.in, through eglGetProcAddress (native lib); then the timed scene ([`scene`]). es1: the ES1 scene (es1-scene.toml) through EGL10 + GL10 (Es1Scene) and through libGLESv1_CM.so (native lib).
/// Prints one `gl-probe:` line per result; the gate judges them. Args: path of the native lib, mode, then clear: w, h, r, g, b, a, scene w, scene h, frames, texel r, g, b, a [, "break"]; es1: w, h, the program lines.
/// Break (clear): no glClear, no draw in the timed frames, and the extension call goes to a no-op instead (what a dead forwarder does).
public class Probe {
    private static final String TAG = "gl-probe";
    /// The scene's grid: GRID x GRID quads, two triangles each.
    static final int GRID = 4;
    /// The fragment loop's iterations; the loop divides each term by the same count, so its sum is the texel (up to float rounding).
    static final int LOOP = 64;
    static final String VERTEX = "attribute vec2 aPos;\nattribute vec2 aUv;\nvarying vec2 vUv;\nvoid main() {\ngl_Position = vec4(aPos, 0.0, 1.0);\nvUv = aUv;\n}\n";
    static final String FRAGMENT = "precision highp float;\nuniform sampler2D uTex;\nvarying vec2 vUv;\nvoid main() {\nvec4 c = texture2D(uTex, vUv);\nfloat a = 0.0;\nfor (int i = 0; i < " + LOOP + "; i++) a += c.r / " + LOOP + ".0;\ngl_FragColor = vec4(a, c.g, c.b, c.a);\n}\n";

    static native long ext64(boolean broken);
    static native String owner();
    /// Runs the program through libGLESv1_CM.so on the current context; the result is `native`-prefixed lines (rows, dladdr, errors).
    static native String es1Scene(String program);

    static void say(String text) {
        Log.i(TAG, text);
    }

    /// The DRM nodes this process holds open, each with its kernel driver (sysfs `device/uevent` MODALIAS): Mesa's DRI drivers open one (WSL: kms_swrast on vgem, platform_android.c), a pure software rasteriser none. "none" when there is none.
    static String drmNodes() {
        StringBuilder out = new StringBuilder();
        java.io.File[] fds = new java.io.File("/proc/self/fd").listFiles();
        for (java.io.File fd : fds == null ? new java.io.File[0] : fds) {
            String target;
            try {
                target = fd.getCanonicalPath();
            } catch (java.io.IOException e) {
                continue;
            }
            if (!target.startsWith("/dev/dri/")) continue;
            String node = target.substring("/dev/dri/".length());
            String driver = "unknown";
            try (java.io.BufferedReader r = new java.io.BufferedReader(new java.io.FileReader("/sys/class/drm/" + node + "/device/uevent"))) {
                for (String l; (l = r.readLine()) != null;) {
                    if (l.startsWith("MODALIAS=")) driver = l.substring("MODALIAS=".length());
                }
            } catch (java.io.IOException e) {
                driver = "unreadable";
            }
            out.append(out.length() == 0 ? "" : " ").append(target).append('=').append(driver);
        }
        return out.length() == 0 ? "none" : out.toString();
    }

    static void check(boolean ok, String what) {
        if (!ok) {
            say("FAIL " + what + ": egl error 0x" + Integer.toHexString(EGL14.eglGetError()));
            System.exit(1);
        }
    }

    static final int EGL_CONTEXT_CLIENT_VERSION = 0x3098;

    static void die(EGL10 egl, String what) {
        say("FAIL " + what + ": egl error 0x" + Integer.toHexString(egl.eglGetError()));
        System.exit(1);
    }

    /// The path of the mapped libGLESv1_CM_mesa.so (the Loader picked Mesa's ES1 driver), or "none".
    static String mesaMaps() {
        try (java.io.BufferedReader r = new java.io.BufferedReader(new java.io.FileReader("/proc/self/maps"))) {
            for (String l; (l = r.readLine()) != null;) {
                if (l.contains("libGLESv1_CM_mesa.so")) return l.substring(l.indexOf('/'));
            }
        } catch (java.io.IOException e) {
            return "unreadable " + e;
        }
        return "none";
    }

    /// Mode es1 (EGL10 as GLSurfaceView does it, a 16-bit depth buffer): the program through GL10 on the first pbuffer, then through libGLESv1_CM.so on a second one of the same context.
    static void es1(int w, int h, String[] ops) {
        EGL10 egl = (EGL10) javax.microedition.khronos.egl.EGLContext.getEGL();
        javax.microedition.khronos.egl.EGLDisplay d = egl.eglGetDisplay(EGL10.EGL_DEFAULT_DISPLAY);
        if (!egl.eglInitialize(d, new int[2])) die(egl, "eglInitialize");
        int[] attrs = {EGL10.EGL_RENDERABLE_TYPE, EGL14.EGL_OPENGL_ES_BIT, EGL10.EGL_SURFACE_TYPE, EGL10.EGL_PBUFFER_BIT,
            EGL10.EGL_RED_SIZE, 8, EGL10.EGL_GREEN_SIZE, 8, EGL10.EGL_BLUE_SIZE, 8, EGL10.EGL_ALPHA_SIZE, 8,
            EGL10.EGL_DEPTH_SIZE, 16, EGL10.EGL_NONE};
        javax.microedition.khronos.egl.EGLConfig[] config = new javax.microedition.khronos.egl.EGLConfig[1];
        int[] n = new int[1];
        if (!egl.eglChooseConfig(d, attrs, config, 1, n) || n[0] != 1) die(egl, "eglChooseConfig");
        javax.microedition.khronos.egl.EGLContext c = egl.eglCreateContext(d, config[0], EGL10.EGL_NO_CONTEXT,
            new int[] {EGL_CONTEXT_CLIENT_VERSION, 1, EGL10.EGL_NONE});
        if (c == EGL10.EGL_NO_CONTEXT) die(egl, "eglCreateContext");
        int[] size = {EGL10.EGL_WIDTH, w, EGL10.EGL_HEIGHT, h, EGL10.EGL_NONE};
        javax.microedition.khronos.egl.EGLSurface s1 = egl.eglCreatePbufferSurface(d, config[0], size);
        if (s1 == EGL10.EGL_NO_SURFACE || !egl.eglMakeCurrent(d, s1, s1, c)) die(egl, "eglMakeCurrent");
        say("version " + GLES10.glGetString(GLES10.GL_VERSION));
        say("renderer " + GLES10.glGetString(GLES10.GL_RENDERER));
        say("maps " + mesaMaps());
        try {
            Es1Scene.run((javax.microedition.khronos.opengles.GL10) c.getGL(), ops);
        } catch (RuntimeException e) {
            say("java error " + e);
        }
        javax.microedition.khronos.egl.EGLSurface s2 = egl.eglCreatePbufferSurface(d, config[0], size);
        if (s2 == EGL10.EGL_NO_SURFACE || !egl.eglMakeCurrent(d, s2, s2, c)) die(egl, "eglMakeCurrent (second pbuffer)");
        for (String line : es1Scene(String.join("\n", ops)).split("\n")) say("native " + line);
        egl.eglMakeCurrent(d, EGL10.EGL_NO_SURFACE, EGL10.EGL_NO_SURFACE, EGL10.EGL_NO_CONTEXT);
        egl.eglTerminate(d);
        say("done");
        System.exit(0);
    }

    /// `what <r,g,b,a>x<count> ...`: every distinct RGBA value of the first `count` pixels in `buf` and how often, so the gate needs no image.
    static String summary(String what, ByteBuffer buf, int count) {
        java.util.TreeMap<String, Integer> seen = new java.util.TreeMap<>();
        for (int i = 0; i < count; i++) {
            String px = (buf.get(4 * i) & 0xff) + "," + (buf.get(4 * i + 1) & 0xff) + "," + (buf.get(4 * i + 2) & 0xff) + "," + (buf.get(4 * i + 3) & 0xff);
            Integer was = seen.get(px);
            seen.put(px, was == null ? 1 : was + 1);
        }
        StringBuilder line = new StringBuilder(what);
        for (java.util.Map.Entry<String, Integer> e : seen.entrySet()) line.append(' ').append(e.getKey()).append('x').append(e.getValue());
        return line.toString();
    }

    static int shader(int type, String source) {
        int s = GLES20.glCreateShader(type);
        GLES20.glShaderSource(s, source);
        GLES20.glCompileShader(s);
        int[] ok = new int[1];
        GLES20.glGetShaderiv(s, GLES20.GL_COMPILE_STATUS, ok, 0);
        if (ok[0] == 0) say("scene-error shader: " + GLES20.glGetShaderInfoLog(s));
        return s;
    }

    /// One frame: clear, draw (not in the broken run), finish, so the frame's time covers the rendering.
    static void frame(int verts, boolean broken) {
        GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT);
        if (!broken) GLES20.glDrawArrays(GLES20.GL_TRIANGLES, 0, verts);
        GLES20.glFinish();
    }

    /// The timed scene: GRID x GRID quads over one texel (`texel`), each fragment running a LOOP-iteration loop over it.
    /// One untimed warm-up frame (the first draw compiles the shader variant), then `frames` frames timed in ns; the last frame is read back as `scene` (every pixel must be the texel) and the times as `frame_ns`.
    static void scene(EGLDisplay d, EGLContext c, EGLConfig cfg, int w, int h, int[] clear, int[] texel, int frames, boolean broken) {
        EGLSurface s = EGL14.eglCreatePbufferSurface(d, cfg, new int[] {EGL14.EGL_WIDTH, w, EGL14.EGL_HEIGHT, h, EGL14.EGL_NONE}, 0);
        check(s != EGL14.EGL_NO_SURFACE, "eglCreatePbufferSurface (scene)");
        check(EGL14.eglMakeCurrent(d, s, s, c), "eglMakeCurrent (scene)");
        GLES20.glViewport(0, 0, w, h);
        GLES20.glClearColor(clear[0] / 255f, clear[1] / 255f, clear[2] / 255f, clear[3] / 255f);

        int prog = GLES20.glCreateProgram();
        GLES20.glAttachShader(prog, shader(GLES20.GL_VERTEX_SHADER, VERTEX));
        GLES20.glAttachShader(prog, shader(GLES20.GL_FRAGMENT_SHADER, FRAGMENT));
        GLES20.glLinkProgram(prog);
        int[] linked = new int[1];
        GLES20.glGetProgramiv(prog, GLES20.GL_LINK_STATUS, linked, 0);
        if (linked[0] == 0) say("scene-error link: " + GLES20.glGetProgramInfoLog(prog));
        GLES20.glUseProgram(prog);
        GLES20.glUniform1i(GLES20.glGetUniformLocation(prog, "uTex"), 0);

        int[] tex = new int[1];
        GLES20.glGenTextures(1, tex, 0);
        GLES20.glActiveTexture(GLES20.GL_TEXTURE0);
        GLES20.glBindTexture(GLES20.GL_TEXTURE_2D, tex[0]);
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MIN_FILTER, GLES20.GL_NEAREST);
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_MAG_FILTER, GLES20.GL_NEAREST);
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_WRAP_S, GLES20.GL_CLAMP_TO_EDGE);
        GLES20.glTexParameteri(GLES20.GL_TEXTURE_2D, GLES20.GL_TEXTURE_WRAP_T, GLES20.GL_CLAMP_TO_EDGE);
        ByteBuffer texels = ByteBuffer.allocateDirect(4);
        for (int v : texel) texels.put((byte) v);
        texels.position(0);
        GLES20.glTexImage2D(GLES20.GL_TEXTURE_2D, 0, GLES20.GL_RGBA, 1, 1, 0, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, texels);

        // interleaved x, y, u, v per vertex; each cell is two triangles, u/v = (x, y) mapped to [0, 1]
        int verts = GRID * GRID * 6;
        FloatBuffer fb = ByteBuffer.allocateDirect(verts * 4 * 4).order(ByteOrder.nativeOrder()).asFloatBuffer();
        for (int i = 0; i < GRID; i++) {
            for (int j = 0; j < GRID; j++) {
                float x0 = -1f + 2f * i / GRID, x1 = -1f + 2f * (i + 1) / GRID;
                float y0 = -1f + 2f * j / GRID, y1 = -1f + 2f * (j + 1) / GRID;
                float[][] corner = {{x0, y0}, {x1, y0}, {x0, y1}, {x1, y0}, {x1, y1}, {x0, y1}};
                for (float[] p : corner) fb.put(p[0]).put(p[1]).put((p[0] + 1f) / 2f).put((p[1] + 1f) / 2f);
            }
        }
        fb.position(0);
        int[] vbo = new int[1];
        GLES20.glGenBuffers(1, vbo, 0);
        GLES20.glBindBuffer(GLES20.GL_ARRAY_BUFFER, vbo[0]);
        GLES20.glBufferData(GLES20.GL_ARRAY_BUFFER, verts * 4 * 4, fb, GLES20.GL_STATIC_DRAW);
        int aPos = GLES20.glGetAttribLocation(prog, "aPos");
        int aUv = GLES20.glGetAttribLocation(prog, "aUv");
        GLES20.glEnableVertexAttribArray(aPos);
        GLES20.glEnableVertexAttribArray(aUv);
        GLES20.glVertexAttribPointer(aPos, 2, GLES20.GL_FLOAT, false, 16, 0);
        GLES20.glVertexAttribPointer(aUv, 2, GLES20.GL_FLOAT, false, 16, 8);

        frame(verts, broken);
        long[] ns = new long[frames];
        for (int f = 0; f < frames; f++) {
            long t0 = System.nanoTime();
            frame(verts, broken);
            ns[f] = System.nanoTime() - t0;
        }
        ByteBuffer px = ByteBuffer.allocateDirect(w * h * 4);
        GLES20.glReadPixels(0, 0, w, h, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, px);
        say(summary("scene", px, w * h));
        int err = GLES20.glGetError();
        if (err != GLES20.GL_NO_ERROR) say("scene-error gl 0x" + Integer.toHexString(err));
        StringBuilder t = new StringBuilder("frame_ns ");
        for (int f = 0; f < frames; f++) t.append(f == 0 ? "" : ",").append(ns[f]);
        say(t.toString());
    }

    public static void main(String[] args) {
        System.load(args[0]);
        if (args[1].equals("es1")) es1(Integer.parseInt(args[2]), Integer.parseInt(args[3]), java.util.Arrays.copyOfRange(args, 4, args.length));
        int w = Integer.parseInt(args[2]), h = Integer.parseInt(args[3]);
        int[] rgba = new int[4];
        for (int i = 0; i < 4; i++) rgba[i] = Integer.parseInt(args[4 + i]);
        int sw = Integer.parseInt(args[8]), sh = Integer.parseInt(args[9]), frames = Integer.parseInt(args[10]);
        int[] texel = new int[4];
        for (int i = 0; i < 4; i++) texel[i] = Integer.parseInt(args[11 + i]);
        boolean broken = args.length > 15 && args[15].equals("break");

        EGLDisplay d = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY);
        int[] version = new int[2];
        check(EGL14.eglInitialize(d, version, 0, version, 1), "eglInitialize");
        say("drm " + drmNodes());
        int[] attrs = {EGL14.EGL_RENDERABLE_TYPE, EGLExt.EGL_OPENGL_ES3_BIT_KHR, EGL14.EGL_SURFACE_TYPE, EGL14.EGL_PBUFFER_BIT,
            EGL14.EGL_RED_SIZE, 8, EGL14.EGL_GREEN_SIZE, 8, EGL14.EGL_BLUE_SIZE, 8, EGL14.EGL_ALPHA_SIZE, 8, EGL14.EGL_NONE};
        EGLConfig[] config = new EGLConfig[1];
        int[] n = new int[1];
        check(EGL14.eglChooseConfig(d, attrs, 0, config, 0, 1, n, 0) && n[0] == 1, "eglChooseConfig");
        EGLContext c = EGL14.eglCreateContext(d, config[0], EGL14.EGL_NO_CONTEXT, new int[] {EGL14.EGL_CONTEXT_CLIENT_VERSION, 3, EGL14.EGL_NONE}, 0);
        check(c != EGL14.EGL_NO_CONTEXT, "eglCreateContext");
        EGLSurface s = EGL14.eglCreatePbufferSurface(d, config[0], new int[] {EGL14.EGL_WIDTH, w, EGL14.EGL_HEIGHT, h, EGL14.EGL_NONE}, 0);
        check(s != EGL14.EGL_NO_SURFACE, "eglCreatePbufferSurface");
        check(EGL14.eglMakeCurrent(d, s, s, c), "eglMakeCurrent");
        say("renderer " + GLES20.glGetString(GLES20.GL_RENDERER));
        say("version " + GLES20.glGetString(GLES20.GL_VERSION));

        GLES20.glClearColor(rgba[0] / 255f, rgba[1] / 255f, rgba[2] / 255f, rgba[3] / 255f);
        if (!broken) GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT);
        ByteBuffer buf = ByteBuffer.allocateDirect(w * h * 4);
        GLES20.glReadPixels(0, 0, w, h, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, buf);
        say(summary("pixels", buf, w * h));

        int[] max = new int[1];
        GLES20.glGetIntegerv(GLES20.GL_MAX_TEXTURE_SIZE, max, 0);
        say("ext glGetInteger64vEXT " + ext64(broken) + " glGetIntegerv " + max[0] + " from " + owner());

        scene(d, c, config[0], sw, sh, rgba, texel, frames, broken);
        EGL14.eglMakeCurrent(d, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT);
        EGL14.eglTerminate(d);
        say("done");
        System.exit(0);
    }
}
