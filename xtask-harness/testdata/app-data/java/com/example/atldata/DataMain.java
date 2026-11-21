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

/**
 * `xtask test app-data` (xtask-harness/src/app_data.rs reads the AtlData lines): the first launch writes one file under the files dir (and the same file under the device-protected and external
 * files dirs, so the gate finds each on the host), the next one reads it back. The `paths` line names the dirs as the app sees them.
 */
public class DataMain extends Activity {
    static final String FILE = "save.txt";
    static final String VALUE = "save-v1";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        File files = getFilesDir();
        File de = createDeviceProtectedStorageContext().getFilesDir();
        File external = getExternalFilesDir(null);
        Log.i("AtlData", "paths files=" + files + " de=" + de + " external=" + external + " source=" + getApplicationInfo().sourceDir);
        File file = new File(files, FILE);
        String state;
        try {
            if (file.exists()) {
                state = "read " + readText(file);
            } else {
                writeText(file, VALUE);
                writeText(new File(de, FILE), VALUE);
                if (external != null) {
                    writeText(new File(external, FILE), VALUE);
                }
                state = "wrote";
            }
        } catch (IOException e) {
            state = "failed " + e;
        }
        Log.i("AtlData", "data: " + state + " dir=" + files.getAbsolutePath());
        Log.i("AtlData", "shortcut: " + shortcut());
        TextView view = new TextView(this);
        view.setText(state);
        setContentView(view);
    }

    /** The dynamic shortcut ids the service kept from an earlier launch ("-" for none); with none, shortcut `keep` is published for the next launch to find. */
    private String shortcut() {
        try {
            android.content.pm.ShortcutManager sm = getSystemService(android.content.pm.ShortcutManager.class);
            if (sm == null) {
                return "absent";
            }
            StringBuilder ids = new StringBuilder();
            for (android.content.pm.ShortcutInfo s : sm.getDynamicShortcuts()) {
                ids.append(ids.length() == 0 ? "" : ",").append(s.getId());
            }
            if (ids.length() == 0) {
                sm.setDynamicShortcuts(java.util.Collections.singletonList(new android.content.pm.ShortcutInfo.Builder(this, "keep").setShortLabel("keep")
                        .setIntent(new android.content.Intent(android.content.Intent.ACTION_VIEW).setClass(this, DataMain.class)).build()));
                return "-";
            }
            return ids.toString();
        } catch (Throwable t) {
            return "threw " + t;
        }
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
