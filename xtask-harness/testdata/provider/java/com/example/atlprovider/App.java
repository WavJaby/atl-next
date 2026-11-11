// SPDX-License-Identifier: MIT
package com.example.atlprovider;

import android.app.Application;
import android.util.Log;

/** `xtask test provider`: its onCreate line must come after every provider's. */
public class App extends Application {
    @Override
    public void onCreate() {
        super.onCreate();
        Log.i(Base.TAG, "application created");
    }
}
