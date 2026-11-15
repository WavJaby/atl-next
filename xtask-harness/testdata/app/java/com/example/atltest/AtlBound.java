// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Service;
import android.content.Intent;
import android.os.Binder;
import android.os.IBinder;
import android.os.Parcel;
import android.os.RemoteException;
import android.util.Log;

/** Main.bound's service (app.rs ASYNC_LINES `bound`, `unbind`): its binder answers {@link #ANSWER} to FIRST_CALL_TRANSACTION; onUnbind logs the Intent it was bound with. */
public class AtlBound extends Service {
    static final int ANSWER = 42;

    private final Binder binder = new Binder() {
        @Override
        protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) throws RemoteException {
            if (code == FIRST_CALL_TRANSACTION) {
                reply.writeInt(ANSWER);
                return true;
            }
            return super.onTransact(code, data, reply, flags);
        }
    };

    @Override
    public IBinder onBind(Intent intent) {
        return binder;
    }

    @Override
    public boolean onUnbind(Intent intent) {
        Log.i("AtlTest", "unbind: onUnbind action=" + intent.getAction());
        return false;
    }
}
