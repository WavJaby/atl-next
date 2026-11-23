// SPDX-License-Identifier: MIT
package com.example.atlnet;

import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.net.ConnectivityManager;
import android.net.Network;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.Log;

/**
 * `xtask test broadcast` connectivity row (xtask-harness/src/broadcast.rs reads the AtlNet lines): a default-network callback on its own thread, and the CONNECTIVITY_ACTION sticky read
 * 500 ms after each callback (the services send the sticky after the callbacks). The harness removes and restores the default route inside atld's network namespace meanwhile.
 * The launched activity holds the process in onCreate, as the sender fixture does: a finished last activity ends its process (window-close rule), and NoDisplay must finish before onResume.
 */
public class Main extends Activity {
    static final String TAG = "AtlNet";
    /** The hold: longer than the harness's route steps (each waits up to 60 s); the harness stops atld when its checks are done. */
    static final long HOLD_MS = 300_000;
    int available;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        HandlerThread thread = new HandlerThread("atlnet");
        thread.start();
        Handler handler = new Handler(thread.getLooper());
        ConnectivityManager cm = (ConnectivityManager) getSystemService(Context.CONNECTIVITY_SERVICE);
        cm.registerDefaultNetworkCallback(new ConnectivityManager.NetworkCallback() {
            @Override
            public void onAvailable(Network network) {
                available++;
                final String when = "after available " + available;
                Log.i(TAG, "AtlNet: available " + available);
                handler.postDelayed(new Runnable() {
                    public void run() {
                        sticky(when);
                    }
                }, 500);
            }

            @Override
            public void onLost(Network network) {
                Log.i(TAG, "AtlNet: lost");
                handler.postDelayed(new Runnable() {
                    public void run() {
                        sticky("after lost");
                    }
                }, 500);
            }
        }, handler);
        try {
            Thread.sleep(HOLD_MS);
        } catch (InterruptedException e) {
            Log.i(TAG, "AtlNet: hold interrupted");
        }
        finish();
    }

    /** registerReceiver(null, CONNECTIVITY_ACTION): the sticky as the system holds it now. */
    void sticky(String when) {
        Intent i = registerReceiver(null, new IntentFilter(ConnectivityManager.CONNECTIVITY_ACTION));
        if (i == null) {
            Log.i(TAG, "AtlNet: sticky " + when + " absent");
            return;
        }
        Log.i(TAG, "AtlNet: sticky " + when + " present noConnectivity="
                + i.getBooleanExtra(ConnectivityManager.EXTRA_NO_CONNECTIVITY, false));
    }
}
