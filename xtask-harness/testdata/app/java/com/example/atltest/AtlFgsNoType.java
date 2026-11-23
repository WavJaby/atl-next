// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Notification;
import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

/**
 * Main.foregroundStarts' service with no foregroundServiceType in its manifest entry (app.rs LOG_LINES `fgsnotype`), in the main process: its log is `log/<pkg>.log`.
 * At targetSdk 36 startForeground without a type throws (ActiveServices.java:2883-2886, the message's start); the row reads the message.
 */
public class AtlFgsNoType extends Service {
    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        Notification n = new Notification.Builder(this, "atl.fgs")
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentTitle("ATL test")
                .build();
        try {
            startForeground(1, n);
            Log.i("AtlTest", "fgsnotype: startForeground returned");
        } catch (RuntimeException e) {
            Log.i("AtlTest", "fgsnotype: " + e.getMessage());
        }
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
