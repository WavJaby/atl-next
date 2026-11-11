// SPDX-License-Identifier: MIT
package com.example.atlbcast;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.Bundle;
import android.util.Log;

/**
 * `xtask test broadcast` (xtask-harness/src/broadcast.rs reads the AtlBcast lines): a runtime receiver gets a broadcast of its own package; a sticky is stored, returned by registerReceiver(null, ..)
 * and replayed to a new receiver as an initial sticky; an explicit broadcast cold-starts the receiver app (xtask-harness/testdata/broadcast/receiver), whose implicit PING must not reach the not-exported receiver here.
 */
public class Main extends Activity {
    static final String TAG = "AtlBcast";
    static final String PING = "com.example.atlbcast.PING";
    static final String STICKY = "com.example.atlbcast.STICKY";
    static final String EXPLICIT = "com.example.atlbcast.EXPLICIT";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // the application's context: the receivers outlive this window-less activity (an activity's own registrations are dropped as leaked when it finishes)
        Context app = getApplicationContext();
        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                Log.i(TAG, "runtime received " + i.getAction());
            }
        }, new IntentFilter(PING), Context.RECEIVER_NOT_EXPORTED);
        sendBroadcast(new Intent(PING).setPackage(getPackageName()));

        sendStickyBroadcast(new Intent(STICKY).putExtra("level", 42));
        Intent s = app.registerReceiver(null, new IntentFilter(STICKY), Context.RECEIVER_NOT_EXPORTED);
        Log.i(TAG, "sticky returned level=" + (s == null ? "none" : String.valueOf(s.getIntExtra("level", -1))));
        app.registerReceiver(new BroadcastReceiver() {
            @Override
            public void onReceive(Context c, Intent i) {
                Log.i(TAG, "sticky delivered initial=" + isInitialStickyBroadcast() + " level=" + i.getIntExtra("level", -1));
            }
        }, new IntentFilter(STICKY), Context.RECEIVER_EXPORTED);

        sendBroadcast(new Intent(EXPLICIT).setComponent(new ComponentName("com.example.atlbcastrecv", "com.example.atlbcastrecv.Recv")));
        Log.i(TAG, "onCreate done");
        // Theme.NoDisplay requires it before onResume
        finish();
    }
}
