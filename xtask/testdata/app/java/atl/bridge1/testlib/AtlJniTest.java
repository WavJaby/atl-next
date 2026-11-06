// SPDX-License-Identifier: MIT
package atl.bridge1.testlib;

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
