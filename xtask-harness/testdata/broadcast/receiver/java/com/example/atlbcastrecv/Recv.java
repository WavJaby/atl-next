// SPDX-License-Identifier: MIT
package com.example.atlbcastrecv;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

/**
 * `xtask test broadcast`: reached by an explicit broadcast, in a process started for it. A sticky send without BROADCAST_STICKY must throw SecurityException; then an implicit PING that the
 * sender's not-exported receiver must not get.
 */
public class Recv extends BroadcastReceiver {
    static final String TAG = "AtlBcast";

    static final String ORDER = "com.example.atlbcast.ORDER";

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
    }
}
