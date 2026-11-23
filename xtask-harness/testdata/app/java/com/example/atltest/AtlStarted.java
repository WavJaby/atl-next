// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

/**
 * Main.started's service (app.rs LOG_LINES startservice, stopservice, stopself), in the main process so its lines reach the app log the gate reads.
 * The chain runs inside the service: START logs its onStartCommand, then stopService stops it (its onDestroy starts STOPSELF);
 * STOPSELF stops itself with stopSelf(startId) and its onDestroy logs the stopself row.
 */
public class AtlStarted extends Service {
    static final String ACTION_START = "atl.test.START";
    static final String ACTION_STOPSELF = "atl.test.STOPSELF";

    /** The action this instance was started with. */
    private String action;
    /** The startId stopSelf was called with. */
    private int stoppedSelfId;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        action = intent.getAction();
        if (ACTION_START.equals(action)) {
            Log.i("AtlTest", "startservice: onStartCommand startId=" + startId + " action=" + action);
            stopService(new Intent(this, AtlStarted.class));
        } else if (ACTION_STOPSELF.equals(action)) {
            stoppedSelfId = startId;
            stopSelf(startId);
        }
        return START_NOT_STICKY;
    }

    @Override
    public void onDestroy() {
        if (ACTION_START.equals(action)) {
            Log.i("AtlTest", "stopservice: onDestroy");
            startService(new Intent(ACTION_STOPSELF).setClass(this, AtlStarted.class));
        } else if (ACTION_STOPSELF.equals(action)) {
            Log.i("AtlTest", "stopself: onDestroy after stopSelf startId=" + stoppedSelfId);
        }
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
