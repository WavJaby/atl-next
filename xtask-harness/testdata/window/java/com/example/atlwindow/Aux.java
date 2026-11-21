// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Service;
import android.content.Intent;
import android.os.Binder;
import android.os.IBinder;
import android.os.Parcel;
import android.os.Process;
import android.util.Log;

/**
 * The aux-process stage's second process (window-aux: android:process=":aux"): a bound service whose process registers its own UI agent and compositor with the services at start, as every app
 * process does. Its binder takes one call, KILL, on which the process kills itself (AuxMain sends it: the stage's death of a sibling process).
 */
public class Aux extends Service {
    /** The one transaction the binder takes: the process kills itself. */
    static final int KILL = IBinder.FIRST_CALL_TRANSACTION;

    @Override
    public void onCreate() {
        super.onCreate();
        Log.i(Main.TAG, "aux created pid=" + Process.myPid());
    }

    @Override
    public IBinder onBind(Intent intent) {
        return new Binder() {
            @Override
            protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) throws android.os.RemoteException {
                if (code == KILL) {
                    Log.i(Main.TAG, "aux killing itself pid=" + Process.myPid());
                    Process.killProcess(Process.myPid());
                    return true;
                }
                return super.onTransact(code, data, reply, flags);
            }
        };
    }
}
