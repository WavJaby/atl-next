// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;

/**
 * The start-finish stage's launcher (window-start): starts Main twice and finishes, as a splash run again does (Fossify Notes). Main is singleTop there, so the second start reuses the
 * top instance: onNewIntent, no second instance, no refusal (ActivityStarter.deliverToCurrentTopIfNeeded). Logs nothing, so the stage's marker is Main's resume.
 */
public class Start extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        startActivity(new Intent(this, Main.class));
        startActivity(new Intent(this, Main.class).putExtra("again", true));
        finish();
    }
}
