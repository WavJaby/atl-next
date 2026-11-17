// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.util.Log;

/**
 * Main's result child (app.rs RESULT): sets RESULT_OK with an extra and finishes in onCreate, so the result goes to Main at its next resume.
 * Started by its implicit action (Main.ACTION_RESULT_CHILD). "calling activity=<class> package=<pkg>" (app.rs CALLING): getCallingActivity / getCallingPackage, Main for a start for result.
 * "result child finishing" marks when the result is set; the order check reads Main's lines after it.
 */
public class ResultChild extends Activity {
    /** The extra Main logs back (app.rs RESULT: `extra=atl-result-7`). */
    static final String EXTRA = "atl.result";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        android.content.ComponentName calling = getCallingActivity();
        Log.i("AtlTest", "calling activity=" + (calling == null ? "null" : calling.getClassName()) + " package=" + getCallingPackage());
        Log.i("AtlTest", "result child finishing");
        setResult(RESULT_OK, new Intent().putExtra(EXTRA, "atl-result-7"));
        finish();
    }
}
