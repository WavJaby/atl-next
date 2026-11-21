// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Activity;
import android.content.ComponentName;
import android.content.Intent;
import android.content.ServiceConnection;
import android.os.Bundle;
import android.os.IBinder;
import android.util.Log;

/**
 * The aux-process stage's launcher (window-aux; its own window stays empty): binds Aux, which starts the ":aux" process, and only once it is connected starts AuxMain and finishes. So the second process of
 * the uid has registered its UI agent before the window activity's first layout (the order that broke WeChat when its ":push" process came first).
 */
public class AuxStart extends Activity {
    private final ServiceConnection connection = new ServiceConnection() {
        @Override
        public void onServiceConnected(ComponentName name, IBinder service) {
            Log.i(Main.TAG, "aux bound before main");
            startActivity(new Intent(AuxStart.this, AuxMain.class));
            finish();
        }

        @Override
        public void onServiceDisconnected(ComponentName name) {
        }
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        bindService(new Intent(this, Aux.class), connection, BIND_AUTO_CREATE);
    }
}
