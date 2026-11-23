// SPDX-License-Identifier: MIT
package com.example.atlbcastrecv;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

/**
 * `xtask test broadcast`: started by the alarm chain (Recv.java startOrLog). Logs its start and stops itself, so the receiver process ends after its grace.
 */
public class AlarmSvc extends Service {
    @Override
    public IBinder onBind(Intent i) {
        return null;
    }

    @Override
    public int onStartCommand(Intent i, int flags, int startId) {
        Log.i(Recv.TAG, "alarm service started");
        stopSelf(startId);
        return START_NOT_STICKY;
    }
}
