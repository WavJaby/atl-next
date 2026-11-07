// SPDX-License-Identifier: MIT
package com.example.atlprovider;

import android.app.Activity;
import android.content.ComponentName;
import android.content.pm.PackageManager;
import android.content.pm.ProviderInfo;
import android.net.Uri;
import android.os.Bundle;
import android.util.Log;

/** `xtask test provider` (xtask/src/provider.rs reads the AtlProvider lines): the PackageManager lookups androidx.startup and FileProvider make, and a call to its own provider through ContentResolver. */
public class Main extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        try {
            PackageManager pm = getPackageManager();
            ProviderInfo low = pm.getProviderInfo(new ComponentName(this, Low.class), PackageManager.GET_META_DATA);
            Log.i(Base.TAG, "meta-data " + low.metaData.getString("com.example.atlprovider.KEY"));
            ProviderInfo alias = pm.resolveContentProvider("com.example.atlprovider.alias", 0);
            Log.i(Base.TAG, "resolved " + (alias == null ? "none" : alias.name));
            Log.i(Base.TAG, "own type " + getContentResolver().getType(Uri.parse("content://com.example.atlprovider.high/x")));
        } catch (PackageManager.NameNotFoundException e) {
            Log.i(Base.TAG, "lookup failed " + e);
        }
        Log.i(Base.TAG, "done");
        finish(); // Theme.NoDisplay requires it before onResume
    }
}
