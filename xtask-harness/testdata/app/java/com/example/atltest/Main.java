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
 * Service lines (app.rs SERVICE_LINES): connectivity = the host's default network as the `connectivity` service reports it, queried and through registerDefaultNetworkCallback; permission = PackageManager.checkPermission through `permissionmgr`; else `absent` / what it threw.
 * Features line (app.rs FEATURES): system features through PackageManager against this process's own GL.
 * media.player line (app.rs MEDIA_PLAYER_SERVICE): setDataSource(fd) reaches the served service and returns.
 * WAV lines (app.rs MEDIA_WAV): a valid WAV is prepared, started, sought and reset per round, by fd and by path; the served extractor must sniff it.
 * Reset lines (app.rs MEDIA_RESET): reset with a seek pending, each within a bound; a reset that never returns is named.
 * result lines (app.rs RESULT): after the marker, Main starts ResultChild for request 7; its result comes back in onActivityResult, and the singleTop self-start from it cancels request 9 before its new intent.
 * sdkext line (app.rs ASYNC_LINES `sdkext`): SdkExtensions.getExtensionVersion for R and for AD_SERVICES, the values bp2ninja's sdkext.go derives for build.prop.
 * timezone line (app.rs ASYNC_LINES `timezone`): TimeZone.getDefault()'s id and raw offset, from persist.sys.timezone (the host zone, RuntimeInit.java:252).
 */
public class Main extends Activity {
    /** A root every supported distro bundle carries (Let's Encrypt's). */
    private static final String ROOT = "CN=ISRG Root X1";
    /** The result round trip's request codes (app.rs RESULT): the child's, and the singleTop self-start's, which is cancelled. */
    private static final int REQUEST_CHILD = 7;
    private static final int REQUEST_SELF = 9;

    /**
     * The priority line (app.rs PRIORITY): on a fresh thread, as Firebase's executor and ART's HandlerThread do, Process.setThreadPriority to BACKGROUND, back to the start value, then FOREGROUND;
     * `start=<n> background=<ok|throwable>/<nice after> restore=... foreground=...`, each nice read back with Process.getThreadPriority.
     */
    private static String priority() {
        final StringBuilder out = new StringBuilder();
        Thread t = new Thread(new Runnable() { @Override public void run() {
            int tid = android.os.Process.myTid();
            int start = android.os.Process.getThreadPriority(tid);
            out.append("start=").append(start);
            int[] asks = {android.os.Process.THREAD_PRIORITY_BACKGROUND, start, android.os.Process.THREAD_PRIORITY_FOREGROUND};
            String[] names = {"background", "restore", "foreground"};
            for (int i = 0; i < asks.length; i++) {
                String r;
                try {
                    android.os.Process.setThreadPriority(asks[i]);
                    r = "ok";
                } catch (Throwable e) {
                    r = e.getClass().getSimpleName();
                }
                out.append(' ').append(names[i]).append('=').append(r).append('/').append(android.os.Process.getThreadPriority(tid));
            }
        } });
        t.start();
        try {
            t.join();
        } catch (InterruptedException e) {
            return "interrupted";
        }
        return out.toString();
    }

    /**
     * CTS's default-UA pattern (cts WebSettingsTest.checkUserAgentStringHelper, CDD 3.4.1/C-1-3), with find() as CTS uses it. Release non-REL builds: CTS is lenient, `[^;]+`.
     */
    private static boolean ctsUserAgent(String ua) {
        String release = "REL".equals(android.os.Build.VERSION.CODENAME) ? java.util.regex.Pattern.quote(android.os.Build.VERSION.RELEASE) : "[^;]+";
        String p = java.util.regex.Pattern.quote("Mozilla/5.0 (Linux; Android ") + release + ";"
                + "( " + java.util.regex.Pattern.quote(android.os.Build.MODEL) + ")?"
                + "( Build/" + java.util.regex.Pattern.quote(android.os.Build.ID) + ")?"
                + "((?<=;)|(?<!;);)"
                + java.util.regex.Pattern.quote(" wv) ")
                + "AppleWebKit/\\d+\\.\\d+ "
                + java.util.regex.Pattern.quote("(KHTML, like Gecko) Version/4.0 ")
                + "Chrome/\\d+\\.\\d+\\.\\d+\\.\\d+ "
                + "(Mobile )?Safari/\\d+\\.\\d+";
        return java.util.regex.Pattern.compile(p).matcher(ua).find();
    }

    /**
     * The WebView UA line (app.rs WEBVIEW_UA): WebSettings.getDefaultUserAgent, AdMob's first WebView call; `cts=<pattern matched> release=<R>|model=<M>|id=<I> ua=<string>` (ua last: it has spaces),
     * or `threw=` the throwable's class and each cause's, outermost first, joined by `<`, then ` at=` the innermost one's top frame (class.method).
     */
    private String webView() {
        try {
            String ua = android.webkit.WebSettings.getDefaultUserAgent(this);
            return "cts=" + ctsUserAgent(ua) + " release=" + android.os.Build.VERSION.RELEASE + "|model=" + android.os.Build.MODEL + "|id=" + android.os.Build.ID + " ua=" + ua;
        } catch (Throwable e) {
            StringBuilder out = new StringBuilder("threw=");
            Throwable last = e;
            for (Throwable t = e; t != null; t = t.getCause()) {
                if (t != e) out.append('<');
                out.append(t.getClass().getName());
                last = t;
            }
            StackTraceElement[] frames = last.getStackTrace();
            if (frames.length > 0) out.append(" at=").append(frames[0].getClassName()).append('.').append(frames[0].getMethodName());
            return out.toString();
        }
    }

    /** The JS bridge object of webViewPage: two annotated methods and one a page must not reach (no @JavascriptInterface). */
    public static final class Bridge {
        @android.webkit.JavascriptInterface
        public int add(int a, int b) { return a + b; }

        @android.webkit.JavascriptInterface
        public String echo(String s) { return s + "!"; }

        public String hidden() { return "reached"; }
    }

    /** The page webViewPage loads; its title reaches onReceivedTitle in no fixed order with onPageFinished (CTS WebChromeClientTest.testOnReceivedTitle polls for it after the load completes). */
    static final String WEBVIEW_PAGE = "<html><head><title>atl</title></head><body>page</body></html>";

    /** The script run after onPageFinished: the bridge round trip (number and string), the unannotated method's absence, the title. */
    static final String WEBVIEW_SCRIPT = "[typeof atl.add, atl.add(2, 3), atl.echo('hi'), typeof atl.hidden, document.title]";

    /**
     * The WebView page line (app.rs WEBVIEW_PAGE): an unattached WebView (no composition before W3) with JavaScript on and the Bridge as `atl` loads WEBVIEW_PAGE through loadData;
     * `events=` the WebViewClient callbacks in order (started, finished); `titles=` every onReceivedTitle; `js=` evaluateJavascript(WEBVIEW_SCRIPT)'s JSON, run at the first onPageFinished.
     * Logged once both the script's result and a title have arrived (UI thread only: no locking); or `threw=` as webView().
     */
    private void webViewPage() {
        try {
            final StringBuilder events = new StringBuilder();
            final StringBuilder titles = new StringBuilder();
            final String[] js = new String[1];
            final Runnable report = new Runnable() {
                private boolean mLogged;
                @Override public void run() {
                    if (mLogged || js[0] == null || titles.length() == 0) return;
                    mLogged = true;
                    Log.i("AtlTest", "webview-page: events=" + events + " titles=" + titles + " js=" + js[0]);
                }
            };
            android.webkit.WebView w = new android.webkit.WebView(this);
            w.getSettings().setJavaScriptEnabled(true);
            w.addJavascriptInterface(new Bridge(), "atl");
            w.setWebChromeClient(new android.webkit.WebChromeClient() {
                @Override public void onReceivedTitle(android.webkit.WebView view, String title) {
                    titles.append(title).append(',');
                    report.run();
                }
            });
            w.setWebViewClient(new android.webkit.WebViewClient() {
                private boolean mDone;
                @Override public void onPageStarted(android.webkit.WebView view, String url, android.graphics.Bitmap favicon) { events.append("started,"); }
                @Override public void onPageFinished(android.webkit.WebView view, String url) {
                    events.append("finished,");
                    if (mDone) return;
                    mDone = true;
                    // an anonymous class: the fixture's javac has no LambdaMetafactory on its boot classpath
                    view.evaluateJavascript(WEBVIEW_SCRIPT, new android.webkit.ValueCallback<String>() {
                        @Override public void onReceiveValue(String v) {
                            js[0] = v;
                            report.run();
                        }
                    });
                }
            });
            mWebView = w;
            w.loadData(WEBVIEW_PAGE, "text/html", null);
        } catch (Throwable e) {
            StringBuilder out = new StringBuilder("threw=");
            for (Throwable t = e; t != null; t = t.getCause()) {
                if (t != e) out.append('<');
                out.append(t.getClass().getName()).append('(').append(t.getMessage()).append(')');
            }
            Log.i("AtlTest", "webview-page: " + out);
        }
    }

    /** Keeps webViewPage's WebView reachable until its line is logged. */
    private android.webkit.WebView mWebView;
    /** The same for webViewCookieWeb's and webViewDownload's. */
    private android.webkit.WebView mCookieView, mDownloadView;

    static final String COOKIE_URL = "https://atl.test/";

    /**
     * The cookie set line (app.rs WEBVIEW_COOKIE_SET, webview plan D5): a worker thread (no Looper) sets a cookie and reads it back at once (`get=`, `has=`); the callback reports `cb=` and
     * runs on the main Looper (`main=`, CookieManager.java javadoc: the caller's Looper, the main one here). Then webViewCookieWeb.
     */
    private void webViewCookies() {
        final String[] got = new String[2];
        final java.util.concurrent.CountDownLatch read = new java.util.concurrent.CountDownLatch(1);
        new Thread(new Runnable() {
            @Override public void run() {
                try {
                    final android.webkit.CookieManager cm = android.webkit.CookieManager.getInstance();
                    cm.setCookie(COOKIE_URL, "a=1; Path=/", new android.webkit.ValueCallback<Boolean>() {
                        @Override public void onReceiveValue(Boolean ok) {
                            try { read.await(); } catch (InterruptedException e) { }
                            boolean main = android.os.Looper.myLooper() == android.os.Looper.getMainLooper();
                            Log.i("AtlTest", "webview-cookie-set: get=" + got[0] + " has=" + got[1] + " cb=" + ok + " main=" + main);
                            webViewCookieWeb(cm);
                        }
                    });
                    got[0] = cm.getCookie(COOKIE_URL);
                    got[1] = String.valueOf(cm.hasCookies());
                } catch (Throwable e) {
                    Log.i("AtlTest", "webview-cookie-set: threw=" + e);
                }
                read.countDown();
            }
        }, "cookie-worker").start();
    }

    /**
     * The web cookie line (app.rs WEBVIEW_COOKIE_WEB, webview plan D5): a page at COOKIE_URL sets `w=2` through document.cookie (`page=` its document.cookie after the set, the engine has it);
     * from then a worker polls CookieManager.getCookie until w=2 shows: `visible=` and `window-us=` the time it took (D5's accepted cost: the copy learns of web-set cookies late).
     */
    private void webViewCookieWeb(final android.webkit.CookieManager cm) {
        android.webkit.WebView w = new android.webkit.WebView(this);
        w.getSettings().setJavaScriptEnabled(true);
        w.setWebViewClient(new android.webkit.WebViewClient() {
            private boolean mDone;
            @Override public void onPageFinished(android.webkit.WebView view, String url) {
                if (mDone) return;
                mDone = true;
                view.evaluateJavascript("document.cookie = 'w=2; path=/'; document.cookie", new android.webkit.ValueCallback<String>() {
                    @Override public void onReceiveValue(final String page) {
                        final long t0 = System.nanoTime();
                        new Thread(new Runnable() {
                            @Override public void run() {
                                String c = null;
                                while (System.nanoTime() - t0 < 5000000000L) {
                                    c = cm.getCookie(COOKIE_URL);
                                    if (c != null && c.contains("w=2")) break;
                                    try { Thread.sleep(1); } catch (InterruptedException e) { break; }
                                }
                                long us = (System.nanoTime() - t0) / 1000;
                                boolean visible = c != null && c.contains("w=2");
                                Log.i("AtlTest", "webview-cookie-web: visible=" + visible + " window-us=" + us + " page=" + page);
                            }
                        }, "cookie-poll").start();
                    }
                });
            }
        });
        mCookieView = w;
        w.loadDataWithBaseURL(COOKIE_URL, "<html><body>cookie</body></html>", "text/html", null, null);
    }

    /**
     * The download line (app.rs WEBVIEW_DOWNLOAD): a response the engine does not render (a MIME type it has no viewer for) reaches DownloadListener.onDownloadStart instead:
     * `mime=` its type, `ua=` whether its userAgent is the view's WebSettings.getUserAgentString.
     */
    /**
     * The initial scale line (app.rs WEBVIEW_SCALE): setInitialScale(200) = 2 device pixels per CSS pixel (WebView.java:1184-1188, density not applied), what the page reads as devicePixelRatio.
     */
    private void webViewScale() {
        try {
            android.webkit.WebView w = new android.webkit.WebView(this);
            w.getSettings().setJavaScriptEnabled(true);
            w.setInitialScale(200);
            w.setWebViewClient(new android.webkit.WebViewClient() {
                private boolean mDone;
                @Override public void onPageFinished(android.webkit.WebView view, String url) {
                    if (mDone) return;
                    mDone = true;
                    view.evaluateJavascript("window.devicePixelRatio", new android.webkit.ValueCallback<String>() {
                        @Override public void onReceiveValue(String v) { Log.i("AtlTest", "webview-scale: dpr=" + v); }
                    });
                }
            });
            mScaleView = w;
            w.loadData("<html><body>scale</body></html>", "text/html", null);
        } catch (Throwable e) {
            Log.i("AtlTest", "webview-scale: threw=" + e);
        }
    }

    private android.webkit.WebView mScaleView;

    /**
     * The class loader line (app.rs WEBVIEW_CLASSLOADER): WebView.getWebViewClassLoader loads the provider's own factory class (`provider=`, the class WebViewFactory instantiated), and a
     * Chromium support-library glue class is absent (`glue=`), the probe androidx.webkit makes.
     */
    private String webViewClassLoader() {
        try {
            ClassLoader cl = android.webkit.WebView.getWebViewClassLoader();
            Class<?> p = Class.forName("com.android.webview.chromium.WebViewChromiumFactoryProviderForT", false, cl);
            boolean glue;
            try {
                Class.forName("org.chromium.support_lib_glue.SupportLibReflectionUtil", false, cl);
                glue = true;
            } catch (ClassNotFoundException e) {
                glue = false;
            }
            return "provider=" + (p.getClassLoader() == cl) + " glue=" + glue;
        } catch (Throwable e) {
            return "threw=" + e;
        }
    }

    private void webViewDownload() {
        try {
            final android.webkit.WebView w = new android.webkit.WebView(this);
            w.setDownloadListener(new android.webkit.DownloadListener() {
                @Override public void onDownloadStart(String url, String userAgent, String contentDisposition, String mimetype, long contentLength) {
                    Log.i("AtlTest", "webview-download: mime=" + mimetype + " ua=" + userAgent.equals(w.getSettings().getUserAgentString()));
                }
            });
            mDownloadView = w;
            w.loadData("x", "application/x-atl-test", null);
        } catch (Throwable e) {
            Log.i("AtlTest", "webview-download: threw=" + e);
        }
    }

    /**
     * The WebView settings line (app.rs WEBVIEW_SETTINGS, webview plan S4.1): a WebView reads back what it sets across the kinds (a flag and an int the engine applies, a text the engine applies, an int the core stores), and AOSP's defaults for what it leaves alone.
     */
    private void webViewSettings() {
        try {
            android.webkit.WebSettings s = new android.webkit.WebView(this).getSettings();
            s.setJavaScriptEnabled(true);
            s.setDefaultFontSize(20);
            s.setSerifFontFamily("atl-serif");
            s.setTextZoom(150);
            Log.i("AtlTest", "webview-settings: js=" + s.getJavaScriptEnabled() + " font-size=" + s.getDefaultFontSize() + " serif=" + s.getSerifFontFamily() + " text-zoom=" + s.getTextZoom() + " min-font=" + s.getMinimumFontSize() + " zoom=" + s.getDefaultZoom() + " layout=" + s.getLayoutAlgorithm() + " support-zoom=" + s.supportZoom());
        } catch (Throwable e) {
            Log.i("AtlTest", "webview-settings: threw=" + e);
        }
    }

    /** The WebView package line (app.rs WEBVIEW_PACKAGE): WebView.getCurrentWebViewPackage's name and whether it is a system app; `null` without one. */
    private String webViewPackage() {
        android.content.pm.PackageInfo p = android.webkit.WebView.getCurrentWebViewPackage();
        if (p == null) return "null";
        boolean system = (p.applicationInfo.flags & android.content.pm.ApplicationInfo.FLAG_SYSTEM) != 0;
        return "name=" + p.packageName + " system=" + system;
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("AtlTest", "cacerts: " + caCerts());
        Log.i("AtlTest", "fonts: default=" + advance(Typeface.DEFAULT) + " monospace=" + advance(Typeface.MONOSPACE));
        Log.i("AtlTest", "soundpool: " + soundPool());
        mediaPlayer();
        Log.i("AtlTest", "connectivity: " + connectivity());
        Log.i("AtlTest", "permission: " + permission());
        Log.i("AtlTest", "notification: " + channels());
        Log.i("AtlTest", "power: " + power());
        Log.i("AtlTest", "uimode: " + uiMode());
        Log.i("AtlTest", "keyguard: " + keyguard());
        Log.i("AtlTest", "appops: " + appOps());
        Log.i("AtlTest", "jobscheduler: " + jobs());
        Log.i("AtlTest", "queryservices: " + queryServices());
        jobRun();
        Log.i("AtlTest", "bindservice: " + bound());
        bindImplicit();
        bindMissing();
        Log.i("AtlTest", "pendingintent: " + pendingIntent());
        Log.i("AtlTest", "processes: " + processes());
        Log.i("AtlTest", "sysanswers: " + sysAnswers());
        Log.i("AtlTest", "settings: " + settings());
        Log.i("AtlTest", "features: " + features());
        Log.i("AtlTest", "sdkext: r=" + android.os.ext.SdkExtensions.getExtensionVersion(android.os.Build.VERSION_CODES.R)
                + " ad_services=" + android.os.ext.SdkExtensions.getExtensionVersion(android.os.ext.SdkExtensions.AD_SERVICES));
        Log.i("AtlTest", "timezone: id=" + java.util.TimeZone.getDefault().getID() + " raw=" + java.util.TimeZone.getDefault().getRawOffset());
        Log.i("AtlTest", "signatures: " + signatures());
        Log.i("AtlTest", "priority: " + priority());
        Log.i("AtlTest", "webview-package: " + webViewPackage());
        // the UA on a worker thread before any WebView exists, as AdMob's AdWorker asks it; then webview-page's WebView on this (UI) thread: the engine must stay the UI thread's.
        // The UI thread does not wait for the worker (the provider may hop to the UI thread for the UA, as Chromium's does).
        final android.os.Handler ui = new android.os.Handler(android.os.Looper.getMainLooper());
        new Thread(new Runnable() {
            @Override public void run() {
                final String ua = webView();
                ui.post(new Runnable() {
                    @Override public void run() {
                        Log.i("AtlTest", "webview-ua: " + ua);
                        webViewSettings();
                        webViewPage();
                        webViewCookies();
                        webViewDownload();
                        webViewScale();
                        Log.i("AtlTest", "webview-classloader: " + webViewClassLoader());
                    }
                });
            }
        }, "ua-worker").start();
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
            bridge1 = jni;
        }
        bridge1Legacy();
        resultToSecurity();
        Log.i("AtlTest", "onCreate reached");
        implicitNone();
        startActivityForResult(new Intent(ACTION_RESULT_CHILD), REQUEST_CHILD);
    }

    /** ResultChild's filter action: the result child is started by an implicit intent the package manager resolves to it (app.rs CALLING). */
    private static final String ACTION_RESULT_CHILD = "atl.test.RESULT_CHILD";

    /** "implicit-none <outcome>" (app.rs IMPLICIT_NONE): a market:// VIEW nothing installed handles throws ActivityNotFoundException (START_INTENT_NOT_RESOLVED), as Doodle Jump expects. */
    private void implicitNone() {
        String outcome;
        try {
            startActivity(new Intent(Intent.ACTION_VIEW, android.net.Uri.parse("market://details?id=com.example.atltest")));
            outcome = "started";
        } catch (android.content.ActivityNotFoundException e) {
            outcome = "threw ActivityNotFoundException";
        } catch (RuntimeException e) {
            outcome = "threw " + e.getClass().getName();
        }
        Log.i("AtlTest", "implicit-none " + outcome);
    }

    /** IActivityTaskManager.aidl: startActivity is the interface's first method, so FIRST_CALL_TRANSACTION. */
    private static final int START_ACTIVITY = android.os.IBinder.FIRST_CALL_TRANSACTION;
    private static final String ATM_DESCRIPTOR = "android.app.IActivityTaskManager";

    /**
     * "result-to-security <outcome>" (app.rs RESULT_TO_SECURITY): IActivityTaskManager.startActivity with this activity as resultTo but another calling package, as the generated proxy parcels it;
     * the services refuse it fail-closed with a SecurityException (activity_start.rs check), counted by the reply hook as exception:SECURITY. The binder is raw: ActivityTaskManager.getService is not
     * on the greylist, ServiceManager.getService, currentActivityThread, getApplicationThread and getActivityToken are (@UnsupportedAppUsage).
     */
    private void resultToSecurity() {
        String outcome;
        android.os.Parcel data = android.os.Parcel.obtain();
        android.os.Parcel reply = android.os.Parcel.obtain();
        try {
            android.os.IBinder atm = (android.os.IBinder) Class.forName("android.os.ServiceManager")
                    .getMethod("getService", String.class).invoke(null, "activity_task");
            Object thread = Class.forName("android.app.ActivityThread").getMethod("currentActivityThread").invoke(null);
            Object appThread = thread.getClass().getMethod("getApplicationThread").invoke(thread);
            android.os.IBinder token = (android.os.IBinder) Activity.class.getMethod("getActivityToken").invoke(this);
            data.writeInterfaceToken(ATM_DESCRIPTOR);
            data.writeStrongBinder(((android.os.IInterface) appThread).asBinder());
            data.writeString("com.example.other");
            data.writeString(null);
            data.writeInt(1);
            new Intent(this, ResultChild.class).writeToParcel(data, 0);
            data.writeString(null);
            data.writeStrongBinder(token);
            data.writeString(null);
            data.writeInt(-1);
            data.writeInt(0);
            data.writeInt(0);
            data.writeInt(0);
            atm.transact(START_ACTIVITY, data, reply, 0);
            reply.readException();
            outcome = "returned " + reply.readInt();
        } catch (SecurityException e) {
            outcome = "threw SecurityException";
        } catch (Exception e) {
            outcome = "failed " + e;
        } finally {
            data.recycle();
            reply.recycle();
        }
        Log.i("AtlTest", "result-to-security " + outcome);
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
        requestPermissions(new String[] {POST_NOTIFICATIONS}, REQUEST_PERMISSION);
    }

    /** The permission request after the result round trip (app.rs PERMISSION_REQUEST). */
    private static final int REQUEST_PERMISSION = 11;
    private static final String POST_NOTIFICATIONS = "android.permission.POST_NOTIFICATIONS";

    /** "permission-request request=<r> names=<[..]> results=<[..]> self=<int>": the callback's arguments and checkSelfPermission after it (app.rs PERMISSION_REQUEST). */
    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        Log.i("AtlTest", "permission-request request=" + requestCode + " names=" + java.util.Arrays.toString(permissions)
                + " results=" + java.util.Arrays.toString(grantResults) + " self=" + checkSelfPermission(POST_NOTIFICATIONS));
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
            // tr-first blocker 5 (b): by the child's result the main Looper has run, so looperArm's callback has had its later call
            if (bridge1 != null) {
                bridge1Call(bridge1, "looperLater", "looperLater");
            }
            startActivityForResult(new Intent(this, Main.class), REQUEST_SELF);
        }
    }

    /** How long the default-network callback may take: online it arrives at registration; offline none comes, and this is the wait that shows it. */
    private static final long CALLBACK_WAIT_MS = 5000;

    /**
     * "active=<type>/<state> metered=<bool> internet=<bool> validated=<bool> iface=<name> callback=<cb>" from ConnectivityManager: the active NetworkInfo's type name and state ("null" without one), isActiveNetworkMetered,
     * the active network's INTERNET / VALIDATED capabilities and link interface; cb = what registerDefaultNetworkCallback delivered within CALLBACK_WAIT_MS: "same validated=<bool> iface=<name>" when its network equals
     * getActiveNetwork ("other ..." when not), "none" when nothing came. "absent" when getSystemService returns null (Cut the Rope's NPE); else what it threw.
     */
    private String connectivity() {
        try {
            final android.net.ConnectivityManager cm =
                    (android.net.ConnectivityManager) getSystemService(CONNECTIVITY_SERVICE);
            if (cm == null) {
                return "absent";
            }
            android.net.NetworkInfo info = cm.getActiveNetworkInfo();
            android.net.Network active = cm.getActiveNetwork();
            android.net.NetworkCapabilities nc = active == null ? null : cm.getNetworkCapabilities(active);
            android.net.LinkProperties lp = active == null ? null : cm.getLinkProperties(active);
            String line = "active=" + (info == null ? "null" : info.getTypeName() + "/" + info.getState())
                    + " metered=" + cm.isActiveNetworkMetered()
                    + " internet=" + (nc != null && nc.hasCapability(android.net.NetworkCapabilities.NET_CAPABILITY_INTERNET))
                    + " validated=" + (nc != null && nc.hasCapability(android.net.NetworkCapabilities.NET_CAPABILITY_VALIDATED))
                    + " iface=" + (lp == null ? "null" : lp.getInterfaceName());
            final java.util.concurrent.CountDownLatch done = new java.util.concurrent.CountDownLatch(1);
            final AtomicReference<String> got = new AtomicReference<>("none");
            final android.net.Network expect = active;
            // onAvailable(network, caps, lp, ...) calls onAvailable, onCapabilitiesChanged, onLinkPropertiesChanged in that order (ConnectivityManager.NetworkCallback)
            android.net.ConnectivityManager.NetworkCallback cb = new android.net.ConnectivityManager.NetworkCallback() {
                private android.net.NetworkCapabilities caps;

                @Override
                public void onCapabilitiesChanged(android.net.Network n, android.net.NetworkCapabilities c) {
                    caps = c;
                }

                @Override
                public void onLinkPropertiesChanged(android.net.Network n, android.net.LinkProperties l) {
                    got.set((n.equals(expect) ? "same" : "other")
                            + " validated=" + (caps != null && caps.hasCapability(android.net.NetworkCapabilities.NET_CAPABILITY_VALIDATED))
                            + " iface=" + l.getInterfaceName());
                    done.countDown();
                }
            };
            cm.registerDefaultNetworkCallback(cb);
            done.await(CALLBACK_WAIT_MS, java.util.concurrent.TimeUnit.MILLISECONDS);
            cm.unregisterNetworkCallback(cb);
            return line + " callback=" + got.get();
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /**
     * "importance=<int> name=<name> visibility=<int> count=<n> absent=<channel> raised=<int>": a channel created at IMPORTANCE_LOW and read back, the channel count, an unknown id, and the importance
     * after re-creating it at IMPORTANCE_HIGH (Unity's registerNotificationChannel path), then " notify=ok" when notifications on that channel post, update and cancel, else the exception's class;
     * "absent" when getSystemService returns null; else what it threw.
     */
    private String channels() {
        try {
            android.app.NotificationManager nm =
                    (android.app.NotificationManager) getSystemService(NOTIFICATION_SERVICE);
            if (nm == null) {
                return "absent";
            }
            nm.createNotificationChannel(new android.app.NotificationChannel(
                    "atl.test", "ATL test", android.app.NotificationManager.IMPORTANCE_LOW));
            android.app.NotificationChannel c = nm.getNotificationChannel("atl.test");
            String read = c == null ? "importance=none"
                    : "importance=" + c.getImportance() + " name=" + c.getName() + " visibility=" + c.getLockscreenVisibility();
            int count = nm.getNotificationChannels().size();
            android.app.NotificationChannel absent = nm.getNotificationChannel("atl.none");
            nm.createNotificationChannel(new android.app.NotificationChannel(
                    "atl.test", "ATL test", android.app.NotificationManager.IMPORTANCE_HIGH));
            android.app.NotificationChannel again = nm.getNotificationChannel("atl.test");
            // nothing was posted, so AOSP's cancel finds no record and returns; the services' count shows it was served (app.rs SERVICE_CALLS)
            nm.cancel("atl.tag", 7);
            // a posted notification (GMS's availability notification in Doodle Jump): shown, updated in place, then cancelled; then one more after the desktop's service went away, which the app
            // sees succeed as any post (app.rs NOTIFY_CALLS: the gate's fake service leaves the bus when it closes id 8's notification)
            String posted;
            try {
                nm.notify("atl.tag", 8, new android.app.Notification.Builder(this, "atl.test")
                        .setSmallIcon(android.R.drawable.ic_dialog_info)
                        .setContentTitle("ATL test")
                        .setContentText("posted")
                        .build());
                nm.notify("atl.tag", 8, new android.app.Notification.Builder(this, "atl.test")
                        .setSmallIcon(android.R.drawable.ic_dialog_info)
                        .setContentTitle("ATL test")
                        .setContentText("updated")
                        .build());
                nm.cancel("atl.tag", 8);
                nm.notify("atl.tag", 9, new android.app.Notification.Builder(this, "atl.test")
                        .setSmallIcon(android.R.drawable.ic_dialog_info)
                        .setContentTitle("ATL test")
                        .setContentText("no service")
                        .build());
                nm.cancel("atl.tag", 9);
                posted = "ok";
            } catch (Throwable t) {
                posted = t.getClass().getSimpleName();
            }
            return read + " count=" + count + " absent=" + absent + " raised=" + (again == null ? "none" : again.getImportance()) + " notify=" + posted;
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /**
     * "interactive=<bool> powersave=<bool> partial=<bool> held=<bool> released=<bool>": PowerManager (vungle's AndroidPlatform.<init> path) answers isInteractive / isPowerSaveMode /
     * isWakeLockLevelSupported(PARTIAL), and a PARTIAL wake lock's isHeld after acquire and after release; "absent" when getSystemService returns null; else what it threw.
     */
    /**
     * "id=<s> again=<bool> screen_off=<s> auto_time=<s> rotation=<s>": Settings.Secure ANDROID_ID (the SSAID of this package's signer), whether a second read equals it, and three keys AOSP's
     * DatabaseHelper loads from SettingsProvider's defaults.xml (System screen_off_timeout, Global auto_time, System accelerometer_rotation); "null" for an absent key; a throwable's class otherwise.
     */
    private String settings() {
        try {
            android.content.ContentResolver r = getContentResolver();
            String id = android.provider.Settings.Secure.getString(r, android.provider.Settings.Secure.ANDROID_ID);
            String again = android.provider.Settings.Secure.getString(r, android.provider.Settings.Secure.ANDROID_ID);
            return "id=" + id + " again=" + java.util.Objects.equals(id, again)
                    + " screen_off=" + android.provider.Settings.System.getString(r, android.provider.Settings.System.SCREEN_OFF_TIMEOUT)
                    + " auto_time=" + android.provider.Settings.Global.getString(r, android.provider.Settings.Global.AUTO_TIME)
                    + " rotation=" + android.provider.Settings.System.getString(r, android.provider.Settings.System.ACCELEROMETER_ROTATION);
        } catch (Throwable t) {
            return t.getClass().getSimpleName() + ": " + t.getMessage();
        }
    }

    /** Small system-service answers games read, one field each: what the call returned, or the throwable's simple name. */
    private String sysAnswers() {
        return "checksums=" + checksums() + " " + audio();
    }

    /** AudioManager as games read it before playing: STREAM_MUSIC volume / max, ringer mode, audio mode. */
    private String audio() {
        try {
            android.media.AudioManager am = (android.media.AudioManager) getSystemService(AUDIO_SERVICE);
            return "volume=" + am.getStreamVolume(android.media.AudioManager.STREAM_MUSIC) + " max=" + am.getStreamMaxVolume(android.media.AudioManager.STREAM_MUSIC)
                    + " ringer=" + am.getRingerMode() + " mode=" + am.getMode();
        } catch (Throwable t) {
            return "audio=" + t.getClass().getSimpleName();
        }
    }

    /** PackageManager.requestChecksums on the fixture itself (TRUST_ALL, SHA256 required): the number of checksums the listener got within 5 s, "timeout", or the throwable's simple name. */
    private String checksums() {
        try {
            java.util.concurrent.CountDownLatch done = new java.util.concurrent.CountDownLatch(1);
            int[] got = {-1};
            getPackageManager().requestChecksums(getPackageName(), false, android.content.pm.Checksum.TYPE_WHOLE_SHA256,
                    android.content.pm.PackageManager.TRUST_ALL, new android.content.pm.PackageManager.OnChecksumsReadyListener() {
                        @Override
                        public void onChecksumsReady(java.util.List<android.content.pm.ApkChecksum> checksums) {
                            got[0] = checksums.size();
                            done.countDown();
                        }
                    });
            return done.await(5, java.util.concurrent.TimeUnit.SECONDS) ? Integer.toString(got[0]) : "timeout";
        } catch (Throwable t) {
            return t.getClass().getSimpleName();
        }
    }

    /**
     * "count=<n> own=<n> uid=<bool> name=<bool> pkg=<bool> importance=<n>": ActivityManager.getRunningAppProcesses as SDK "am I the main process" checks read it: how many records, how many carry
     * Process.myPid(), whether every one has Process.myUid(), and the own record's processName (= ApplicationInfo.processName), pkgList (= the package) and importance; "null" for a null list.
     */
    private String processes() {
        try {
            java.util.List<android.app.ActivityManager.RunningAppProcessInfo> list =
                    ((android.app.ActivityManager) getSystemService(ACTIVITY_SERVICE)).getRunningAppProcesses();
            if (list == null) {
                return "null";
            }
            int own = 0;
            boolean uid = true;
            android.app.ActivityManager.RunningAppProcessInfo mine = null;
            for (android.app.ActivityManager.RunningAppProcessInfo i : list) {
                if (i.pid == android.os.Process.myPid()) {
                    own++;
                    mine = i;
                }
                uid &= i.uid == android.os.Process.myUid();
            }
            String rest = mine == null ? "" : " name=" + mine.processName.equals(getApplicationInfo().processName)
                    + " pkg=" + java.util.Arrays.equals(mine.pkgList, new String[] {getPackageName()}) + " importance=" + mine.importance;
            return "count=" + list.size() + " own=" + own + " uid=" + uid + rest;
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    private String power() {
        try {
            android.os.PowerManager pm = (android.os.PowerManager) getSystemService(POWER_SERVICE);
            if (pm == null) {
                return "absent";
            }
            android.os.PowerManager.WakeLock wl = pm.newWakeLock(android.os.PowerManager.PARTIAL_WAKE_LOCK, "atl:test");
            wl.acquire();
            boolean held = wl.isHeld();
            wl.release();
            return "interactive=" + pm.isInteractive() + " powersave=" + pm.isPowerSaveMode()
                    + " partial=" + pm.isWakeLockLevelSupported(android.os.PowerManager.PARTIAL_WAKE_LOCK)
                    + " held=" + held + " released=" + !wl.isHeld();
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /** JobIntentService's path: enqueue the same job twice (its work joins one job), read it back, cancel it; a job naming an activity as its service is refused. "absent" when getSystemService returns null. */
    /** JobIntentService's run: one work item for AtlJob.WORK_JOB, which AtlJob drains and logs (`jobrun:`). A failure to enqueue shows as the absent line. */
    private void jobRun() {
        try {
            android.app.job.JobScheduler js = (android.app.job.JobScheduler) getSystemService(JOB_SCHEDULER_SERVICE);
            if (js != null) {
                js.enqueue(new android.app.job.JobInfo.Builder(AtlJob.WORK_JOB, new android.content.ComponentName(this, AtlJob.class)).setOverrideDeadline(0).build(),
                        new android.app.job.JobWorkItem(new Intent("atl.test.WORK")));
            }
        } catch (Throwable t) {
            Log.i("AtlTest", "jobrun: threw " + t);
        }
    }

    /** bindService(AtlBound, BIND_AUTO_CREATE): onServiceConnected calls the service's binder (`bound:`), then unbinds, which reaches AtlBound.onUnbind (`unbind:`). Returns bindService's answer. */
    private String bound() {
        try {
            Intent intent = new Intent("atl.test.BIND").setClass(this, AtlBound.class);
            boolean ok = bindService(intent, new android.content.ServiceConnection() {
                @Override
                public void onServiceConnected(android.content.ComponentName name, android.os.IBinder service) {
                    String answer;
                    android.os.Parcel data = android.os.Parcel.obtain();
                    android.os.Parcel reply = android.os.Parcel.obtain();
                    try {
                        service.transact(android.os.IBinder.FIRST_CALL_TRANSACTION, data, reply, 0);
                        answer = String.valueOf(reply.readInt());
                    } catch (Throwable t) {
                        answer = "threw " + t;
                    } finally {
                        data.recycle();
                        reply.recycle();
                    }
                    Log.i("AtlTest", "bound: connected=" + name.getClassName() + " answer=" + answer);
                    unbindService(this);
                }

                @Override
                public void onServiceDisconnected(android.content.ComponentName name) {
                    Log.i("AtlTest", "bound-lost: " + name.getClassName());
                }
            }, BIND_AUTO_CREATE);
            return "bindService=" + ok;
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /** bindService(Intent atl.test.BIND with setPackage: implicit, no component), BIND_AUTO_CREATE: onServiceConnected logs the connected class with bindService's answer, then unbinds (`bindimplicit:`). */
    private void bindImplicit() {
        final boolean[] answer = {false};
        try {
            Intent intent = new Intent("atl.test.BIND").setPackage(getPackageName());
            answer[0] = bindService(intent, new android.content.ServiceConnection() {
                @Override
                public void onServiceConnected(android.content.ComponentName name, android.os.IBinder service) {
                    Log.i("AtlTest", "bindimplicit: bindService=" + answer[0] + " connected=" + name.getClassName());
                    unbindService(this);
                }

                @Override
                public void onServiceDisconnected(android.content.ComponentName name) {
                    Log.i("AtlTest", "bindimplicit-lost: " + name.getClassName());
                }
            }, BIND_AUTO_CREATE);
            if (!answer[0]) {
                Log.i("AtlTest", "bindimplicit: bindService=false connected=none");
            }
        } catch (Throwable t) {
            Log.i("AtlTest", "bindimplicit: threw " + t);
        }
    }

    /** bindService to the absent com.android.vending (Play billing's implicit-with-package Intent, then explicit), unbindService at once: AOSP answers false (bindServiceLocked found nothing: 0),
     * never calls the connection, and unbind finds the dispatcher bindServiceCommon registered before asking ActivityManager. Logged from a main-thread post so a queued callback is seen. */
    private void bindMissing() {
        final boolean[] called = {false};
        String r;
        try {
            android.content.ServiceConnection[] conns = new android.content.ServiceConnection[2];
            for (int i = 0; i < conns.length; i++) {
                conns[i] = new android.content.ServiceConnection() {
                    @Override
                    public void onServiceConnected(android.content.ComponentName name, android.os.IBinder service) {
                        called[0] = true;
                    }

                    @Override
                    public void onServiceDisconnected(android.content.ComponentName name) {
                        called[0] = true;
                    }

                    @Override
                    public void onNullBinding(android.content.ComponentName name) {
                        called[0] = true;
                    }
                };
            }
            r = "bindService=" + bindService(new Intent("com.android.vending.billing.InAppBillingService.BIND").setPackage("com.android.vending"), conns[0], BIND_AUTO_CREATE);
            r += " explicit=" + bindService(new Intent().setClassName("com.android.vending", "com.android.vending.billing.InAppBillingService"), conns[1], BIND_AUTO_CREATE);
            for (android.content.ServiceConnection c : conns) {
                unbindService(c);
            }
            r += " unbind=ok";
        } catch (Throwable t) {
            r = "threw " + t;
        }
        final String line = r;
        new android.os.Handler(android.os.Looper.getMainLooper()).post(new Runnable() {
            @Override
            public void run() {
                Log.i("AtlTest", "bindmissing: " + line + " called=" + called[0]);
            }
        });
    }

    /** queryIntentServices: the fixture's own atl.test.BIND service (AtlBound), then Play billing's Intent to the absent com.android.vending (empty, as on a device without Play). */
    private String queryServices() {
        try {
            java.util.List<android.content.pm.ResolveInfo> own = getPackageManager().queryIntentServices(new Intent("atl.test.BIND").setPackage(getPackageName()), 0);
            java.util.List<android.content.pm.ResolveInfo> billing = getPackageManager().queryIntentServices(
                    new Intent("com.android.vending.billing.InAppBillingService.BIND").setPackage("com.android.vending"), 0);
            String r = "own=" + own.size();
            for (android.content.pm.ResolveInfo ri : own) {
                r += " " + ri.serviceInfo.name;
            }
            return r + " billing=" + billing.size();
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    private String jobs() {
        try {
            android.app.job.JobScheduler js = (android.app.job.JobScheduler) getSystemService(JOB_SCHEDULER_SERVICE);
            if (js == null) {
                return "absent";
            }
            android.app.job.JobInfo job = new android.app.job.JobInfo.Builder(7, new android.content.ComponentName(this, AtlJob.class)).setOverrideDeadline(0).build();
            int first = js.enqueue(job, new android.app.job.JobWorkItem(new Intent("atl.test.JOB")));
            int again = js.enqueue(job, new android.app.job.JobWorkItem(new Intent("atl.test.JOB")));
            boolean pending = js.getPendingJob(7) != null;
            int count = js.getAllPendingJobs().size();
            js.cancel(7);
            boolean cancelled = js.getPendingJob(7) == null;
            String rejected;
            try {
                js.schedule(new android.app.job.JobInfo.Builder(8, new android.content.ComponentName(this, Main.class)).setOverrideDeadline(0).build());
                rejected = "none";
            } catch (Throwable t) {
                rejected = t.getClass().getSimpleName();
            }
            return "enqueue=" + first + " again=" + again + " pending=" + pending + " count=" + count + " cancelled=" + cancelled + " rejected=" + rejected;
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /** UiModeManager's mode type and night mode, and the uiMode of the app's Configuration they must agree with; "absent" when getSystemService returns null; else what it threw. */
    private String uiMode() {
        try {
            android.app.UiModeManager um = (android.app.UiModeManager) getSystemService(UI_MODE_SERVICE);
            if (um == null) {
                return "absent";
            }
            return "type=" + um.getCurrentModeType() + " night=" + um.getNightMode() + " config=" + getResources().getConfiguration().uiMode;
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /** KeyguardManager on a device with no lock-screen credential: the manager (its constructor needs `trust`, and the window service's keyguard calls), and isDeviceLocked, isKeyguardLocked, isDeviceSecure, isKeyguardSecure all false; "absent" when getSystemService returns null; else what it threw. */
    private String keyguard() {
        try {
            android.app.KeyguardManager km = getSystemService(android.app.KeyguardManager.class);
            if (km == null) {
                return "absent";
            }
            return "manager=yes locked=" + km.isDeviceLocked() + " keyguardLocked=" + km.isKeyguardLocked()
                    + " secure=" + km.isDeviceSecure() + " keyguardSecure=" + km.isKeyguardSecure();
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /**
     * AppOpsManager against the `appops` service: the mode of POST_NOTIFICATION (its runtime permission requested and denied), of COARSE_LOCATION (not requested: the op's default), a noted
     * COARSE_LOCATION for this package and for one that is not this uid's, then checkPackage for both ("ok" / what it threw); "absent" when getSystemService returns null; else what it threw.
     */
    private String appOps() {
        try {
            android.app.AppOpsManager ops = (android.app.AppOpsManager) getSystemService(APP_OPS_SERVICE);
            if (ops == null) {
                return "absent";
            }
            int uid = android.os.Process.myUid();
            String pkg = getPackageName();
            String own;
            try {
                ops.checkPackage(uid, pkg);
                own = "ok";
            } catch (SecurityException e) {
                own = "SecurityException";
            }
            String foreign;
            try {
                ops.checkPackage(uid, "android");
                foreign = "ok";
            } catch (SecurityException e) {
                foreign = "SecurityException";
            }
            return "post=" + ops.unsafeCheckOpNoThrow("android:post_notification", uid, pkg)
                    + " coarse=" + ops.unsafeCheckOpNoThrow(android.app.AppOpsManager.OPSTR_COARSE_LOCATION, uid, pkg)
                    + " noted=" + ops.noteOpNoThrow(android.app.AppOpsManager.OPSTR_COARSE_LOCATION, uid, pkg)
                    + " noted-foreign=" + ops.noteOpNoThrow(android.app.AppOpsManager.OPSTR_COARSE_LOCATION, uid, "android")
                    + " own=" + own + " foreign=" + foreign;
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /** What PendingIntent reads back through IActivityManager.getInfoForIntentSender (the Facebook ads SDK's getCreatorPackage path): an immutable and a mutable broadcast of this package. */
    private String pendingIntent() {
        try {
            Intent target = new Intent("atl.test.PI").setPackage(getPackageName());
            android.app.PendingIntent fixed = android.app.PendingIntent.getBroadcast(this, 1, target, android.app.PendingIntent.FLAG_IMMUTABLE);
            android.app.PendingIntent open = android.app.PendingIntent.getBroadcast(this, 2, target, android.app.PendingIntent.FLAG_MUTABLE);
            return "package=" + fixed.getCreatorPackage() + " self=" + (fixed.getCreatorUid() == android.os.Process.myUid())
                    + " immutable=" + fixed.isImmutable() + " broadcast=" + fixed.isBroadcast() + " mutable=" + !open.isImmutable();
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /**
     * The features line (app.rs FEATURES): "landscape=<bool> touchscreen=<bool> aep=<bool> gl-aep=<bool|threw ...> listed=<bool> gles=<hex> config=<hex>": hasSystemFeature for an always-present,
     * an absent and a probed feature; what this process's own ES 3 context lists (gl-aep); whether getSystemAvailableFeatures holds the always-present one, its GL ES entry (the one without a name)
     * and the configuration's reqGlEsVersion; else what it threw.
     */
    private String features() {
        try {
            android.content.pm.PackageManager pm = getPackageManager();
            String landscape = android.content.pm.PackageManager.FEATURE_SCREEN_LANDSCAPE;
            boolean listed = false;
            int gles = -1;
            for (android.content.pm.FeatureInfo f : pm.getSystemAvailableFeatures()) {
                if (f.name == null) {
                    gles = f.reqGlEsVersion;
                } else if (f.name.equals(landscape)) {
                    listed = true;
                }
            }
            android.app.ActivityManager am = (android.app.ActivityManager) getSystemService(ACTIVITY_SERVICE);
            String glAep;
            try {
                glAep = String.valueOf(glHasAep());
            } catch (Throwable t) {
                glAep = "threw " + t;
            }
            return "landscape=" + pm.hasSystemFeature(landscape)
                    + " touchscreen=" + pm.hasSystemFeature(android.content.pm.PackageManager.FEATURE_TOUCHSCREEN)
                    + " aep=" + pm.hasSystemFeature(android.content.pm.PackageManager.FEATURE_OPENGLES_EXTENSION_PACK)
                    + " gl-aep=" + glAep + " listed=" + listed + " gles=0x" + Integer.toHexString(gles)
                    + " config=0x" + Integer.toHexString(am.getDeviceConfigurationInfo().reqGlEsVersion);
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    /** The extension whose presence a device declares as FEATURE_OPENGLES_EXTENSION_PACK (CDD 7.1.4.1). */
    private static final String AEP_EXTENSION = "GL_ANDROID_extension_pack_es31a";

    /** Whether an ES 3 context made here (1x1 pbuffer, as a game makes its first context) lists AEP_EXTENSION in GL_EXTENSIONS. */
    private static boolean glHasAep() {
        android.opengl.EGLDisplay d = android.opengl.EGL14.eglGetDisplay(android.opengl.EGL14.EGL_DEFAULT_DISPLAY);
        int[] version = new int[2];
        if (!android.opengl.EGL14.eglInitialize(d, version, 0, version, 1)) {
            throw new IllegalStateException("eglInitialize " + android.opengl.EGL14.eglGetError());
        }
        int[] attribs = {android.opengl.EGL14.EGL_RENDERABLE_TYPE, android.opengl.EGLExt.EGL_OPENGL_ES3_BIT_KHR,
                android.opengl.EGL14.EGL_SURFACE_TYPE, android.opengl.EGL14.EGL_PBUFFER_BIT, android.opengl.EGL14.EGL_NONE};
        android.opengl.EGLConfig[] config = new android.opengl.EGLConfig[1];
        int[] n = new int[1];
        if (!android.opengl.EGL14.eglChooseConfig(d, attribs, 0, config, 0, 1, n, 0) || n[0] == 0) {
            throw new IllegalStateException("no ES 3 pbuffer config");
        }
        android.opengl.EGLContext ctx = android.opengl.EGL14.eglCreateContext(d, config[0], android.opengl.EGL14.EGL_NO_CONTEXT,
                new int[] {android.opengl.EGL14.EGL_CONTEXT_CLIENT_VERSION, 3, android.opengl.EGL14.EGL_NONE}, 0);
        android.opengl.EGLSurface s = android.opengl.EGL14.eglCreatePbufferSurface(d, config[0],
                new int[] {android.opengl.EGL14.EGL_WIDTH, 1, android.opengl.EGL14.EGL_HEIGHT, 1, android.opengl.EGL14.EGL_NONE}, 0);
        if (ctx == android.opengl.EGL14.EGL_NO_CONTEXT || s == android.opengl.EGL14.EGL_NO_SURFACE
                || !android.opengl.EGL14.eglMakeCurrent(d, s, s, ctx)) {
            throw new IllegalStateException("ES 3 context " + android.opengl.EGL14.eglGetError());
        }
        try {
            String ext = android.opengl.GLES20.glGetString(android.opengl.GLES20.GL_EXTENSIONS);
            return ext != null && java.util.Arrays.asList(ext.split(" ")).contains(AEP_EXTENSION);
        } finally {
            android.opengl.EGL14.eglMakeCurrent(d, android.opengl.EGL14.EGL_NO_SURFACE, android.opengl.EGL14.EGL_NO_SURFACE, android.opengl.EGL14.EGL_NO_CONTEXT);
            android.opengl.EGL14.eglDestroySurface(d, s);
            android.opengl.EGL14.eglDestroyContext(d, ctx);
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

    /**
     * "signatures=<n> sig0=<sha256> signers=<n> signer0=<sha256> history=<bool> check=<int>": this package's GET_SIGNATURES array and GET_SIGNING_CERTIFICATES SigningInfo (the Facebook SDK's
     * getKeyHash path), each certificate's SHA-256 in lower hex, and checkSignatures against itself; `null` for a missing array or info; else what it threw.
     */
    private String signatures() {
        try {
            android.content.pm.PackageManager pm = getPackageManager();
            android.content.pm.PackageInfo old = pm.getPackageInfo(getPackageName(), android.content.pm.PackageManager.GET_SIGNATURES);
            android.content.pm.PackageInfo now = pm.getPackageInfo(getPackageName(), android.content.pm.PackageManager.GET_SIGNING_CERTIFICATES);
            StringBuilder out = new StringBuilder();
            if (old.signatures == null) {
                out.append("signatures=null");
            } else {
                out.append("signatures=").append(old.signatures.length).append(" sig0=").append(sha256(old.signatures[0].toByteArray()));
            }
            if (now.signingInfo == null) {
                out.append(" signers=null");
            } else {
                android.content.pm.Signature[] s = now.signingInfo.getApkContentsSigners();
                out.append(" signers=").append(s.length).append(" signer0=").append(sha256(s[0].toByteArray()))
                        .append(" history=").append(now.signingInfo.hasPastSigningCertificates());
            }
            out.append(" check=").append(pm.checkSignatures(getPackageName(), getPackageName()));
            return out.toString();
        } catch (Throwable t) {
            return "threw " + t;
        }
    }

    private static String sha256(byte[] bytes) throws java.security.NoSuchAlgorithmException {
        StringBuilder hex = new StringBuilder();
        for (byte b : java.security.MessageDigest.getInstance("SHA-256").digest(bytes)) {
            hex.append(String.format("%02x", b & 0xff));
        }
        return hex.toString();
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

    /** /proc/cpuinfo as this process's Java reads it (the cpuinfoView row); what it threw instead, so the row shows the reason. */
    private static String procCpuinfo() {
        try (java.io.FileInputStream in = new java.io.FileInputStream("/proc/cpuinfo")) {
            java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            for (int n; (n = in.read(buf)) > 0; ) {
                out.write(buf, 0, n);
            }
            return new String(out.toByteArray(), java.nio.charset.StandardCharsets.UTF_8);
        } catch (java.io.IOException e) {
            return "threw " + e;
        }
    }

    /** The bridge-1 fixture class once loaded (onActivityResult makes the looperLater call with it); null without it. */
    private Class<?> bridge1;

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
        bridge1Call(jni, "windowProbe", "windowProbe", probeSurface());
        bridge1Call(jni, "mediandkLinked", "mediandkLinked");
        bridge1Call(jni, "resolverProbe", "resolverProbe");
        bridge1Call(jni, "cpuProbe", "cpuProbe", Runtime.getRuntime().availableProcessors());
        bridge1Call(jni, "cpuinfoView", "cpuinfoView", procCpuinfo());
        bridge1Call(jni, "keyDtorDetachReturn", "keyDtorDetach", 0);
        bridge1Call(jni, "keyDtorDetachExit", "keyDtorDetach", 1);
        bridge1Call(jni, "keyDtorAttach", "keyDtorDetach", 2);
        // tr-first blocker 5: (a) on this thread inside the call; then a fresh thread arms the main looper for (b), read in onActivityResult
        bridge1Call(jni, "looperNow", "looperNow");
        Thread arm = new Thread(new Runnable() {
            @Override
            public void run() {
                bridge1Call(jni, "looperArm", "looperArm");
            }
        });
        arm.start();
        try {
            arm.join();
        } catch (InterruptedException e) {
            Log.i("AtlTest", "bridge1 call looperArm = threw " + e);
        }
    }

    /** windowProbe's Surface: a SurfaceTexture's producer with default buffer size WINDOW_W x WINDOW_H (app.rs BRIDGE1_CALLS windowProbe). */
    private static final int WINDOW_W = 48;
    private static final int WINDOW_H = 32;

    private static android.view.Surface probeSurface() {
        android.graphics.SurfaceTexture st = new android.graphics.SurfaceTexture(0);
        st.setDefaultBufferSize(WINDOW_W, WINDOW_H);
        return new android.view.Surface(st);
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
