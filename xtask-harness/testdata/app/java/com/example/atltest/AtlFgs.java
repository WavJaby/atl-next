// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.IBinder;
import android.util.Log;

/**
 * Main.foregroundStarts' dataSync foreground service (app.rs FGS row `fgs`), in its own process `:fgs`: its log is `log/<pkg>:fgs.log`, which the gate reads.
 * Calls startForeground with the type its manifest declares (dataSync) and logs the result; the gate kills the process once the services have the start's result, and the foreground state must end with it.
 * START_NOT_STICKY: the death drops the record (ActiveServices.killServicesLocked, canStopIfKilled), so the foreground state ends there, not in a restart.
 */
public class AtlFgs extends Service {
    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        NotificationManager nm = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
        nm.createNotificationChannel(new NotificationChannel("atl.fgs", "fgs", NotificationManager.IMPORTANCE_LOW));
        Notification n = new Notification.Builder(this, "atl.fgs")
                .setSmallIcon(android.R.drawable.ic_dialog_info)
                .setContentTitle("ATL test")
                .setContentText("fgs")
                .build();
        try {
            startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC);
            Log.i("AtlTest", "fgs: startForeground ok");
        } catch (RuntimeException e) {
            Log.i("AtlTest", "fgs: startForeground threw " + e);
        }
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
