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
 * Bridge-1 line (app.rs BRIDGE1): the fixture is loaded (AtlJniTest's static initialiser); until bridge 1 loads bionic code, the NativeBridge core refuses and ART throws UnsatisfiedLinkError with its text.
 */
public class Main extends Activity {
    /** A root every supported distro bundle carries (Let's Encrypt's). */
    private static final String ROOT = "CN=ISRG Root X1";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("AtlTest", "cacerts: " + caCerts());
        Log.i("AtlTest", "fonts: default=" + advance(Typeface.DEFAULT) + " monospace=" + advance(Typeface.MONOSPACE));
        try {
            Class.forName("atl.bridge1.testlib.AtlJniTest");
            Log.i("AtlTest", "bridge1 loaded");
        } catch (UnsatisfiedLinkError | ClassNotFoundException e) {
            Log.i("AtlTest", "bridge1 not loaded: " + e);
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
