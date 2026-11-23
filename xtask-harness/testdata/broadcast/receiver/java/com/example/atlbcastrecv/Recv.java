// SPDX-License-Identifier: MIT
package com.example.atlbcastrecv;

import android.app.AlarmManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

/**
 * `xtask test broadcast`: reached by an explicit broadcast, in a process started for it. A sticky send without BROADCAST_STICKY must throw SecurityException; then an implicit PING that the
 * sender's not-exported receiver must not get; then the alarm chain (ALARM_CONTROL, ALARM_GRANT).
 */
public class Recv extends BroadcastReceiver {
    static final String TAG = "AtlBcast";

    static final String ORDER = "com.example.atlbcast.ORDER";
    /**
     * The alarm chain: both alarms are set by this app for itself. The inexact one's start has no grant and is refused; the exact one is set only after that refusal and grants its start.
     */
    static final String ALARM_CONTROL = "com.example.atlbcast.ALARM_CONTROL";
    static final String ALARM_GRANT = "com.example.atlbcast.ALARM_GRANT";
    /**
     * The inexact alarm's delay, from the EXPLICIT receiver. Later than the gate's empty grace (broadcast.rs EMPTY_GRACE_MS, 2 s): that process has ended by then, so the chain starts a new
     * one. The grant's uid stays active for 10 s and then settles for AOSP's 60 s, and a headless process ends only once its uid is idle (activity.rs end_headless), so a grant made first
     * would hold the explicit process open for 70 s.
     */
    static final long CONTROL_DELAY_MS = 5000;
    /** The exact alarm's delay, from the refused control receiver. The gate waits for the chain's last line, so this only sets the pace. */
    static final long GRANT_DELAY_MS = 1000;

    @Override
    public void onReceive(Context c, Intent i) {
        String action = i.getAction();
        Log.i(TAG, "manifest received " + action);
        // the ordered broadcast of the sender: it sees the result the sender's receivers left, and its own result (7) is the final one
        if (ORDER.equals(action)) {
            Log.i(TAG, "order manifest code=" + getResultCode() + " data=" + getResultData());
            setResultCode(7);
            return;
        }
        if (ALARM_CONTROL.equals(action)) {
            startOrLog(c, "control");
            setAlarm(c, true, GRANT_DELAY_MS, ALARM_GRANT);
            return;
        }
        if (ALARM_GRANT.equals(action)) {
            startOrLog(c, "grant");
            return;
        }
        // ABORT and IMPLICIT reach here only if the dispatcher delivered them (the gate fails on that line)
        if (!"com.example.atlbcast.EXPLICIT".equals(action)) {
            return;
        }
        try {
            c.sendStickyBroadcast(new Intent("com.example.atlbcastrecv.MINE"));
            Log.i(TAG, "sticky allowed");
        } catch (SecurityException e) {
            Log.i(TAG, "sticky denied: " + e.getMessage());
        }
        c.sendBroadcast(new Intent("com.example.atlbcast.PING"));
        Log.i(TAG, "foreign ping sent");
        // the sender keeps its process (and the not-exported receiver) until this arrives
        c.sendBroadcast(new Intent("com.example.atlbcast.DONE").setPackage("com.example.atlbcast"));
        // the chain's first alarm is inexact: AlarmManagerService :2751, :2762 gives it no idleOptions (neither exact nor allow-while-idle), so its start gets no temp allowlist
        setAlarm(c, false, CONTROL_DELAY_MS, ALARM_CONTROL);
    }

    /** startService of AlarmSvc. A refusal is logged with its message (ContextImpl :2049, ActiveServices :1112 `app is in background uid`); a start as `ok`. */
    static void startOrLog(Context c, String which) {
        try {
            c.startService(new Intent(c, AlarmSvc.class));
            Log.i(TAG, which + " start ok");
        } catch (IllegalStateException e) {
            Log.i(TAG, which + " start refused: " + e.getMessage());
        }
    }

    /** A broadcast PendingIntent to this receiver for `action` (immutable: targetSdk 31+ needs a mutability flag), sent by an exact or an inexact RTC_WAKEUP alarm. */
    static void setAlarm(Context c, boolean exact, long delayMs, String action) {
        PendingIntent pi = PendingIntent.getBroadcast(c, 0,
                new Intent(action).setComponent(new ComponentName(c, Recv.class)), PendingIntent.FLAG_IMMUTABLE);
        AlarmManager am = (AlarmManager) c.getSystemService(Context.ALARM_SERVICE);
        long at = System.currentTimeMillis() + delayMs;
        if (exact) {
            am.setExact(AlarmManager.RTC_WAKEUP, at, pi);
        } else {
            am.set(AlarmManager.RTC_WAKEUP, at, pi);
        }
        Log.i(TAG, (exact ? "exact" : "inexact") + " alarm set " + action);
    }
}
