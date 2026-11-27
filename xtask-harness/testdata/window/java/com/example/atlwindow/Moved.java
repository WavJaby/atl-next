// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;
import android.view.Choreographer;

/**
 * The move stage's second process (window: android:process=":moved"), started by the instance relaunched for the move, so after the monitor changed. At its first vsync it logs the display its
 * services and its own SurfaceFlinger answer (Main.logDisplay, `display moved-process`): both must be the moved display, not the one the run started on (move_output.rs `second-process`).
 */
public class Moved extends Service {
    @Override
    public void onCreate() {
        super.onCreate();
        Log.i(Main.TAG, "moved process created pid=" + android.os.Process.myPid());
        // an anonymous class: the fixtures compile without the lambda metafactory
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                Main.logDisplay("moved-process");
            }
        });
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
