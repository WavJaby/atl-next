// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;

import java.security.KeyStore;
import java.security.cert.X509Certificate;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509TrustManager;

/** The M2 gate: `xtask test app` (xtask/src/app.rs MARKER) waits for the onCreate line; the CA line before it is checked too (app.rs CACERTS). */
public class Main extends Activity {
    /** A root every supported distro bundle carries (Let's Encrypt's). */
    private static final String ROOT = "CN=ISRG Root X1";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("AtlTest", "cacerts: " + caCerts());
        Log.i("AtlTest", "onCreate reached");
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
