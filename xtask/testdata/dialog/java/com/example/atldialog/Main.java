// SPDX-License-Identifier: MIT
package com.example.atldialog;

import android.app.Activity;
import android.graphics.PixelFormat;
import android.graphics.Rect;
import android.os.Bundle;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
import android.view.ViewTreeObserver;
import android.view.WindowManager;
import android.widget.FrameLayout;
import android.widget.PopupWindow;

/**
 * The dialog gate: `xtask test window --stage dialog` (xtask/src/window/dialog.rs). A software window of colour WINDOW with an ANCHOR view near its bottom;
 * a PopupWindow of POPUP drops down from the anchor (no room below, so it must open above it, inside the window); then a dialog window of DIALOG, DIALOG_W x DIALOG_H,
 * centred (gravity 0), FLAG_DIM_BEHIND at DIM, with manual surfaceInsets INSET on each side (a shadow's room; set by reflection, the field is hidden API).
 * Sizes are pixels; colours stay at or under 0x80 per channel, so the dimmed value is exact whether the compositor's alpha is 0.5 or 128/255.
 * After the dialog's first draw the fixture logs the window size and the anchor's rectangle.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    static final int WINDOW = 0xff204060;
    static final int ANCHOR = 0xff606020;
    static final int POPUP = 0xff804000;
    static final int DIALOG = 0xffc08020;
    static final float DIM = 0.5f;
    static final int DIALOG_W = 240, DIALOG_H = 120, INSET = 16;
    static final int ANCHOR_W = 120, ANCHOR_H = 40, ANCHOR_LEFT = 40, ANCHOR_BOTTOM = 8;
    static final int POPUP_W = 160, POPUP_H = 80;

    private FrameLayout root;
    private View anchor;
    private boolean shown;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        root = new FrameLayout(this);
        root.setBackgroundColor(WINDOW);
        anchor = new View(this);
        anchor.setBackgroundColor(ANCHOR);
        FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(ANCHOR_W, ANCHOR_H, Gravity.BOTTOM | Gravity.LEFT);
        lp.leftMargin = ANCHOR_LEFT;
        lp.bottomMargin = ANCHOR_BOTTOM;
        root.addView(anchor, lp);
        setContentView(root);
        root.getViewTreeObserver().addOnGlobalLayoutListener(new ViewTreeObserver.OnGlobalLayoutListener() {
            @Override
            public void onGlobalLayout() {
                if (shown) {
                    return;
                }
                shown = true;
                root.post(new Runnable() {
                    @Override
                    public void run() {
                        showPopup();
                        showDialog();
                    }
                });
            }
        });
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "onResume reached");
    }

    private void showPopup() {
        View content = new View(this);
        content.setBackgroundColor(POPUP);
        PopupWindow popup = new PopupWindow(content, POPUP_W, POPUP_H);
        popup.setBackgroundDrawable(null);
        popup.setElevation(0);
        popup.setAnimationStyle(0);
        popup.showAsDropDown(anchor);
    }

    private void showDialog() {
        final View content = new View(this);
        content.setBackgroundColor(DIALOG);
        WindowManager.LayoutParams lp = new WindowManager.LayoutParams(DIALOG_W, DIALOG_H, WindowManager.LayoutParams.TYPE_APPLICATION,
                WindowManager.LayoutParams.FLAG_DIM_BEHIND, PixelFormat.TRANSLUCENT);
        lp.gravity = 0;
        lp.dimAmount = DIM;
        try {
            ((Rect) WindowManager.LayoutParams.class.getField("surfaceInsets").get(lp)).set(INSET, INSET, INSET, INSET);
            WindowManager.LayoutParams.class.getField("hasManualSurfaceInsets").setBoolean(lp, true);
        } catch (ReflectiveOperationException e) {
            Log.i(TAG, "dialog surfaceInsets not set: " + e);
            return;
        }
        content.getViewTreeObserver().addOnDrawListener(new ViewTreeObserver.OnDrawListener() {
            private boolean logged;

            @Override
            public void onDraw() {
                if (logged) {
                    return;
                }
                logged = true;
                int[] at = new int[2];
                anchor.getLocationInWindow(at);
                Log.i(TAG, "dialog shown window=" + root.getWidth() + "x" + root.getHeight() + " anchor=" + at[0] + "," + at[1] + ","
                        + (at[0] + anchor.getWidth()) + "," + (at[1] + anchor.getHeight()));
            }
        });
        getWindowManager().addView(content, lp);
    }
}
