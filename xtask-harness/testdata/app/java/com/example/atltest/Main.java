// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Activity;
import android.content.Intent;
import android.content.res.AssetFileDescriptor;
import android.graphics.Bitmap;
import android.graphics.Paint;
import android.graphics.Typeface;
import android.media.AudioManager;
import android.media.MediaExtractor;
import android.media.MediaMetadataRetriever;
import android.media.MediaPlayer;
import android.media.SoundPool;
import android.os.Bundle;
import android.util.Log;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.security.KeyStore;
import java.security.cert.X509Certificate;
import java.util.concurrent.atomic.AtomicReference;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509TrustManager;

/**
 * The M2 gate: `xtask test app` (xtask-harness/src/app.rs MARKER) waits for the onCreate line; two lines before it are checked too.
 * CA line (app.rs CACERTS): the default TrustManagerFactory trusts a distro root.
 * Fonts line (app.rs FONTS): the default and monospace typefaces' advance of one text, which the gate compares with the pinned fonts' hmtx tables.
 * Bridge-1 line (app.rs BRIDGE1): the fixture is loaded (AtlJniTest's static initialiser) through the NativeBridge core into its guest; a failed load logs ART's UnsatisfiedLinkError, which carries the core's reason.
 * SoundPool line (app.rs SOUNDPOOL): `new SoundPool` (the legacy 3-arg path HJ takes) loads libsoundpool.so and constructs, or logs what it threw.
 * MediaPlayer lines (app.rs MEDIAPLAYER): Happy Jump's GameShared.loadMusic sequence, one line per step.
 * Service lines (app.rs SERVICE_LINES): connectivity = the offline answers of the `connectivity` service; permission = PackageManager.checkPermission through `permissionmgr`; else `absent` / what it threw.
 * media.player line (app.rs MEDIA_PLAYER_SERVICE): setDataSource(fd) reaches the served service and returns.
 * WAV lines (app.rs MEDIA_WAV): a valid WAV is prepared, started, sought and reset per round, by fd and by path; the served extractor must sniff it.
 * Reset lines (app.rs MEDIA_RESET): reset with a seek pending, each within a bound; a reset that never returns is named.
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
        mediaExtractorWav();
        mediaRetrieverWav();
        mediaPlayerWav();
        mediaPlayerReset();
        readSkipped();
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

    /** The asset naming the bridge-1 calls this run does not make, one label a line (app.rs SKIP_ASSET): the gate writes it for an engine that does not carry them yet. */
    private static final String SKIP_ASSET = "bridge1-skip.txt";
    /** The labels of SKIP_ASSET; empty when the APK has none. */
    private static final java.util.Set<String> SKIPPED = new java.util.HashSet<>();

    /** Reads SKIP_ASSET into SKIPPED; a missing asset skips nothing. */
    private void readSkipped() {
        try (java.io.BufferedReader r = new java.io.BufferedReader(new java.io.InputStreamReader(getAssets().open(SKIP_ASSET)))) {
            for (String l = r.readLine(); l != null; l = r.readLine()) {
                if (!l.isEmpty()) {
                    SKIPPED.add(l);
                }
            }
        } catch (java.io.IOException e) {
            // no asset: every call runs
        }
    }

    /** One native call's result line (app.rs BRIDGE1_CALLS): "bridge1 call <label> = <value>", "skipped" for a SKIPPED label, or what it threw. Reflection: the fixture class is compiled apart from Main. */
    private static void bridge1Call(Class<?> jni, String label, String method, Object... args) {
        if (SKIPPED.contains(label)) {
            Log.i("AtlTest", "bridge1 call " + label + " = skipped");
            return;
        }
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
        bridge1Call(jni, "firstConfig", "firstConfig");
        bridge1Call(jni, "glesPbuffer", "glesPbuffer");
        bridge1Call(jni, "driverMapped", "driverMapped");
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
        String[] rows = {"legacyStdio", "legacyNew", "zRoundTrip"};
        // every row skipped: the library is not loaded either
        if (SKIPPED.containsAll(java.util.Arrays.asList(rows))) {
            for (String row : rows) {
                Log.i("AtlTest", "bridge1 call " + row + " = skipped");
            }
            return;
        }
        Class<?> legacy;
        try {
            legacy = Class.forName("atl.bridge1.testlib.AtlLegacyTest");
        } catch (Throwable t) {
            for (String row : rows) {
                Log.i("AtlTest", "bridge1 call " + row + " = threw " + t);
            }
            return;
        }
        for (String row : rows) {
            bridge1Call(legacy, row, row);
        }
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
                    // VideoView's next step (prepareAsync in the game): the prepare creates the extractor; the WAV rounds (app.rs MEDIA_WAV) are the check that it works
                    try {
                        mp.prepare();
                    } catch (Exception e) {
                        // the bytes are no media, so the prepare fails; the line above already answered the probe
                    }
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

    /**
     * WAV rounds of the media.player WAV probe (app.rs MEDIA_WAV): even rounds pass the fd, odd rounds the path. MediaPlayer.java:1237-1238 opens a path and
     * passes its fd, so both reach GenericSource through the same fd route.
     */
    private static final int WAV_ROUNDS = 4;
    /** Length of the probe's WAV, and the position each round seeks to (app.rs MEDIA_WAV). */
    private static final int WAV_SECONDS = 2;
    private static final int WAV_SEEK_MS = 1000;
    /** Rounds of the reset probe; the bound each round of the WAV and reset probes gets (app.rs MEDIA_ROUND_BOUND_MS). */
    private static final int RESET_ROUNDS = 4;
    private static final long ROUND_BOUND_MS = 10000L;

    /**
     * Diagnosis, not gated: MediaMetadataRetriever reads the WAV's mime from the fd through a PlayerServiceFileSource and the same
     * MediaExtractorFactory::Create that GenericSource runs (StagefrightMetadataRetriever.cpp:96,105). A mime here means the sniff works on that source, so
     * a failing MediaPlayer line sits in GenericSource's own flow.
     */
    private void mediaRetrieverWav() {
        File wav;
        try {
            wav = writeWav("retriever.wav");
        } catch (IOException e) {
            Log.i("AtlTest", "mediaretriever: no file " + thrown(e));
            return;
        }
        MediaMetadataRetriever r = new MediaMetadataRetriever();
        FileInputStream in = null;
        try {
            in = new FileInputStream(wav);
            r.setDataSource(in.getFD());
            Log.i("AtlTest", "mediaretriever: fd mime " + r.extractMetadata(MediaMetadataRetriever.METADATA_KEY_MIMETYPE));
        } catch (Throwable t) {
            Log.i("AtlTest", "mediaretriever: fd " + thrown(t));
        } finally {
            try {
                r.release();
            } catch (IOException e) {
                Log.i("AtlTest", "mediaretriever: release " + thrown(e));
            }
            if (in != null) {
                try {
                    in.close();
                } catch (IOException e) {
                    Log.i("AtlTest", "mediaretriever: close " + thrown(e));
                }
            }
        }
    }

    /**
     * Diagnosis, not gated: android.media.MediaExtractor (the NDK extractor path, FileSource) reads the track count of the same WAV by path and by fd, so a
     * failing MediaPlayer line can be set against an extractor that works in the same process.
     */
    private void mediaExtractorWav() {
        File wav;
        try {
            wav = writeWav("extractor.wav");
        } catch (IOException e) {
            Log.i("AtlTest", "mediaextractor: no file " + thrown(e));
            return;
        }
        String[] how = {"path", "fd"};
        for (int i = 0; i < how.length; i++) {
            MediaExtractor ex = new MediaExtractor();
            try {
                if (i == 0) {
                    ex.setDataSource(wav.getPath());
                } else {
                    FileInputStream in = new FileInputStream(wav);
                    try {
                        ex.setDataSource(in.getFD());
                    } finally {
                        in.close();
                    }
                }
                Log.i("AtlTest", "mediaextractor: " + how[i] + " tracks " + ex.getTrackCount());
            } catch (Throwable t) {
                Log.i("AtlTest", "mediaextractor: " + how[i] + " " + thrown(t));
            } finally {
                ex.release();
            }
        }
    }

    /**
     * media.extractor end to end (app.rs MEDIA_WAV): a WAV is set, prepared, started, sought and reset, per round, on a worker (joinProbe) so a round that
     * never returns is named. A failing step logs its throwable.
     */
    private void mediaPlayerWav() {
        final File wav;
        try {
            wav = writeWav("probe.wav");
        } catch (IOException e) {
            Log.i("AtlTest", "mediaplayer-wav: no file " + thrown(e));
            return;
        }
        final AtomicReference<String> at = new AtomicReference<String>("setDataSource");
        Thread worker = new Thread(new Runnable() {
            @Override
            public void run() {
                for (int i = 0; i < WAV_ROUNDS; i++) {
                    at.set("setDataSource");
                    MediaPlayer mp = new MediaPlayer();
                    try {
                        if (i % 2 == 0) {
                            FileInputStream in = new FileInputStream(wav);
                            try {
                                mp.setDataSource(in.getFD());
                            } finally {
                                in.close();
                            }
                        } else {
                            mp.setDataSource(wav.getPath());
                        }
                        at.set("prepare");
                        mp.prepare();
                        at.set("start");
                        mp.start();
                        at.set("seekTo");
                        mp.seekTo(WAV_SEEK_MS);
                        at.set("reset");
                        long t0 = System.nanoTime();
                        mp.reset();
                        long ms = (System.nanoTime() - t0) / 1000000L;
                        Log.i("AtlTest", "mediaplayer-wav: round " + i + " ok " + ms + " ms");
                    } catch (Throwable t) {
                        Log.i("AtlTest", "mediaplayer-wav: round " + i + " " + at.get() + " " + thrown(t));
                    } finally {
                        mp.release();
                    }
                }
            }
        });
        worker.start();
        joinProbe(worker, "mediaplayer-wav", WAV_ROUNDS, at);
    }

    /**
     * l1-restart regression (app.rs MEDIA_RESET): a seek is issued and reset runs at once, so the seek-complete notify can still be pending in the
     * service while reset waits for the service. On a worker thread (joinProbe): each round gets ROUND_BOUND_MS; a reset that never returns leaves its round
     * line out and the probe's line says "no answer within ... at reset".
     */
    private void mediaPlayerReset() {
        final AtomicReference<String> at = new AtomicReference<String>("setDataSource");
        Thread worker = new Thread(new Runnable() {
            @Override
            public void run() {
                int i = 0;
                try {
                    File wav = writeWav("reset.wav");
                    for (i = 0; i < RESET_ROUNDS; i++) {
                        MediaPlayer mp = new MediaPlayer();
                        try {
                            at.set("setDataSource");
                            mp.setDataSource(wav.getPath());
                            at.set("prepare");
                            mp.prepare();
                            at.set("start");
                            mp.start();
                            at.set("seekTo");
                            mp.seekTo(WAV_SEEK_MS);
                            at.set("reset");
                            long t0 = System.nanoTime();
                            mp.reset();
                            long ms = (System.nanoTime() - t0) / 1000000L;
                            Log.i("AtlTest", "mediaplayer-reset: round " + i + " ok " + ms + " ms");
                        } finally {
                            mp.release();
                        }
                    }
                } catch (Throwable t) {
                    Log.i("AtlTest", "mediaplayer-reset: round " + i + " " + at.get() + " " + thrown(t));
                }
            }
        });
        worker.start();
        joinProbe(worker, "mediaplayer-reset", RESET_ROUNDS, at);
    }

    /**
     * Waits for a probe's worker: rounds * ROUND_BOUND_MS in all. A worker still running after that is named with the step it was in, so a round that
     * never returns reads "no answer within <ms> ms at <step>" (app.rs round_failures).
     */
    private void joinProbe(Thread worker, String name, int rounds, AtomicReference<String> at) {
        try {
            worker.join(rounds * ROUND_BOUND_MS);
        } catch (InterruptedException e) {
            Log.i("AtlTest", name + ": interrupted " + e);
        }
        if (worker.isAlive()) {
            Log.i("AtlTest", name + ": no answer within " + rounds * ROUND_BOUND_MS + " ms at " + at.get());
        }
    }

    /** A PCM WAV of silence, WAV_SECONDS long at 8 kHz, 16-bit mono, in the app's files dir: no asset to ship. */
    private File writeWav(String name) throws IOException {
        int rate = 8000;
        int dataLen = rate * 2 * WAV_SECONDS;
        ByteBuffer b = ByteBuffer.allocate(44 + dataLen).order(ByteOrder.LITTLE_ENDIAN);
        b.put("RIFF".getBytes()).putInt(36 + dataLen).put("WAVE".getBytes());
        b.put("fmt ".getBytes()).putInt(16).putShort((short) 1).putShort((short) 1);
        b.putInt(rate).putInt(rate * 2).putShort((short) 2).putShort((short) 16);
        b.put("data".getBytes()).putInt(dataLen);
        File f = new File(getFilesDir(), name);
        FileOutputStream out = new FileOutputStream(f);
        try {
            out.write(b.array());
        } finally {
            out.close();
        }
        return f;
    }
}
