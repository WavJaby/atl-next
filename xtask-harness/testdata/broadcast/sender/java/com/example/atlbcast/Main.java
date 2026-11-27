// SPDX-License-Identifier: MIT
package com.example.atlbcast;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * `xtask test broadcast` (xtask-harness/src/broadcast.rs reads the AtlBcast lines): a runtime receiver gets a broadcast of its own package; a sticky is stored, returned by registerReceiver(null, ..)
 * and replayed to a new receiver as an initial sticky; an explicit broadcast cold-starts the receiver app (xtask-harness/testdata/broadcast/receiver), whose implicit PING must not reach the not-exported receiver here.
 */
public class Main extends Activity {
    static final String TAG = "AtlBcast";
    static final String PING = "com.example.atlbcast.PING";
    static final String STICKY = "com.example.atlbcast.STICKY";
    static final String EXPLICIT = "com.example.atlbcast.EXPLICIT";
    /** The receiver app's answer, sent after its PING: the PING's skip is decided by then (both are its synchronous broadcastIntent calls, in order). */
    static final String DONE = "com.example.atlbcast.DONE";
    /** The ordered rules (orderedBroadcasts) and the implicit skip: the receiver app's manifest receiver filters these. */
    static final String ORDER = "com.example.atlbcast.ORDER";
    static final String ABORT = "com.example.atlbcast.ABORT";
    static final String IMPLICIT = "com.example.atlbcast.IMPLICIT";
    /** Intent.FLAG_RECEIVER_INCLUDE_BACKGROUND (Intent.java:7660): a hidden API, absent from the public stubs this fixture compiles against, so its value. */
    static final int INCLUDE_BACKGROUND = 0x0100_0000;
    /** How long onCreate waits for DONE: under the gate's 180 s LIMIT; a cold start of the receiver app takes ~1 s. */
    static final long DONE_WAIT_S = 60;

    /**
     * The INTERNET rows (judged by broadcast.rs judge_internet): this app has no android.permission.INTERNET, so AOSP refuses its AF_INET socket
     * (EPERM at socket(), a loopback ServerSocket included), a name lookup throws SecurityException, and localhost resolves from the hosts file.
     */
    static void internetRows() {
        try {
            new java.net.ServerSocket(0, 16, java.net.InetAddress.getByName("127.0.0.1")).close();
            Log.i("AtlBcast", "net server-socket=ok");
        } catch (java.io.IOException e) {
            Log.i("AtlBcast", "net server-socket=" + e);
        }
        try {
            java.net.InetAddress.getByName("atl-nonet.invalid");
            Log.i("AtlBcast", "net dns=ok");
        } catch (SecurityException e) {
            Log.i("AtlBcast", "net dns=SecurityException: " + e.getMessage());
        } catch (java.net.UnknownHostException e) {
            Log.i("AtlBcast", "net dns=UnknownHostException");
        }
        try {
            Log.i("AtlBcast", "net localhost=" + java.net.InetAddress.getByName("localhost").getHostAddress());
        } catch (java.net.UnknownHostException e) {
            Log.i("AtlBcast", "net localhost=UnknownHostException");
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // off the main thread: libcore throws NetworkOnMainThreadException for a socket or lookup on it
        Thread net = new Thread(new Runnable() {
            @Override
            public void run() {
                internetRows();
            }
        }, "atl-net");
        net.start();
        try {
            net.join();
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        // the application's context: the receivers are the process's, not this activity's (an activity's own registrations are dropped as leaked when it finishes)
        Context app = getApplicationContext();
        // DONE on a thread of its own: the main thread waits for it below. The PING receiver shares that looper, so DONE is delivered FIFO behind any PING (broadcast_queue.rs:4-6)
        CountDownLatch done = new CountDownLatch(1);
        HandlerThread doneThread = new HandlerThread("done");
        doneThread.start();
        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                Log.i(TAG, "runtime received " + i.getAction());
            }
        }, new IntentFilter(PING), null, new Handler(doneThread.getLooper()), Context.RECEIVER_NOT_EXPORTED);
        sendBroadcast(new Intent(PING).setPackage(getPackageName()));

        sendStickyBroadcast(new Intent(STICKY).putExtra("level", 42));
        Intent s = app.registerReceiver(null, new IntentFilter(STICKY), Context.RECEIVER_NOT_EXPORTED);
        Log.i(TAG, "sticky returned level=" + (s == null ? "none" : String.valueOf(s.getIntExtra("level", -1))));
        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                Log.i(TAG, "sticky delivered initial=" + isInitialStickyBroadcast() + " level=" + i.getIntExtra("level", -1));
            }
        }, new IntentFilter(STICKY), Context.RECEIVER_EXPORTED);

        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                Log.i(TAG, "done received");
                done.countDown();
            }
        }, new IntentFilter(DONE), null, new Handler(doneThread.getLooper()), Context.RECEIVER_EXPORTED);

        sendBroadcast(new Intent(EXPLICIT).setComponent(new ComponentName("com.example.atlbcastrecv", "com.example.atlbcastrecv.Recv")));
        protectedBroadcasts(app);
        // on the DONE thread, not the main looper: the main thread waits for DONE below, and an ordered delivery in flight would hold DONE back in this process's queue
        orderedBroadcasts(app, new Handler(doneThread.getLooper()));
        Log.i(TAG, "onCreate done");
        // the not-exported receiver must still be registered when the PING comes: since 3be29e97 a finished last activity ends this process (window-close-esc plan, point 4), and Theme.NoDisplay
        // needs finish() before onResume, so the wait is here
        try {
            Log.i(TAG, "done " + done.await(DONE_WAIT_S, TimeUnit.SECONDS));
        } catch (InterruptedException e) {
            Log.i(TAG, "done interrupted");
        }
        finish();
    }

    /**
     * The protected-broadcast sender rules (BroadcastController.broadcastIntentLocked:1031-1093; core/res AndroidManifest.xml:69 BATTERY_CHANGED, :71 BATTERY_LOW): this app is not the system, so a
     * protected action is a SecurityException, sticky or not (it holds BROADCAST_STICKY: the protected check comes first); its fake BATTERY_CHANGED stores nothing, so registerReceiver(null, ..) answers
     * the system's own (the battery service's sticky, its level the host's). Registering for only protected actions needs no export flag at targetSdk 36 (:470-472); an unprotected action without one throws (:457-460).
     */
    void protectedBroadcasts(Context app) {
        try {
            sendBroadcast(new Intent(Intent.ACTION_BATTERY_LOW));
            Log.i(TAG, "protected send: delivered");
        } catch (SecurityException e) {
            Log.i(TAG, "protected send denied: " + e.getMessage());
        }
        try {
            sendStickyBroadcast(new Intent(Intent.ACTION_BATTERY_CHANGED).putExtra("level", 1));
            Log.i(TAG, "protected sticky: stored");
        } catch (SecurityException e) {
            Log.i(TAG, "protected sticky denied: " + e.getMessage());
        }
        Intent battery = app.registerReceiver(null, new IntentFilter(Intent.ACTION_BATTERY_CHANGED));
        Log.i(TAG, "battery sticky level=" + (battery == null ? "none" : String.valueOf(battery.getIntExtra("level", -1))));
        BroadcastReceiver ignore = new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {}
        };
        try {
            app.registerReceiver(ignore, new IntentFilter(Intent.ACTION_BATTERY_LOW));
            Log.i(TAG, "protected-only register without flag: ok");
        } catch (SecurityException e) {
            Log.i(TAG, "protected-only register without flag denied: " + e.getMessage());
        }
        try {
            app.registerReceiver(ignore, new IntentFilter(PING + ".UNFLAGGED"));
            Log.i(TAG, "unflagged register: ok");
        } catch (SecurityException e) {
            Log.i(TAG, "unflagged register denied: " + e.getMessage());
        }
        app.unregisterReceiver(ignore);
    }

    /**
     * The ordered rules (BroadcastController :1520-1545, BroadcastRecord :786-795, BroadcastQueueImpl :1381-1412) and the implicit skip. ORDER: a priority-10 receiver sets code 5 and data
     * "first", the priority-0 one sees it, the receiver app's manifest receiver (INCLUDE_BACKGROUND, so its restricted uid gets it) sees it and sets 7, and resultTo gets the final code.
     * ABORT: the priority-10 receiver aborts, so the rest is skipped and resultTo still gets the result. IMPLICIT, sent without INCLUDE_BACKGROUND, is skipped for the receiver
     * app's restricted uid (BroadcastSkipPolicy :229-246): the gate reads its counter.
     */
    void orderedBroadcasts(Context app, Handler scheduler) {
        BroadcastReceiver high = new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                String which = ORDER.equals(i.getAction()) ? "order" : "abort";
                Log.i(TAG, which + " first code=" + getResultCode() + " data=" + getResultData());
                if (ORDER.equals(i.getAction())) {
                    setResultCode(5);
                    setResultData("first");
                } else {
                    abortBroadcast();
                }
            }
        };
        BroadcastReceiver low = new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                String which = ORDER.equals(i.getAction()) ? "order" : "abort";
                Log.i(TAG, which + " second code=" + getResultCode() + " data=" + getResultData());
            }
        };
        IntentFilter highFilter = new IntentFilter(ORDER);
        highFilter.addAction(ABORT);
        highFilter.setPriority(10);
        IntentFilter lowFilter = new IntentFilter(ORDER);
        lowFilter.addAction(ABORT);
        app.registerReceiver(high, highFilter, null, scheduler, Context.RECEIVER_NOT_EXPORTED);
        app.registerReceiver(low, lowFilter, null, scheduler, Context.RECEIVER_NOT_EXPORTED);
        BroadcastReceiver resultTo = new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                String which = ORDER.equals(i.getAction()) ? "order" : "abort";
                Log.i(TAG, which + " resultTo code=" + getResultCode() + " data=" + getResultData());
            }
        };
        app.sendOrderedBroadcast(new Intent(ORDER).addFlags(INCLUDE_BACKGROUND), null, resultTo, scheduler, 0, null, null);
        app.sendOrderedBroadcast(new Intent(ABORT).addFlags(INCLUDE_BACKGROUND), null, resultTo, scheduler, 0, null, null);
        app.sendBroadcast(new Intent(IMPLICIT));
    }
}
