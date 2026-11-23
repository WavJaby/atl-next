// SPDX-License-Identifier: MIT
package com.example.atlbcastrecv;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

/** Started by BootRecv inside the BOOT_COMPLETED temp allowlist; it stops itself at once, so the headless process ends after its grace. */
public class BootSvc extends Service {
    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        Log.i(Recv.TAG, "boot service started");
        stopSelf(startId);
        return START_NOT_STICKY;
    }
}
