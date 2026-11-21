// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

/** Main.componentState's receiver: each explicit broadcast that reaches it logs its `n` (app.rs LOG_LINES componentstate-receive); the one sent while it is disabled must not. */
public class AtlStateReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        Log.i("AtlTest", "componentstate-receive: n=" + intent.getIntExtra("n", 0));
    }
}
