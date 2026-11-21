// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** Main.componentState's manifest-disabled receiver (`android:enabled="false"`): resolvable only while setComponentEnabledSetting holds it ENABLED. Never sent a broadcast. */
public class AtlOffReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
    }
}
