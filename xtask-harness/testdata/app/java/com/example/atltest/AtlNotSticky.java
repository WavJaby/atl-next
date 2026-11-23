// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

/**
 * Main.stickyStart's START_NOT_STICKY service (app.rs STICKY case), in its own process `:notsticky`: its log is `log/<pkg>:notsticky.log`.
 * Its process's end must not restart it (ActiveServices.killServicesLocked: canStopIfKilled), so the gate watches for a new start of that process.
 */
public class AtlNotSticky extends Service {
    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? "null" : intent.getAction();
        Log.i("AtlTest", "notsticky: onStartCommand startId=" + startId + " action=" + action);
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
