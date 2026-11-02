// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;

/** The M2 gate: `xtask test app` (xtask/src/app.rs MARKER) waits for this line. */
public class Main extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("AtlTest", "onCreate reached");
    }
}
