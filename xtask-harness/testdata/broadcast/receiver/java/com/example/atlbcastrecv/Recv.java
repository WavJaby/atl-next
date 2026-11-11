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

    @Override
    public void onReceive(Context c, Intent i) {
        Log.i(TAG, "manifest received " + i.getAction());
        try {
            c.sendStickyBroadcast(new Intent("com.example.atlbcastrecv.MINE"));
            Log.i(TAG, "sticky allowed");
        } catch (SecurityException e) {
            Log.i(TAG, "sticky denied: " + e.getMessage());
        }
        c.sendBroadcast(new Intent("com.example.atlbcast.PING"));
        Log.i(TAG, "foreign ping sent");
    }
}
