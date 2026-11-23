// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

/**
 * Main.foregroundStarts' service that never calls startForeground (app.rs FGS row `fgstimeout`), in its own process `:fgstimeout`: its log is `log/<pkg>:fgstimeout.log`.
 * The start arms ActiveServices' fgRequired timer (30 s, ActivityManagerConstants.java:318); its stop is onDestroy here. The headless process then ends at the ANR, 10 s later (:320, ActiveServices :7640).
 */
public class AtlFgsSilent extends Service {
    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        Log.i("AtlTest", "fgstimeout: onStartCommand startId=" + startId);
        return START_NOT_STICKY;
    }

    @Override
    public void onDestroy() {
        Log.i("AtlTest", "fgstimeout: onDestroy");
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
