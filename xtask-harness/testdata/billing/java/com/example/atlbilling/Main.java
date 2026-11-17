// SPDX-License-Identifier: MIT
package com.example.atlbilling;

import android.app.Activity;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.ServiceConnection;
import android.os.Bundle;
import android.os.IBinder;
import android.os.Parcel;
import android.util.Log;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;

/**
 * `xtask test billing` (xtask-harness/src/billing.rs reads the AtlBilling lines): what a Play Billing client does first -- resolve the store's billing service, bind it (implicit Intent with
 * setPackage, as BillingClient does), then ask isBillingSupported (code 1) and isBillingSupportedExtraParams (code 10) through the IInAppBillingService wire format, and one call the
 * responder must not answer (code 2).
 */
public class Main extends Activity {
    private static final String TAG = "AtlBilling";
    private static final String ACTION = "com.android.vending.billing.InAppBillingService.BIND";
    private static final String STORE = "com.android.vending";
    private static final String DESCRIPTOR = "com.android.vending.billing.IInAppBillingService";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        final Context app = getApplicationContext();
        final CountDownLatch connected = new CountDownLatch(1);
        Intent intent = new Intent(ACTION).setPackage(STORE);
        int found = getPackageManager().queryIntentServices(intent, 0).size();
        // callbacks on their own thread: onCreate waits for them before finish() (a finished task's process is killed)
        boolean bound = app.bindService(intent, BIND_AUTO_CREATE, Executors.newSingleThreadExecutor(), new ServiceConnection() {
            @Override
            public void onServiceConnected(ComponentName name, IBinder service) {
                String supported = call(service, IBinder.FIRST_CALL_TRANSACTION, false);
                String extra = call(service, IBinder.FIRST_CALL_TRANSACTION + 9, true);
                String other = call(service, IBinder.FIRST_CALL_TRANSACTION + 1, false);
                Log.i(TAG, "connected " + name.getClassName() + " supported " + supported + " extra " + extra + " other " + other);
                app.unbindService(this);
                connected.countDown();
            }

            @Override
            public void onServiceDisconnected(ComponentName name) {
                Log.i(TAG, "disconnected " + name.getClassName());
            }
        });
        Log.i(TAG, "query " + found + " bind " + bound);
        if (bound) {
            try {
                if (!connected.await(CONNECT_WAIT_S, TimeUnit.SECONDS)) {
                    Log.i(TAG, "no connection in " + CONNECT_WAIT_S + " s");
                }
            } catch (InterruptedException e) {
                Log.i(TAG, "interrupted");
            }
        }
        Log.i(TAG, "done");
        finish(); // Theme.NoDisplay requires it before onResume
    }

    /** Room for a cold service start (process start + ART init without a boot image), under the gate's 180 s limit (billing.rs LIMIT). */
    private static final long CONNECT_WAIT_S = 60;

    /** One call in the AIDL proxy's layout (apiVersion, packageName, type[, extraParams = null Bundle]): the reply's int, or "false" when the service did not answer the code. */
    private String call(IBinder service, int code, boolean extraParams) {
        Parcel data = Parcel.obtain();
        Parcel reply = Parcel.obtain();
        try {
            data.writeInterfaceToken(DESCRIPTOR);
            data.writeInt(extraParams ? 7 : 3);
            data.writeString(getPackageName());
            data.writeString("inapp");
            if (extraParams) {
                data.writeInt(0);
            }
            if (!service.transact(code, data, reply, 0)) {
                return "false";
            }
            reply.readException();
            return String.valueOf(reply.readInt());
        } catch (Throwable t) {
            return "threw " + t;
        } finally {
            data.recycle();
            reply.recycle();
        }
    }
}
