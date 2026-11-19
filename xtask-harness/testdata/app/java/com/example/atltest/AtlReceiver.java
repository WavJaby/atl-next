// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** The fixture's one declared `<receiver>`: Main.receiverInfo looks it up with PackageManager.getReceiverInfo (app.rs SERVICE_LINES receiver). Never sent a broadcast. */
public class AtlReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
    }
}
