// SPDX-License-Identifier: MIT
package com.example.atlwidgets;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.view.Choreographer;
import android.widget.TextView;

/** The widget gate's second window (`xtask test window --stage widgets`, sub-gate window): started by Main, shown for FRAMES frame callbacks, then finished, as an app's sub-page and back. */
public class Second extends Activity {
    /** Frames it stays: 0.2 s at 60 Hz, past its first layout and draw. */
    static final int FRAMES = 12;

    private int frames;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        TextView text = new TextView(this);
        text.setText("Second window");
        setContentView(text);
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(Main.TAG, "second resumed");
        post();
    }

    private void post() {
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                frames++;
                if (frames < FRAMES) {
                    post();
                } else {
                    finish();
                }
            }
        });
    }
}
