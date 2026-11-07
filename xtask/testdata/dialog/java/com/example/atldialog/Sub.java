// SPDX-License-Identifier: MIT
package com.example.atldialog;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.view.View;

/**
 * The back-close gate's sub-activity (xtask/src/window/dialog.rs): a click on Main's anchor starts it; its Back pops to Main, which stays.
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
