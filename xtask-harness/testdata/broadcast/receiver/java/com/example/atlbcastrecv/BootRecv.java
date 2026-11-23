// SPDX-License-Identifier: MIT
package com.example.atlbcastrecv;

import android.content.BroadcastReceiver;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

/**
 * The S9 boot rows (xtask-harness/src/broadcast.rs): this package is opted in (atld --autostart), so LOCKED_BOOT_COMPLETED then BOOT_COMPLETED reach it at services start,
 * headless; BOOT_COMPLETED's receiver starts BootSvc inside the 20 s temp allowlist; MY_PACKAGE_REPLACED reaches it on an upgrade.
 */
public class BootRecv extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        String action = intent.getAction();
        Log.i(Recv.TAG, "boot received " + action);
        if (Intent.ACTION_BOOT_COMPLETED.equals(action)) {
            try {
                ComponentName started = context.startService(new Intent(context, BootSvc.class));
                Log.i(Recv.TAG, "boot service start " + (started != null ? "returned" : "null"));
            } catch (RuntimeException e) {
                Log.i(Recv.TAG, "boot service refused: " + e.getMessage());
            }
        }
    }
}
