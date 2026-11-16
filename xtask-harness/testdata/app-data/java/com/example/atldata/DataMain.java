// SPDX-License-Identifier: MIT
package com.example.atldata;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.widget.TextView;

import java.io.DataInputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;

/** `xtask test app-data` (xtask-harness/src/app_data.rs reads the AtlData line): the first launch writes one file under the files dir, the next one reads it back. */
public class DataMain extends Activity {
    static final String FILE = "save.txt";
    static final String VALUE = "save-v1";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        File file = new File(getFilesDir(), FILE);
        String state;
        try {
            if (file.exists()) {
                state = "read " + readText(file);
            } else {
                writeText(file, VALUE);
                state = "wrote";
            }
        } catch (IOException e) {
            state = "failed " + e;
        }
        Log.i("AtlData", "data: " + state + " dir=" + getFilesDir().getAbsolutePath());
        TextView view = new TextView(this);
        view.setText(state);
        setContentView(view);
    }

    private static void writeText(File file, String text) throws IOException {
        FileOutputStream out = new FileOutputStream(file);
        try {
            out.write(text.getBytes("UTF-8"));
        } finally {
            out.close();
        }
    }

    private static String readText(File file) throws IOException {
        byte[] bytes = new byte[(int) file.length()];
        DataInputStream in = new DataInputStream(new FileInputStream(file));
        try {
            in.readFully(bytes);
        } finally {
            in.close();
        }
        return new String(bytes, "UTF-8");
    }
}
