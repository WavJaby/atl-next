// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;

/**
 * The activity Main starts with no source activity (app.rs LOG_LINES start-app-context, start-stale-result-to): from the application Context (resultTo null) and through a raw
 * startActivity whose resultTo names no record. Logs "<row>: started calling=<getCallingActivity>" under the row its EXTRA names, then finishes.
 */
public class NoSourceChild extends Activity {
    /** The row name the start put in the intent. */
    static final String EXTRA = "atl.row";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        android.content.ComponentName calling = getCallingActivity();
        Log.i("AtlTest", getIntent().getStringExtra(EXTRA) + ": started calling=" + (calling == null ? "null" : calling.getClassName()));
        finish();
    }
}
