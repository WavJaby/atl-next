// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Service;
import android.content.Intent;
import android.hardware.display.DisplayManager;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.util.Log;
import android.view.Choreographer;

/**
 * The move stage's watch process (window-move: android:process=":watch"), started by the first instance, so before the move; it has no window and never reports a monitor. It logs the display
 * at its first vsync (`display watch-start`) and at each display callback (`display watch-changed`, DisplayManager.DisplayListener.onDisplayChanged): at its first callback after the move its own
 * SurfaceFlinger must already answer the moved display, as AOSP changes the SurfaceFlinger mode before DisplayManagerService's event (move_output.rs `watch`).
 */
public class Watch extends Service {
    @Override
    public void onCreate() {
        super.onCreate();
        Log.i(Main.TAG, "watch process created pid=" + android.os.Process.myPid());
        // anonymous classes: the fixtures compile without the lambda metafactory
        getSystemService(DisplayManager.class).registerDisplayListener(new DisplayManager.DisplayListener() {
            @Override
            public void onDisplayAdded(int displayId) {}

            @Override
            public void onDisplayRemoved(int displayId) {}

            @Override
            public void onDisplayChanged(int displayId) {
                Main.logDisplay("watch-changed");
            }
        }, new Handler(Looper.getMainLooper()));
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                Main.logDisplay("watch-start");
            }
        });
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
