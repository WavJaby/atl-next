// SPDX-License-Identifier: MIT
package com.example.atlbcast;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

/**
 * The S9 negative rows (xtask-harness/src/broadcast.rs): this package is not opted in (no --autostart), so it gets no LOCKED_BOOT_COMPLETED or BOOT_COMPLETED at services start;
 * it gets MY_PACKAGE_REPLACED only when it is upgraded.
 */
public class BootRecv extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        Log.i("AtlBcast", "boot received " + intent.getAction());
    }
}
