// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.os.Handler;
import android.os.Looper;
import android.webkit.ValueCallback;

/**
 * A CookieManager result for the app's ValueCallback, delivered on the calling thread's Looper (CookieManager.java javadoc of setCookie / removeAllCookies / removeSessionCookies); a caller
 * without a Looper gets the main one. The natives call {@link #post} once, from the UI thread.
 */
final class CookieReply implements Runnable {
    private final ValueCallback<Boolean> mCallback;
    private final Handler mHandler;
    private boolean mValue;

    CookieReply(ValueCallback<Boolean> callback) {
        mCallback = callback;
        Looper looper = Looper.myLooper();
        mHandler = new Handler(looper != null ? looper : Looper.getMainLooper());
    }

    // called by the natives
    void post(boolean value) {
        mValue = value;
        mHandler.post(this);
    }

    @Override public void run() { mCallback.onReceiveValue(mValue); }

    static CookieReply of(ValueCallback<Boolean> callback) { return callback == null ? null : new CookieReply(callback); }
}
