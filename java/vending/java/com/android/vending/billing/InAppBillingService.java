// SPDX-License-Identifier: MIT
package com.android.vending.billing;

import android.app.Service;
import android.content.Intent;
import android.os.Binder;
import android.os.IBinder;
import android.os.Parcel;
import android.os.RemoteException;

/**
 * Answers every billing client "billing unavailable" (M62 D1; AGENTS Languages exception: the framework instantiates a Service by class name, JNI cannot create one).
 * Constants -- descriptor and transaction codes: the public Play Billing library client's IInAppBillingService proxy (isBillingSupported = FIRST_CALL_TRANSACTION + 0 = 1,
 * isBillingSupportedExtraParams = FIRST_CALL_TRANSACTION + 9 = 10); 3 = BillingResponseCode.BILLING_UNAVAILABLE (developer.android.com, Play Billing reference).
 * Every other call is unanswered (onTransact false -> the client sees a failed transaction), so no purchase, licence or account is ever reported.
 */
public final class InAppBillingService extends Service {
    private static final String DESCRIPTOR = "com.android.vending.billing.IInAppBillingService";
    private static final int IS_BILLING_SUPPORTED = IBinder.FIRST_CALL_TRANSACTION;
    private static final int IS_BILLING_SUPPORTED_EXTRA_PARAMS = IBinder.FIRST_CALL_TRANSACTION + 9;
    private static final int BILLING_UNAVAILABLE = 3;

    private final Binder binder = new Binder() {
        {
            attachInterface(null, DESCRIPTOR);
        }

        @Override
        protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) throws RemoteException {
            if (code != IS_BILLING_SUPPORTED && code != IS_BILLING_SUPPORTED_EXTRA_PARAMS) {
                return super.onTransact(code, data, reply, flags);
            }
            data.enforceInterface(DESCRIPTOR);
            reply.writeNoException();
            reply.writeInt(BILLING_UNAVAILABLE);
            return true;
        }
    };

    @Override
    public IBinder onBind(Intent intent) {
        return binder;
    }
}
