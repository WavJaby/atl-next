// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;

/** The start-finish stage's launcher (window-start): starts Main and finishes, as an app's splash does. Logs nothing, so the stage's marker is Main's resume. */
public class Start extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        startActivity(new Intent(this, Main.class));
        finish();
    }
}
