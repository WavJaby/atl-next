// SPDX-License-Identifier: MIT
package com.example.atldialog;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.view.View;

/**
 * The back sub-gate's sub-activity (xtask-harness/src/window/dialog.rs): Main starts it after the dialog's first draw; it leaves Back unhandled, so the services' requestFinish finishes it and Main shows again.
 * Logs onResume and onDestroy with a "sub" prefix, so Main's lines are told apart from its.
 */
public class Sub extends Activity {
    static final int SUB = 0xff408040;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        View content = new View(this);
        content.setBackgroundColor(SUB);
        setContentView(content);
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(Main.TAG, "sub onResume reached");
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        Log.i(Main.TAG, "sub onDestroy reached");
    }
}
