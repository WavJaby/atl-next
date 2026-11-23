// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

/**
 * Main.stickyStart's START_STICKY service (app.rs STICKY case), in its own process `:sticky`: its log is `log/<pkg>:sticky.log`, which the gate reads.
 * Each start logs its startId and action; the restart after its process is ended has startId 2 and a null action (the null Intent of a sticky restart).
 */
public class AtlSticky extends Service {
    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? "null" : intent.getAction();
        Log.i("AtlTest", "sticky: onStartCommand startId=" + startId + " action=" + action);
        return START_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
