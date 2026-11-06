// SPDX-License-Identifier: MIT
// Test driver of `xtask test native-load` (xtask/src/native_load.rs), run on host ART; never shipped.
// Every result is one stdout line `nlt <key> <value>`, flushed at once: a register_* call may abort the process, and the lines before it are the measurement.
// Modes: load <lib> | startreg <lib> <shim> | walk <lib> <shim> <startReg vaddr> <gRegJNI slot vaddr>... | call <lib> <shim> <startReg vaddr> <function vaddr>...; vaddrs in hex, from the library's .symtab.
// walk calls through each slot as relocated at run time and stops at the first negative return, as register_jni_procs does; call registers the given functions, then runs the native calls.
// chain <shim> <exit status> <rounds>: the shim installs its own SIGSEGV handler with sigaction, then `rounds` null derefs in read (JIT-compiled: implicit null checks fault), then a native fault the shim's handler ends the process on.
// crash <shim>: the shim raises SIGABRT in native code; the process must die of it (prints the number first).
// media <lib> <shim> <mime>...: NDK AMediaCodec_createDecoderByType per mime (libmediandk, NEEDED by lib), prints 1 = codec created, 0 = null, -100 = libmediandk not loaded; a Codec2 abort ends the process, the lines before it are the measurement.
// decode <lib> <shim> <clip> <pcm out>: one clip through the NDK extractor and decoder (stage 9): `nlt decode` is the step that failed (`error=`), else frames, bytes, channels, rate, and the store the process sees (`store=`, `declared=`, `instances=`).
// overflow <rounds>: unbounded recursion in recurse, `rounds` times on the main thread (warms the JIT: compiled code checks the stack implicitly, a SIGSEGV on the guard page), then `rounds` times on a new thread; each catches StackOverflowError, prints its depth, and the process exits normally.

import java.io.BufferedReader;
import java.io.FileReader;
import java.util.LinkedHashSet;
import java.util.Set;

public class NativeLoad {
    static final String TAG = "nlt";

    // libnlt_shim (shim/src/lib.rs)
    static native int startReg();
    static native int call(long startRegVaddr, long vaddr);
    static native int callSlot(long startRegVaddr, long slotVaddr);
    static native int installSegv(int exitStatus);
    static native void nativeFault();
    static native int abortSignal();
    static native void raiseAbort();
    static native int createDecoder(String mime);
    static native String decodeFile(String input, String output);

    static class Box {
        int v;
    }

    static void out(String key, Object value) {
        System.out.println("nlt " + key + " " + value);
        System.out.flush();
    }

    static boolean load(String path) {
        try {
            System.load(path);
            out("loaded", path);
            return true;
        } catch (UnsatisfiedLinkError e) {
            out("load-error", path + "\t" + String.valueOf(e.getMessage()).replace('\n', ' '));
            return false;
        }
    }

    public static void main(String[] a) throws Exception {
        String mode = a[0];
        if (mode.equals("overflow")) {
            overflow(Integer.parseInt(a[1]));
            return;
        }
        if (!load(a[1])) {
            System.exit(1);
        }
        if (mode.equals("load")) {
            maps();
            return;
        }
        if (mode.equals("chain")) {
            chain(Integer.parseInt(a[2]), Integer.parseInt(a[3]));
            return;
        }
        if (mode.equals("crash")) {
            out("signal", abortSignal());
            raiseAbort();
            out("crash", "survived");
            return;
        }
        if (!load(a[2])) {
            System.exit(1);
        }
        if (mode.equals("media")) {
            for (int i = 3; i < a.length; i++) {
                out("codec", a[i] + " " + createDecoder(a[i]));
            }
            return;
        }
        if (mode.equals("decode")) {
            out("decode", decodeFile(a[3], a[4]));
            return;
        }
        if (mode.equals("startreg")) {
            out("startreg", startReg());
            return;
        }
        long startRegVaddr = Long.parseLong(a[3], 16);
        boolean walk = mode.equals("walk");
        for (int i = 4; i < a.length; i++) {
            out("reg", i - 4);
            long vaddr = Long.parseLong(a[i], 16);
            int rc = walk ? callSlot(startRegVaddr, vaddr) : call(startRegVaddr, vaddr);
            out("ret", (i - 4) + " " + rc);
            if (rc < 0) {
                return;
            }
        }
        if (mode.equals("call")) {
            calls();
        }
    }

    static int read(Box b) {
        return b.v;
    }

    static void chain(int exitStatus, int rounds) {
        out("sigaction", installSegv(exitStatus));
        int npe = 0;
        for (int i = 0; i < rounds; i++) {
            try {
                read(null);
            } catch (NullPointerException e) {
                npe++;
            }
        }
        out("npe", npe);
        nativeFault();
        out("fault", "survived");
    }

    static int depth;

    static void recurse() {
        depth++;
        recurse();
    }

    // `rounds` overflows on the calling thread, each `nlt <key> <depth>`
    static void overflows(String key, int rounds) {
        for (int i = 0; i < rounds; i++) {
            depth = 0;
            try {
                recurse();
            } catch (StackOverflowError e) {
                out(key, depth);
            }
        }
    }

    static void overflow(final int rounds) throws InterruptedException {
        overflows("soe-main", rounds);
        Thread t = new Thread(new Runnable() {
            public void run() {
                overflows("soe-thread", rounds);
            }
        }, "overflow");
        t.start();
        t.join();
        out("overflow", "done");
    }

    // mapped files after the load: one line per distinct path
    static void maps() throws Exception {
        Set<String> paths = new LinkedHashSet<>();
        try (BufferedReader r = new BufferedReader(new FileReader("/proc/self/maps"))) {
            for (String line; (line = r.readLine()) != null; ) {
                int slash = line.indexOf('/');
                if (slash >= 0) {
                    paths.add(line.substring(slash));
                }
            }
        }
        for (String p : paths) {
            out("map", p);
        }
    }

    static void calls() {
        long t0 = System.nanoTime();
        long up = android.os.SystemClock.uptimeMillis();
        long t1 = System.nanoTime();
        out("uptime", t0 + " " + up + " " + t1);
        out("loggable-verbose", android.util.Log.isLoggable(TAG, android.util.Log.VERBOSE));
        out("loggable-info", android.util.Log.isLoggable(TAG, android.util.Log.INFO));
        // bufID 0 = LOG_ID_MAIN (liblog log_id_t)
        out("println", android.util.Log.println_native(0, android.util.Log.INFO, TAG, "native-load println_native"));
        // below the default INFO: liblog refuses it before any logger (xtask checks it never reaches stderr)
        out("println-verbose", android.util.Log.println_native(0, android.util.Log.VERBOSE, TAG, "native-load verbose"));
    }
}
