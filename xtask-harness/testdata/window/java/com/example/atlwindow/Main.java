// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Activity;
import android.app.ActivityManager;
import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.Rect;
import android.os.Bundle;
import android.os.IBinder;
import android.os.Parcel;
import android.os.Process;
import android.util.Log;
import android.view.Choreographer;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;
import android.view.WindowManager;

/**
 * The window gate: `xtask test window` (xtask-harness/src/window.rs) waits for the onResume line.
 * The content view paints four solid quadrants and no text: top-left red, top-right green, bottom-left blue, bottom-right white (the frames stage checks these pixels).
 * After onResume the activity takes FRAME_CALLBACKS Choreographer frame callbacks, each logging its frame time (the frames stage checks the cadence).
 * Each touch the content view gets is logged (the input stage checks it against the injected pointer script).
 * Each layout of the content view logs its size, the window's and the display's bounds and screenWidthDp (the resize stage checks them against the configures it injects).
 * Each marker is logged once, when it is first reached.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    /** The window's title: the compositor's layer name, by which the frames stage finds the first frame (xtask-harness/src/window.rs WINDOW_TITLE). */
    static final String WINDOW_TITLE = "AtlWindowFixture";
    /** The TaskDescription label set after the first draw: the desktop window's title must become it (xtask-harness/src/window.rs TASK_LABEL). */
    static final String TASK_LABEL = "ATL task description";
    /** Frame callbacks per run (xtask-harness/src/window.rs FRAME_CALLBACKS). */
    static final int FRAME_CALLBACKS = 31;
    /** The compositor registry's name and descriptor, and its GET_COMPOSER code (atl-services compositor_registry.rs). */
    static final String COMPOSER_REGISTRY = "atl.compositor_registry";
    static final String COMPOSER_REGISTRY_DESCRIPTOR = "atl.ICompositorRegistry";
    static final int GET_COMPOSER = 5;
    /** REMOVE_TASK (compositor_registry.rs) and the Task it names for the foreign-close probe: Integer.MAX_VALUE, a valid Task number the services never reach in one run, so no Task of this uid. */
    static final int REMOVE_TASK = 6;
    static final int FOREIGN_TASK = Integer.MAX_VALUE;
    /** The activity meta-data whose int value the activity requests with setRequestedOrientation (window-orientation; xtask-harness/src/window.rs stage orientation). */
    static final String REQUEST_ORIENTATION = "atl.requestOrientation";

    private int frameCallbacks;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "onCreate reached");
        logDensity("created", getResources().getConfiguration());
        // the title reaches the window's attributes only through them: Window.setTitle leaves the default (the component name)
        WindowManager.LayoutParams attrs = getWindow().getAttributes();
        attrs.setTitle(WINDOW_TITLE);
        getWindow().setAttributes(attrs);
        setContentView(new Quadrants(this));
        requestOrientation();
    }

    /** Manifest meta-data REQUEST_ORIENTATION on this activity (window-orientation): setRequestedOrientation with its value at the first create, then the request read back (orientation stage). */
    private void requestOrientation() {
        Bundle meta;
        try {
            meta = getPackageManager().getActivityInfo(getComponentName(), android.content.pm.PackageManager.GET_META_DATA).metaData;
        } catch (android.content.pm.PackageManager.NameNotFoundException e) {
            Log.w(TAG, "own activity info: " + e);
            return;
        }
        if (meta == null || !meta.containsKey(REQUEST_ORIENTATION)) {
            return;
        }
        int wanted = meta.getInt(REQUEST_ORIENTATION);
        if (getRequestedOrientation() != wanted) {
            setRequestedOrientation(wanted);
        }
        Log.i(TAG, "requested orientation set=" + wanted + " get=" + getRequestedOrientation());
    }

    /** A start that reused this instance (singleTop in window-start): the start-finish stage's new-intent sub-gate reads this line. */
    @Override
    protected void onNewIntent(android.content.Intent intent) {
        super.onNewIntent(intent);
        Log.i(TAG, "onNewIntent reached again=" + intent.getBooleanExtra("again", false));
    }

    /* The lifecycle after the desktop's close (capture stage `destroy`): AOSP's removeTask runs pause, stop, destroy in this order before the process ends. */
    @Override
    protected void onPause() {
        super.onPause();
        Log.i(TAG, "onPause reached");
    }

    @Override
    protected void onStop() {
        super.onStop();
        Log.i(TAG, "onStop reached");
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        Log.i(TAG, "onDestroy reached");
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "onResume reached");
        // the virtual keyboard's ground truth (PhoneWindow.preparePanel loads this map on every resume)
        KeyCharacterMap map = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD);
        int keyA = map.get(KeyEvent.KEYCODE_A, 0);
        Log.i(TAG, "keymap keyboardType=" + map.getKeyboardType() + " keyA=" + (char) keyA
                + (keyA == 'a' ? " ok" : " wrong"));
        postFrame();
        logComposer();
    }

    /**
     * The capture stage's refusal probe (xtask-harness/src/window/capture.rs, sub-gate refused): this process is an app, so the registry must refuse its
     * GET_COMPOSER with SECURITY. The call is the registry's own transaction, the interface token then the package, and the reply starts with
     * the exception header, which Parcel.readException raises as SecurityException. One line: the status and the uid.
     */
    private void logComposer() {
        String status;
        try {
            // ServiceManager is hidden from the SDK stubs this build compiles against: reflection on its greylisted getService
            IBinder registry = (IBinder) Class.forName("android.os.ServiceManager")
                    .getMethod("getService", String.class).invoke(null, COMPOSER_REGISTRY);
            Parcel data = Parcel.obtain();
            Parcel reply = Parcel.obtain();
            try {
                data.writeInterfaceToken(COMPOSER_REGISTRY_DESCRIPTOR);
                data.writeString(getPackageName());
                registry.transact(GET_COMPOSER, data, reply, 0);
                reply.readException();
                status = "ok";
            } finally {
                data.recycle();
                reply.recycle();
            }
        } catch (SecurityException e) {
            status = "SECURITY";
        } catch (Exception e) {
            status = "error";
            Log.w(TAG, "composer GET_COMPOSER failed: " + e);
        }
        Log.i(TAG, "composer GET_COMPOSER status=" + status + " uid=" + Process.myUid());
    }

    /** F12 is the capture stage's trigger for the foreign-close probe; no other stage presses it, so no other stage sends the probe. */
    @Override
    public boolean onKeyUp(int keyCode, KeyEvent event) {
        if (keyCode == KeyEvent.KEYCODE_F12) {
            removeForeignTask();
            return true;
        }
        return super.onKeyUp(keyCode, event);
    }

    /**
     * The capture stage's foreign-close probe (capture.rs, sub-gate foreign; its trigger is F12, onKeyUp): REMOVE_TASK naming FOREIGN_TASK, a Task this uid does not own, must be refused by the registry
     * (PERMISSION_DENIED, counted in the services log). The reply carries a status only, so the line here records that the call was made; the verdict is the services' count.
     */
    private void removeForeignTask() {
        String sent = "yes";
        try {
            IBinder registry = (IBinder) Class.forName("android.os.ServiceManager")
                    .getMethod("getService", String.class).invoke(null, COMPOSER_REGISTRY);
            Parcel data = Parcel.obtain();
            Parcel reply = Parcel.obtain();
            try {
                data.writeInterfaceToken(COMPOSER_REGISTRY_DESCRIPTOR);
                data.writeInt(FOREIGN_TASK);
                registry.transact(REMOVE_TASK, data, reply, 0);
            } finally {
                data.recycle();
                reply.recycle();
            }
        } catch (Exception e) {
            // a refused status may surface as an exception: the call was still made
            sent = "yes " + e.getClass().getSimpleName();
        }
        Log.i(TAG, "composer REMOVE_TASK foreign task=" + FOREIGN_TASK + " sent=" + sent);
    }

    /** A configuration change delivered without a relaunch (ActivityConfigurationChangeItem): one line each, with the screenWidthDp it now has. */
    @Override
    public void onConfigurationChanged(android.content.res.Configuration newConfig) {
        super.onConfigurationChanged(newConfig);
        Log.i(TAG, "configuration changed screenWidthDp=" + newConfig.screenWidthDp + " orientation=" + newConfig.orientation);
        logDensity("changed", newConfig);
    }

    /** The density this instance sees (xtask-harness/src/window.rs move_checks): the Configuration's and the resources' DisplayMetrics, which must agree. */
    private void logDensity(String when, android.content.res.Configuration config) {
        Log.i(TAG, "density " + when + " config=" + config.densityDpi
                + " metrics=" + getResources().getDisplayMetrics().densityDpi);
    }

    /** One frame callback per vsync, FRAME_CALLBACKS in all; each logs the frame time it was given (CLOCK_MONOTONIC ns). */
    private void postFrame() {
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                frameCallbacks++;
                Log.i(TAG, "frame " + frameCallbacks + " time=" + frameTimeNanos);
                if (frameCallbacks < FRAME_CALLBACKS) {
                    postFrame();
                }
            }
        });
    }

    /** Four solid colours, one per quadrant; no text. */
    static class Quadrants extends View {
        private final Paint paint = new Paint();
        private boolean drawn;

        Quadrants(Context context) {
            super(context);
        }

        @Override
        protected void onDraw(Canvas canvas) {
            super.onDraw(canvas);
            if (!drawn) {
                drawn = true;
                // hardware=true: a RecordingCanvas, so hwui's RenderThread draws it (stage hwui checks this)
                Log.i(TAG, "onDraw reached hardware=" + canvas.isHardwareAccelerated());
                // the touch mode the window was added in (WMS ADD_FLAG_IN_TOUCH_MODE -> ViewRootImpl's first traversal): true on AOSP (config_defaultInTouchMode), so no focus highlight on the first frame
                Log.i(TAG, "touch-mode first-draw=" + isInTouchMode());
                // the title stage: the window was titled by the label (no TaskDescription label yet); a label set now retitles it
                post(new Runnable() {
                    @Override
                    public void run() {
                        ((Activity) getContext()).setTaskDescription(new ActivityManager.TaskDescription(TASK_LABEL));
                        Log.i(TAG, "task description set");
                    }
                });
            }
            int halfWidth = getWidth() / 2;
            int halfHeight = getHeight() / 2;
            int right = getWidth();
            int bottom = getHeight();
            paint.setStyle(Paint.Style.FILL);
            paint.setColor(Color.RED);
            canvas.drawRect(0, 0, halfWidth, halfHeight, paint);
            paint.setColor(Color.GREEN);
            canvas.drawRect(halfWidth, 0, right, halfHeight, paint);
            paint.setColor(Color.BLUE);
            canvas.drawRect(0, halfHeight, halfWidth, bottom, paint);
            paint.setColor(Color.WHITE);
            canvas.drawRect(halfWidth, halfHeight, right, bottom, paint);
        }

        /** One line per size change (xtask-harness/src/window.rs resize_checks): the content size, screenWidthDp, the window's bounds and the display's. */
        @Override
        protected void onSizeChanged(int w, int h, int oldw, int oldh) {
            super.onSizeChanged(w, h, oldw, oldh);
            WindowManager wm = getContext().getSystemService(WindowManager.class);
            Rect window = wm.getCurrentWindowMetrics().getBounds();
            Rect display = wm.getMaximumWindowMetrics().getBounds();
            Log.i(TAG, "size=" + w + "x" + h + " screenWidthDp=" + getResources().getConfiguration().screenWidthDp
                    + " window=" + window.width() + "x" + window.height()
                    + " display=" + display.width() + "x" + display.height()
                    + " orientation=" + getResources().getConfiguration().orientation);
        }

        /** Logs each touch: the action, the pointer's x and y, the pointer count (xtask-harness/src/window.rs input_check). An UP past BLOCK_MIN_X sleeps BLOCK_MS on the UI thread: the gtk stage's liveness probe expects no pong meanwhile. */
        @Override
        public boolean onTouchEvent(MotionEvent event) {
            Log.i(TAG, "touch action=" + event.getActionMasked() + " x=" + event.getX() + " y=" + event.getY() + " pointers=" + event.getPointerCount());
            if (event.getActionMasked() == MotionEvent.ACTION_UP && event.getX() > BLOCK_MIN_X) {
                try {
                    Thread.sleep(BLOCK_MS);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
            return true;
        }

        static final float BLOCK_MIN_X = 500f;
        static final long BLOCK_MS = 2000L;
    }
}
