// SPDX-License-Identifier: MIT
package com.example.atlprovider;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.net.Uri;
import android.util.Log;

/** `xtask test provider`: logs its class at onCreate and answers getType with it, so the gate sees which providers ran and in what order. */
public abstract class Base extends ContentProvider {
    static final String TAG = "AtlProvider";

    @Override
    public boolean onCreate() {
        Log.i(TAG, "provider created " + getClass().getSimpleName());
        return true;
    }

    @Override
    public String getType(Uri uri) {
        return getClass().getSimpleName();
    }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] args, String order) {
        return null;
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        return null;
    }

    @Override
    public int delete(Uri uri, String selection, String[] args) {
        return 0;
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] args) {
        return 0;
    }
}
