// SPDX-License-Identifier: MIT
package com.example.atlwindow;

import android.app.Dialog;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.ServiceConnection;
import android.graphics.Canvas;
import android.graphics.Color;
import android.os.Bundle;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.Parcel;
import android.os.RemoteException;
import android.util.Log;
import android.view.View;
import android.view.ViewGroup;

/**
 * The aux-process stage's window activity (window-aux): Main's quadrants, with a sibling process of its uid around it. AuxStart already started ":aux" before this activity's first layout. Once
 * resumed and bound, it waits KILL_DELAY_MS (its first frames present meanwhile), tells Aux to kill its process, and on that death binds Aux again: a new ":aux" process registers after this window's
 * first layout. Once that one is connected it shows a dialog, a new window of this process, so its layer must reach this process's compositor too. Lines: aux main bound, aux kill sent, aux died,
 * aux restarted, aux dialog shown, aux dialog drawn (xtask-harness/src/window/aux_process.rs).
 */
public class AuxMain extends Main {
    /** After the resume: the window's first frames present, under any host load the window gates allow. */
    static final long KILL_DELAY_MS = 3000L;
    /** The dialog's content size, px: any size inside the window. */
    static final int DIALOG_WIDTH = 300;
    static final int DIALOG_HEIGHT = 200;

    private final Handler handler = new Handler(Looper.getMainLooper());
    private IBinder aux;
    private boolean resumed;
    private boolean killScheduled;

    private final ServiceConnection first = new ServiceConnection() {
        @Override
        public void onServiceConnected(ComponentName name, IBinder service) {
            Log.i(TAG, "aux main bound");
            aux = service;
            try {
                service.linkToDeath(new IBinder.DeathRecipient() {
                    @Override
                    public void binderDied() {
                        handler.post(new Runnable() {
                            @Override
                            public void run() {
                                auxDied();
                            }
                        });
                    }
                }, 0);
            } catch (RemoteException e) {
                Log.w(TAG, "aux linkToDeath failed: " + e);
                return;
            }
            scheduleKill();
        }

        @Override
        public void onServiceDisconnected(ComponentName name) {
        }
    };

    private final ServiceConnection second = new ServiceConnection() {
        @Override
        public void onServiceConnected(ComponentName name, IBinder service) {
            Log.i(TAG, "aux restarted");
            showDialog();
        }

        @Override
        public void onServiceDisconnected(ComponentName name) {
        }
    };

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        bindService(new Intent(this, Aux.class), first, BIND_AUTO_CREATE);
    }

    @Override
    protected void onResume() {
        super.onResume();
        resumed = true;
        scheduleKill();
    }

    /** Once both resumed and bound, KILL goes to Aux KILL_DELAY_MS later. */
    private void scheduleKill() {
        if (!resumed || aux == null || killScheduled) {
            return;
        }
        killScheduled = true;
        handler.postDelayed(new Runnable() {
            @Override
            public void run() {
                Parcel data = Parcel.obtain();
                try {
                    aux.transact(Aux.KILL, data, null, IBinder.FLAG_ONEWAY);
                    Log.i(TAG, "aux kill sent");
                } catch (RemoteException e) {
                    Log.w(TAG, "aux kill failed: " + e);
                } finally {
                    data.recycle();
                }
            }
        }, KILL_DELAY_MS);
    }

    private void auxDied() {
        Log.i(TAG, "aux died");
        unbindService(first);
        bindService(new Intent(this, Aux.class), second, BIND_AUTO_CREATE);
    }

    /** A dialog, not cancelable (no input reaches it in this stage anyway), logging its first draw. */
    private void showDialog() {
        Dialog dialog = new Dialog(this);
        dialog.setCancelable(false);
        dialog.setCanceledOnTouchOutside(false);
        dialog.setContentView(new Solid(this), new ViewGroup.LayoutParams(DIALOG_WIDTH, DIALOG_HEIGHT));
        dialog.show();
        Log.i(TAG, "aux dialog shown");
    }

    /** One colour; logs its first draw. */
    static class Solid extends View {
        private boolean drawn;

        Solid(Context context) {
            super(context);
        }

        @Override
        protected void onDraw(Canvas canvas) {
            super.onDraw(canvas);
            canvas.drawColor(Color.YELLOW);
            if (!drawn) {
                drawn = true;
                Log.i(TAG, "aux dialog drawn");
            }
        }
    }
}
