// SPDX-License-Identifier: MIT
package com.example.atlgl;

import android.opengl.EGL14;
import android.opengl.EGLConfig;
import android.opengl.EGLContext;
import android.opengl.EGLDisplay;
import android.opengl.EGLExt;
import android.opengl.EGLSurface;
import android.opengl.GLES20;
import android.util.Log;

import java.nio.ByteBuffer;

/// `xtask test gl`'s driver: a GLES3 pbuffer through the framework's EGL14 / GLES20 (AOSP libEGL + libGLESv2 -> the driver), cleared to the colour given as four bytes and read back;
/// then glGetInteger64vEXT, a name outside AOSP's entries.in, through eglGetProcAddress (native lib). Prints one `gl-probe:` line per result; the gate judges them.
/// Args: path of the native lib, w, h, r, g, b, a [, "break"]. Break: no glClear, and the extension call goes to a no-op instead (what a dead forwarder does).
public class Probe {
    private static final String TAG = "gl-probe";

    static native long ext64(boolean broken);
    static native String owner();

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

    public static void main(String[] args) {
        System.load(args[0]);
        int w = Integer.parseInt(args[1]), h = Integer.parseInt(args[2]);
        int[] rgba = new int[4];
        for (int i = 0; i < 4; i++) rgba[i] = Integer.parseInt(args[3 + i]);
        boolean broken = args.length > 7 && args[7].equals("break");

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
        say("renderer " + GLES20.glGetString(GLES20.GL_RENDERER) + " / " + GLES20.glGetString(GLES20.GL_VERSION));

        GLES20.glClearColor(rgba[0] / 255f, rgba[1] / 255f, rgba[2] / 255f, rgba[3] / 255f);
        if (!broken) GLES20.glClear(GLES20.GL_COLOR_BUFFER_BIT);
        ByteBuffer buf = ByteBuffer.allocateDirect(w * h * 4);
        GLES20.glReadPixels(0, 0, w, h, GLES20.GL_RGBA, GLES20.GL_UNSIGNED_BYTE, buf);
        // pixels: every distinct RGBA value read and how often, so the gate needs no image
        java.util.TreeMap<String, Integer> seen = new java.util.TreeMap<>();
        for (int i = 0; i < w * h; i++) {
            String px = (buf.get(4 * i) & 0xff) + "," + (buf.get(4 * i + 1) & 0xff) + "," + (buf.get(4 * i + 2) & 0xff) + "," + (buf.get(4 * i + 3) & 0xff);
            Integer was = seen.get(px);
            seen.put(px, was == null ? 1 : was + 1);
        }
        StringBuilder line = new StringBuilder("pixels");
        for (java.util.Map.Entry<String, Integer> e : seen.entrySet()) line.append(' ').append(e.getKey()).append('x').append(e.getValue());
        say(line.toString());

        int[] max = new int[1];
        GLES20.glGetIntegerv(GLES20.GL_MAX_TEXTURE_SIZE, max, 0);
        say("ext glGetInteger64vEXT " + ext64(broken) + " glGetIntegerv " + max[0] + " from " + owner());
        EGL14.eglMakeCurrent(d, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT);
        EGL14.eglTerminate(d);
        say("done");
        System.exit(0);
    }
}
