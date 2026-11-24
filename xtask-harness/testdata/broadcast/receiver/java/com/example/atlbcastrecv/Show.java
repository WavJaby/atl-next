// SPDX-License-Identifier: MIT
package com.example.atlbcastrecv;

import android.app.Activity;
import android.content.pm.PackageManager;
import android.os.Bundle;
import android.util.Log;

/** The downgrade row's launch (broadcast.rs run 3): says which installed versionCode runs, then finishes, so atld ends with its last app. */
public class Show extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        long version;
        try {
            version = getPackageManager().getPackageInfo(getPackageName(), 0).getLongVersionCode();
        } catch (PackageManager.NameNotFoundException e) {
            version = -1;
        }
        Log.i(Recv.TAG, "activity onCreate versionCode=" + version);
        finish();
    }
}
