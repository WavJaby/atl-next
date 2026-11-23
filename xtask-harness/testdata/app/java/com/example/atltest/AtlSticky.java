// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.ActivityManager;
import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.os.Process;
import android.util.Log;
import java.util.List;

/**
 * Main.stickyStart's START_STICKY service (app.rs STICKY case), in its own process `:sticky`: its log is `log/<pkg>:sticky.log`, which the gate reads.
 * Each start logs its startId and action; the restart after its process is ended has startId 2 and a null action (the null Intent of a sticky restart).
 * The first start logs this process's importance in getRunningAppProcesses before its first-start line (S11: a started service's process is IMPORTANCE_SERVICE, 300).
 */
public class AtlSticky extends Service {
    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? "null" : intent.getAction();
        if (startId == 1) {
            Log.i("AtlTest", "importance: sticky=" + importanceOfMyProcess());
        }
        Log.i("AtlTest", "sticky: onStartCommand startId=" + startId + " action=" + action);
        return START_STICKY;
    }

    /** This process's importance in getRunningAppProcesses, -1 when it is not listed. */
    private int importanceOfMyProcess() {
        List<ActivityManager.RunningAppProcessInfo> list =
                ((ActivityManager) getSystemService(ACTIVITY_SERVICE)).getRunningAppProcesses();
        if (list != null) {
            for (ActivityManager.RunningAppProcessInfo info : list) {
                if (info.pid == Process.myPid()) {
                    return info.importance;
                }
            }
        }
        return -1;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
