// SPDX-License-Identifier: MIT
package atl.bridge1.testlib;

import android.content.res.AssetManager;
import android.graphics.Bitmap;
import javax.microedition.khronos.egl.EGL10;
import javax.microedition.khronos.egl.EGLConfig;
import javax.microedition.khronos.egl.EGLContext;
import javax.microedition.khronos.egl.EGLDisplay;
import javax.microedition.khronos.egl.EGLSurface;

/**
 * Java side of the bridge-1 fixture. Native code: libatl-jni-test.so (xtask/testdata/jni-test, built by xtask/src/jni_fixture.rs), packaged into the M2 test APK's lib/x86_64/; the M2 app does not load it until bridge 1 can.
 * A failed load must surface as UnsatisfiedLinkError naming the library; a crash is a failed gate.
 * Expected: errnoRoundTrip 2 (ENOENT), tlsCounter 1 then 2, 3 after threadSum, 1 on a fresh thread, threadSum(100) 5050, greet("bridge1") "hello, bridge1", callJavaAdd(2, 3) 5, cppThrowCatch 1, dlopenSibling 42, registeredAnswer 7, sigsegvRecover 1, then nullFieldRead "caught".
 */
public final class AtlJniTest {
    static {
        System.loadLibrary("atl-jni-test");
    }

    private AtlJniTest() {}

    /** JNI down-call target; native callJavaAdd() reaches it via GetStaticMethodID. */
    public static int javaAdd(int a, int b) {
        return a + b;
    }

    /** open() of a missing path; returns errno. */
    public static native int errnoRoundTrip();

    /** __thread counter; returns this thread's post-increment value. */
    public static native int tlsCounter();

    /** pthread worker sums 1..n in its own thread; returns the sum. */
    public static native int threadSum(int n);

    /** JNI up-call: NewStringUTF of "hello, " + name. name must be non-null. */
    public static native String greet(String name);

    /** JNI down-call: javaAdd(a, b) via CallStaticIntMethod. */
    public static native int callJavaAdd(int a, int b);

    /** C++ throw/catch of std::runtime_error through libc++. */
    public static native int cppThrowCatch();

    /** dlopen("libatl-jni-sibling.so") + dlsym + call. */
    public static native int dlopenSibling();

    /** __android_log_print(WARN, "AtlJniGuest", "guest liblog 42") through the guest's liblog; 1 = liblog's return. */
    public static native int guestLog();

    /** Guest callbacks of __system_property_read_callback / __system_property_foreach run on the caller's guest thread: 11 (atl_jni_test.c). */
    public static native int propCallbacks();

    /** GLES2 through the GL proxies: pixels equal to the scene (4096 = all of a 64x64 pbuffer), -1 on an EGL or GL error (atl_jni_test.c). */
    public static native int glesPbuffer();

    /** After glesPbuffer: 1 when /proc/self/maps names the host driver ("mesa"), 0 when not, -1 when unreadable (atl_jni_test.c). */
    public static native int mesaMapped();

    /** The ES1 context the es1 natives draw on: an EGL10 pbuffer and context made by es1Begin on the calling thread, as GLSurfaceView makes them for Happy Jump (plan gs3 S3). */
    private static EGL10 egl;
    private static EGLDisplay dpy;
    private static EGLSurface surf;
    private static EGLContext ctx;

    /** The scene's surface (es1-scene.toml [surface]); es1_scene.inc carries the same size. */
    private static final int ES1_SIZE = 64;

    /** Makes an ES1 context current on this thread; false when EGL refuses. */
    public static boolean es1Begin() {
        egl = (EGL10) EGLContext.getEGL();
        dpy = egl.eglGetDisplay(EGL10.EGL_DEFAULT_DISPLAY);
        if (dpy == EGL10.EGL_NO_DISPLAY || !egl.eglInitialize(dpy, new int[2])) {
            return false;
        }
        // EGL_RENDERABLE_TYPE 0x0001 is EGL_OPENGL_ES_BIT (ES1); no EGL_CONTEXT_CLIENT_VERSION attribute, so the context is ES1
        int[] cfgAttr = {
            EGL10.EGL_SURFACE_TYPE, EGL10.EGL_PBUFFER_BIT,
            EGL10.EGL_RENDERABLE_TYPE, 0x0001,
            EGL10.EGL_RED_SIZE, 8, EGL10.EGL_GREEN_SIZE, 8, EGL10.EGL_BLUE_SIZE, 8, EGL10.EGL_ALPHA_SIZE, 8,
            EGL10.EGL_NONE,
        };
        EGLConfig cfg = exact8888(egl, dpy, cfgAttr);
        if (cfg == null) {
            return false;
        }
        surf = egl.eglCreatePbufferSurface(dpy, cfg, new int[] {
            EGL10.EGL_WIDTH, ES1_SIZE, EGL10.EGL_HEIGHT, ES1_SIZE, EGL10.EGL_NONE,
        });
        ctx = egl.eglCreateContext(dpy, cfg, EGL10.EGL_NO_CONTEXT, new int[] { EGL10.EGL_NONE });
        return surf != EGL10.EGL_NO_SURFACE && ctx != EGL10.EGL_NO_CONTEXT
                && egl.eglMakeCurrent(dpy, surf, surf, ctx);
    }

    /** GLSurfaceView's ComponentSizeChooser: the first listed config with RGBA exactly 8 each (EGL's *_SIZE attributes are minimums); null when none. */
    private static EGLConfig exact8888(EGL10 egl, EGLDisplay dpy, int[] attr) {
        EGLConfig[] all = new EGLConfig[64];
        int[] n = new int[1];
        if (!egl.eglChooseConfig(dpy, attr, all, all.length, n)) {
            return null;
        }
        int[] v = new int[1];
        for (int i = 0; i < Math.min(n[0], all.length); i++) {
            boolean exact = true;
            for (int size : new int[] { EGL10.EGL_RED_SIZE, EGL10.EGL_GREEN_SIZE, EGL10.EGL_BLUE_SIZE, EGL10.EGL_ALPHA_SIZE }) {
                exact &= egl.eglGetConfigAttrib(dpy, all[i], size, v) && v[0] == 8;
            }
            if (exact) {
                return all[i];
            }
        }
        return null;
    }

    /** Releases es1Begin's context and pbuffer; true when EGL released both. */
    public static boolean es1End() {
        if (egl == null) {
            return false;
        }
        boolean ok = egl.eglMakeCurrent(dpy, EGL10.EGL_NO_SURFACE, EGL10.EGL_NO_SURFACE, EGL10.EGL_NO_CONTEXT);
        ok &= egl.eglDestroyContext(dpy, ctx);
        ok &= egl.eglDestroySurface(dpy, surf);
        return ok;
    }

    /** GL_VERSION of the current context starts "OpenGL ES-CM 1." or "OpenGL ES 1.1" (ANGLE): 1 when so, 0 when not, 0 with no current context (atl_jni_es1.c). */
    public static native int es1Version();

    /** After es1Begin: 1 when /proc/self/maps names ANGLE's libGLESv2_angle.so, 0 when not, -1 when unreadable (atl_jni_es1.c). */
    public static native int es1Driver();

    /** The scene (es1-scene.toml, through es1_scene.inc) on the current context: 1 when no GL error came back. Its rows are logged as `row <y> <hex>`, and app.rs judges them (atl_jni_es1.c). */
    public static native int es1Pixels();

    /** C3 asset probe over libandroid's AAsset API: one line of streamed SHA-256s, a seek's tail, getBuffer and a missing name (atl_jni_test.c assetProbe, app.rs BRIDGE1_CALLS). */
    public static native String assetProbe(AssetManager assets);

    /** C3 bitmap probe over libjnigraphics' AndroidBitmap: info and the locked pixel bytes, one line (atl_jni_test.c bitmapProbe). */
    public static native String bitmapProbe(Bitmap bitmap);

    /** Bound by RegisterNatives in JNI_OnLoad. */
    public static native int registeredAnswer();

    /** Guest SIGSEGV handler + siglongjmp; leaves the handler installed. */
    public static native int sigsegvRecover();

    private static final class Box {
        int v;
    }

    /** Not final: javac cannot fold the null away. */
    private static Box nullBox;

    /** Calls that make readV hot enough for the JIT, whose code checks null implicitly; the interpreter checks explicitly (no signal). */
    private static final int WARM_CALLS = 2_000_000;

    private static int readV(Box b) {
        return b.v;
    }

    /** Up-call target once JIT-compiled: its allocation goes through an entrypoint compiled code finds via %gs (ART's Thread* on x86_64). */
    public static int jitWork(int k) {
        return new int[k + 1].length + k;
    }

    /** Calls jitWork(i &amp; 7) `calls` times through JNI CallStaticIntMethod from guest code; returns the sum. */
    private static native long jitUpCallNative(int calls);

    /** Up-calls jitUpCall makes: UP_CALLS / 8 rounds of sum(2k + 1, k = 0..7) = 64 -> 8_000_000. */
    private static final int UP_CALLS = 1_000_000;

    /** JNI up-calls into JIT-compiled Java: jitWork warmed like readV, then reached from guest code. */
    public static long jitUpCall() {
        int s = 0;
        for (int i = 0; i < WARM_CALLS; i++) {
            s += jitWork(i & 7);
        }
        return s == 0 ? -1 : jitUpCallNative(UP_CALLS);
    }

    /** A field read through null in JIT code: ART's implicit null check (a SIGSEGV its fault handler turns into an NPE) must still win over the guest handler sigsegvRecover left. */
    public static String nullFieldRead() {
        Box live = new Box();
        int s = 0;
        for (int i = 0; i < WARM_CALLS; i++) {
            s += readV(live);
        }
        try {
            return "read " + (s + readV(nullBox));
        } catch (NullPointerException e) {
            return "caught";
        }
    }
}
