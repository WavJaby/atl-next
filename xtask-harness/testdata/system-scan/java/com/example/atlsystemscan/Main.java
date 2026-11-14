// SPDX-License-Identifier: MIT
package com.example.atlsystemscan;

import android.app.Activity;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.os.Bundle;
import android.system.ErrnoException;
import android.system.Os;
import android.system.OsConstants;
import android.util.Log;
import java.io.FileDescriptor;
import java.io.FileInputStream;
import java.io.IOException;

/** `xtask test system-scan` (xtask-harness/src/system_scan.rs reads the AtlSystemScan lines): its own ApplicationInfo through PackageManager, a read of its sourceDir, and a write there. */
public class Main extends Activity {
    private static final String TAG = "AtlSystemScan";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        try {
            ApplicationInfo ai = getPackageManager().getPackageInfo(getPackageName(), 0).applicationInfo;
            Log.i(TAG, "system " + ((ai.flags & ApplicationInfo.FLAG_SYSTEM) != 0) + " source " + ai.sourceDir);
            try (FileInputStream in = new FileInputStream(ai.sourceDir)) {
                byte[] magic = new byte[2];
                int n = in.read(magic);
                Log.i(TAG, "read " + (n == 2 && magic[0] == 'P' && magic[1] == 'K' ? "zip" : "other " + n));
            } catch (IOException e) {
                Log.i(TAG, "read failed " + e);
            }
            try {
                FileDescriptor fd = Os.open(ai.sourceDir, OsConstants.O_WRONLY | OsConstants.O_APPEND, 0);
                Os.close(fd);
                Log.i(TAG, "write opened");
            } catch (ErrnoException e) {
                Log.i(TAG, "write " + (e.errno == OsConstants.EROFS ? "EROFS" : OsConstants.errnoName(e.errno)));
            }
        } catch (PackageManager.NameNotFoundException e) {
            Log.i(TAG, "lookup failed " + e);
        }
        Log.i(TAG, "done");
        finish(); // Theme.NoDisplay requires it before onResume
    }
}
