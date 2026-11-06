// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Activity;
import android.graphics.Paint;
import android.graphics.Typeface;
import android.os.Bundle;
import android.util.Log;

import java.security.KeyStore;
import java.security.cert.X509Certificate;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509TrustManager;

/**
 * The M2 gate: `xtask test app` (xtask/src/app.rs MARKER) waits for the onCreate line; two lines before it are checked too.
 * CA line (app.rs CACERTS): the default TrustManagerFactory trusts a distro root.
 * Fonts line (app.rs FONTS): the default and monospace typefaces' advance of one text, which the gate compares with the pinned fonts' hmtx tables.
 * Bridge-1 line (app.rs BRIDGE1): the fixture is loaded (AtlJniTest's static initialiser) through the NativeBridge core into its guest; a failed load logs ART's UnsatisfiedLinkError, which carries the core's reason.
 */
public class Main extends Activity {
    /** A root every supported distro bundle carries (Let's Encrypt's). */
    private static final String ROOT = "CN=ISRG Root X1";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("AtlTest", "cacerts: " + caCerts());
        Log.i("AtlTest", "fonts: default=" + advance(Typeface.DEFAULT) + " monospace=" + advance(Typeface.MONOSPACE));
        Class<?> jni;
        try {
            jni = Class.forName("atl.bridge1.testlib.AtlJniTest");
            Log.i("AtlTest", "bridge1 loaded");
        } catch (UnsatisfiedLinkError | ClassNotFoundException e) {
            Log.i("AtlTest", "bridge1 not loaded: " + e);
            jni = null;
        }
        if (jni != null) {
            bridge1Calls(jni);
        }
        Log.i("AtlTest", "onCreate reached");
    }

    /** The font gate's text (app.rs FONT_TEXT). */
    private static final String FONT_TEXT = "Handgloves";
    /** Text size = the pinned fonts' unitsPerEm, so every advance is a whole number of pixels (test_fonts.rs advance). */
    private static final float FONT_SIZE = 2048f;

    /** FONT_TEXT's advance in `tf` at FONT_SIZE, kerning and ligatures off: the sum of the font's hmtx advances. */
    private static float advance(Typeface tf) {
        Paint p = new Paint();
        p.setTypeface(tf);
        p.setTextSize(FONT_SIZE);
        p.setFontFeatureSettings("'kern' 0, 'liga' 0, 'clig' 0, 'calt' 0");
        return p.measureText(FONT_TEXT);
    }

    /** One native call's result line (app.rs BRIDGE1_CALLS): "bridge1 call <label> = <value>", or what it threw. Reflection: the fixture class is compiled apart from Main. */
    private static void bridge1Call(Class<?> jni, String label, String method, Object... args) {
        String v;
        try {
            Class<?>[] types = new Class<?>[args.length];
            for (int i = 0; i < args.length; i++) {
                types[i] = args[i] instanceof Integer ? int.class : args[i].getClass();
            }
            v = String.valueOf(jni.getMethod(method, types).invoke(null, args));
        } catch (java.lang.reflect.InvocationTargetException e) {
            v = "threw " + e.getCause();
        } catch (Throwable t) {
            v = "threw " + t;
        }
        Log.i("AtlTest", "bridge1 call " + label + " = " + v);
    }

    /** Order matters: tlsCounter is per thread, so threadSum's worker and a fresh Java thread must leave the main thread's count alone. */
    private static void bridge1Calls(Class<?> jni) {
        bridge1Call(jni, "tlsCounter#1", "tlsCounter");
        bridge1Call(jni, "tlsCounter#2", "tlsCounter");
        bridge1Call(jni, "threadSum", "threadSum", 100);
        bridge1Call(jni, "tlsCounter#3", "tlsCounter");
        // no lambda: the APK build has no LambdaMetafactory desugaring
        Thread t = new Thread(new Runnable() {
            @Override
            public void run() {
                bridge1Call(jni, "tlsCounterFreshThread", "tlsCounter");
            }
        });
        t.start();
        try {
            t.join();
        } catch (InterruptedException e) {
            Log.i("AtlTest", "bridge1 call tlsCounterFreshThread = threw " + e);
        }
        bridge1Call(jni, "errnoRoundTrip", "errnoRoundTrip");
        bridge1Call(jni, "cppThrowCatch", "cppThrowCatch");
        bridge1Call(jni, "dlopenSibling", "dlopenSibling");
        bridge1Call(jni, "greet", "greet", "bridge1");
        bridge1Call(jni, "callJavaAdd", "callJavaAdd", 2, 3);
        bridge1Call(jni, "registeredAnswer", "registeredAnswer");
        bridge1Call(jni, "sigsegvRecover", "sigsegvRecover");
        bridge1Call(jni, "nullFieldRead", "nullFieldRead");
        bridge1Call(jni, "jitUpCall", "jitUpCall");
        bridge1Call(jni, "guestLog", "guestLog");
        bridge1Call(jni, "propCallbacks", "propCallbacks");
    }

    /** The default TrustManagerFactory's issuers (Conscrypt: AndroidCAStore -> TrustedCertificateStore): "trusted <ROOT> of <n>", or why not. */
    private static String caCerts() {
        try {
            TrustManagerFactory f = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
            f.init((KeyStore) null);
            for (TrustManager tm : f.getTrustManagers()) {
                if (tm instanceof X509TrustManager) {
                    X509Certificate[] issuers = ((X509TrustManager) tm).getAcceptedIssuers();
                    for (X509Certificate c : issuers) {
                        if (c.getSubjectX500Principal().getName().contains(ROOT)) {
                            return "trusted " + ROOT + " of " + issuers.length;
                        }
                    }
                    return "FAIL no " + ROOT + " among " + issuers.length;
                }
            }
            return "FAIL no X509TrustManager";
        } catch (Exception e) {
            return "FAIL " + e;
        }
    }
}
