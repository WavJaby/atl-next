// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Activity;
import android.content.Intent;
import android.content.res.AssetFileDescriptor;
import android.graphics.Bitmap;
import android.graphics.Paint;
import android.graphics.Typeface;
import android.media.AudioManager;
import android.media.MediaPlayer;
import android.media.SoundPool;
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
 * SoundPool line (app.rs SOUNDPOOL): `new SoundPool` (the legacy 3-arg path HJ takes) loads libsoundpool.so and constructs, or logs what it threw.
 * MediaPlayer lines (app.rs MEDIAPLAYER): Happy Jump's GameShared.loadMusic sequence, one line per step.
 * Service lines (app.rs SERVICE_LINES): connectivity = the offline answers of the `connectivity` service; permission = PackageManager.checkPermission through `permissionmgr`; else `absent` / what it threw.
 * media.player line (app.rs MEDIA_PLAYER_SERVICE): setDataSource(fd) reaches the served service and returns.
 * result lines (app.rs RESULT): after the marker, Main starts ResultChild for request 7; its result comes back in onActivityResult, and the singleTop self-start from it cancels request 9 before its new intent.
 */
public class Main extends Activity {
    /** A root every supported distro bundle carries (Let's Encrypt's). */
    private static final String ROOT = "CN=ISRG Root X1";
    /** The result round trip's request codes (app.rs RESULT): the child's, and the singleTop self-start's, which is cancelled. */
    private static final int REQUEST_CHILD = 7;
    private static final int REQUEST_SELF = 9;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("AtlTest", "cacerts: " + caCerts());
        Log.i("AtlTest", "fonts: default=" + advance(Typeface.DEFAULT) + " monospace=" + advance(Typeface.MONOSPACE));
        Log.i("AtlTest", "soundpool: " + soundPool());
        mediaPlayer();
        Log.i("AtlTest", "connectivity: " + connectivity());
        Log.i("AtlTest", "permission: " + permission());
        mediaPlayerService();
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
        bridge1Legacy();
        Log.i("AtlTest", "onCreate reached");
        startActivityForResult(new Intent(this, ResultChild.class), REQUEST_CHILD);
    }

    /** The order check's marker: a result line must come before Main's next resume (app.rs RESULT). */
    @Override
    protected void onResume() {
        super.onResume();
        Log.i("AtlTest", "main resumed");
    }

    /** The intent of the singleTop self-start, after its cancelled result (app.rs RESULT). */
    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        Log.i("AtlTest", "main new intent");
    }

    /**
     * "result request=<r> code=<c> extra=<e>" for each result (app.rs RESULT); a missing extra is "none". The child's result (request 7) starts Main singleTop for request 9:
     * the services cancel that before the new intent (ActivityStarter.java:2469).
     */
    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        String extra = data == null ? null : data.getStringExtra(ResultChild.EXTRA);
        Log.i("AtlTest", "result request=" + requestCode + " code=" + resultCode + " extra=" + (extra == null ? "none" : extra));
        if (requestCode == REQUEST_CHILD) {
            startActivityForResult(new Intent(this, Main.class), REQUEST_SELF);
        }
    }

    /** "active=<NetworkInfo> metered=<bool>" from ConnectivityManager; "absent" when getSystemService returns null (Cut the Rope's NPE); else what it threw. */
    private String connectivity() {
        try {
            android.net.ConnectivityManager cm =
                    (android.net.ConnectivityManager) getSystemService(CONNECTIVITY_SERVICE);
            if (cm == null) {
                return "absent";
            }
            return "active=" + cm.getActiveNetworkInfo() + " metered=" + cm.isActiveNetworkMetered();
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /** "network_state=<int> camera=<int>": PackageManager.checkPermission for this package (Cut the Rope's getNetworkType path); else what it threw. */
    private String permission() {
        try {
            android.content.pm.PackageManager pm = getPackageManager();
            return "network_state=" + pm.checkPermission("android.permission.ACCESS_NETWORK_STATE", getPackageName())
                    + " camera=" + pm.checkPermission("android.permission.CAMERA", getPackageName());
        } catch (Throwable t) {
            return "threw " + t;
        }
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
    private void bridge1Calls(Class<?> jni) {
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
        bridge1Call(jni, "glesPbuffer", "glesPbuffer");
        bridge1Call(jni, "mesaMapped", "mesaMapped");
        // S3 (plan gs3): the ES1 scene through libGLESv1_CM.so on an EGL10 context made here, as Happy Jump's GLSurfaceView makes one
        bridge1Call(jni, "es1Begin", "es1Begin");
        bridge1Call(jni, "es1Version", "es1Version");
        bridge1Call(jni, "es1Driver", "es1Driver");
        bridge1Call(jni, "es1Pixels", "es1Pixels");
        bridge1Call(jni, "es1End", "es1End");
        bridge1Call(jni, "assetProbe", "assetProbe", getAssets());
        bridge1Call(jni, "bitmapArgb8888", "bitmapProbe", bitmap3x2(Bitmap.Config.ARGB_8888, ARGB_PIXELS));
        bridge1Call(jni, "bitmapRgb565", "bitmapProbe", bitmap3x2(Bitmap.Config.RGB_565, RGB565_PIXELS));
    }

    /** The C3 bitmap rows' pixels, row-major, opaque, exact in both configs (app.rs BRIDGE1_CALLS): red, green, blue / white, black, gray 0x80 (ARGB_8888); yellow for the last one in RGB_565, whose 565 values are exact for primaries and 0 / 255 channels. */
    private static final int[] ARGB_PIXELS = {
        0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF, 0xFF000000, 0xFF808080,
    };
    private static final int[] RGB565_PIXELS = {
        0xFFFF0000, 0xFF00FF00, 0xFF0000FF, 0xFFFFFFFF, 0xFF000000, 0xFFFFFF00,
    };

    /** A 3x2 bitmap of `config` holding `pixels`. */
    private static Bitmap bitmap3x2(Bitmap.Config config, int[] pixels) {
        Bitmap b = Bitmap.createBitmap(3, 2, config);
        b.setPixels(pixels, 0, 3, 0, 0, 3, 2);
        return b;
    }

    /** C12 legacy rows (AtlLegacyTest.java), loaded apart from the bridge-1 fixture. A failed load logs every row as threw, with ART's reason (the library's name). */
    private static void bridge1Legacy() {
        Class<?> legacy;
        try {
            legacy = Class.forName("atl.bridge1.testlib.AtlLegacyTest");
        } catch (Throwable t) {
            for (String row : new String[] {"legacyStdio", "legacyNew", "zRoundTrip"}) {
                Log.i("AtlTest", "bridge1 call " + row + " = threw " + t);
            }
            return;
        }
        bridge1Call(legacy, "legacyStdio", "legacyStdio");
        bridge1Call(legacy, "legacyNew", "legacyNew");
        bridge1Call(legacy, "zRoundTrip", "zRoundTrip");
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

    /**
     * "constructed" when `new SoundPool` returns (libsoundpool.so loaded, its constructor ran); else "threw <throwable>" with its frames
     * (innermost first, " <- " between, one log line) so the line says where it came from. Released at once: the gate plays nothing.
     */
    private static String soundPool() {
        try {
            SoundPool pool = new SoundPool(4, AudioManager.STREAM_MUSIC, 0);
            pool.release();
            return "constructed";
        } catch (Throwable t) {
            StringBuilder s = new StringBuilder("threw " + t);
            StackTraceElement[] frames = t.getStackTrace();
            for (int i = 0; i < frames.length && i < 12; i++) {
                s.append(" <- ").append(frames[i]);
            }
            for (Throwable c = t.getCause(); c != null; c = c.getCause()) {
                s.append(" caused by ").append(c);
            }
            return s.toString();
        }
    }

    /**
     * Happy Jump's GameShared.loadMusic sequence (app.rs MEDIAPLAYER, native_load.rs PLAYER_STEPS): one line per step, "<step> ok" or "<step> <throwable>".
     * start() in Idle: no exception; AOSP reports INVALID_OPERATION to the (absent) listener (android_media_MediaPlayer.cpp _start). Released at once; the gate plays nothing.
     */
    private static void mediaPlayer() {
        MediaPlayer mp;
        try {
            mp = new MediaPlayer();
            mediaPlayerLog("new ok");
        } catch (Throwable t) {
            mediaPlayerLog("new " + thrown(t));
            return;
        }
        try {
            mp.setLooping(true);
            mediaPlayerLog("setLooping ok");
        } catch (Throwable t) {
            mediaPlayerLog("setLooping " + thrown(t));
        }
        try {
            mp.setVolume(0.5f, 0.5f);
            mediaPlayerLog("setVolume ok");
        } catch (Throwable t) {
            mediaPlayerLog("setVolume " + thrown(t));
        }
        try {
            mp.start();
            mediaPlayerLog("start ok");
        } catch (Throwable t) {
            mediaPlayerLog("start " + thrown(t));
        }
        try {
            mp.release();
            mediaPlayerLog("release ok");
        } catch (Throwable t) {
            mediaPlayerLog("release " + thrown(t));
        }
    }

    private static void mediaPlayerLog(String text) {
        Log.i("AtlTest", "mediaplayer: " + text);
    }

    /** A throwable's class and message on one line. */
    private static String thrown(Throwable t) {
        return t.getClass().getName() + ": " + String.valueOf(t.getMessage()).replace('\n', ' ');
    }

    /** How long the media.player probe waits for the view's answer before its line says so (app.rs MEDIA_PLAYER_SERVICE). */
    private static final long PROBE_MS = 20000L;

    /**
     * media.player through the process's view (audio P1a): setDataSource(fd) on a stored asset of bytes no demuxer knows. The native call looks the service up
     * (IMediaDeathNotifier::getMediaPlayerService, waitForService); the service answers and setDataSource returns, so the line reads "returned" (MediaPlayer.java drops a native error). A lookup nobody
     * answers blocks the call, so the probe runs on its own thread and the line then says "no answer".
     */
    private void mediaPlayerService() {
        Thread probe = new Thread(new Runnable() {
            @Override
            public void run() {
                String r;
                try {
                    AssetFileDescriptor afd = getAssets().openFd("stored.bin");
                    MediaPlayer mp = new MediaPlayer();
                    mp.setDataSource(afd.getFileDescriptor(), afd.getStartOffset(), afd.getLength());
                    r = "returned";
                } catch (Throwable t) {
                    r = thrown(t);
                }
                Log.i("AtlTest", "media.player: " + r);
            }
        });
        probe.start();
        try {
            probe.join(PROBE_MS);
        } catch (InterruptedException e) {
            Log.i("AtlTest", "media.player: interrupted " + e);
        }
        if (probe.isAlive()) {
            Log.i("AtlTest", "media.player: no answer within " + PROBE_MS + " ms");
        }
    }
}
