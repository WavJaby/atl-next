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
    /** How long onCreate waits for DONE: under the gate's 180 s LIMIT; a cold start of the receiver app takes ~1 s. */
    static final long DONE_WAIT_S = 60;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // the application's context: the receivers are the process's, not this activity's (an activity's own registrations are dropped as leaked when it finishes)
        Context app = getApplicationContext();
        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                Log.i(TAG, "runtime received " + i.getAction());
            }
        }, new IntentFilter(PING), Context.RECEIVER_NOT_EXPORTED);
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

        // DONE on a thread of its own: the main thread waits for it below
        CountDownLatch done = new CountDownLatch(1);
        HandlerThread doneThread = new HandlerThread("done");
        doneThread.start();
        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                done.countDown();
            }
        }, new IntentFilter(DONE), null, new Handler(doneThread.getLooper()), Context.RECEIVER_EXPORTED);

        sendBroadcast(new Intent(EXPLICIT).setComponent(new ComponentName("com.example.atlbcastrecv", "com.example.atlbcastrecv.Recv")));
        protectedBroadcasts(app);
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
}
