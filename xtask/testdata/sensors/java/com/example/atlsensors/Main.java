// SPDX-License-Identifier: MIT
package com.example.atlsensors;

import android.app.Activity;
import android.hardware.Sensor;
import android.hardware.SensorEvent;
import android.hardware.SensorEventListener;
import android.hardware.SensorManager;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.SystemClock;
import android.util.Log;

/**
 * The sensors gate's app (xtask/src/sensors.rs reads its `AtlSensors` lines).
 * onCreate: "start" (the gate times the lookup from it), "lookup ms=" how long getSystemService + getDefaultSensor(TYPE_ACCELEROMETER) took, then "list n=" the sensor count.
 * No accelerometer: "default null register=<registerListener result>", then "done".
 * One: "default name=... type=... stringType=... minDelay=...", registerListener(GAME), one "event ts=<timestamp> now=<elapsedRealtimeNanos> v=<x>,<y>,<z>" line per event up to EVENTS, then unregisterListener and "done events=<n>".
 * The key tilt's injected accelerometer (INJECTED): every event for TILT_WAIT_MS, so the gate sees the keys its script holds.
 */
public class Main extends Activity implements SensorEventListener {
    private static final String TAG = "AtlSensors";
    /** Events logged before the listener is removed. */
    private static final int EVENTS = 5;
    /** How long to wait for them (the gate's limit is longer). */
    private static final long WAIT_MS = 3000;
    /** atl-services sensor.rs INJECTED_NAME. */
    private static final String INJECTED = "ATL injected accelerometer";
    /** Longer than the gate's tilt script (sensors.rs TILT_SCRIPT) from the first frame. */
    private static final long TILT_WAIT_MS = 16000;

    private SensorManager manager;
    private int events;
    private boolean finished;
    private boolean tilt;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "start");
        long t0 = SystemClock.elapsedRealtime();
        manager = (SensorManager) getSystemService(SENSOR_SERVICE);
        Sensor accel = manager.getDefaultSensor(Sensor.TYPE_ACCELEROMETER);
        Log.i(TAG, "lookup ms=" + (SystemClock.elapsedRealtime() - t0));
        Log.i(TAG, "list n=" + manager.getSensorList(Sensor.TYPE_ALL).size());
        if (accel == null) {
            Log.i(TAG, "default null register=" + manager.registerListener(this, null, SensorManager.SENSOR_DELAY_GAME));
            Log.i(TAG, "done");
            return;
        }
        Log.i(TAG, "default name=" + accel.getName() + " type=" + accel.getType() + " stringType=" + accel.getStringType() + " minDelay=" + accel.getMinDelay());
        tilt = INJECTED.equals(accel.getName());
        Log.i(TAG, "register=" + manager.registerListener(this, accel, SensorManager.SENSOR_DELAY_GAME));
        // no lambda: the boot classpath has no LambdaMetafactory for javac
        new Handler(Looper.getMainLooper()).postDelayed(new Runnable() {
            @Override
            public void run() {
                finish_();
            }
        }, tilt ? TILT_WAIT_MS : WAIT_MS);
    }

    @Override
    public void onSensorChanged(SensorEvent e) {
        if (finished) {
            return;
        }
        Log.i(TAG, "event ts=" + e.timestamp + " now=" + SystemClock.elapsedRealtimeNanos() + " v=" + e.values[0] + "," + e.values[1] + "," + e.values[2]);
        if (++events >= EVENTS && !tilt) {
            finish_();
        }
    }

    @Override
    public void onAccuracyChanged(Sensor sensor, int accuracy) {}

    private void finish_() {
        if (finished) {
            return;
        }
        finished = true;
        manager.unregisterListener(this);
        Log.i(TAG, "done events=" + events);
    }
}
