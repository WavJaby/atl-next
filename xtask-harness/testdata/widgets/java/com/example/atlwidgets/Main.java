// SPDX-License-Identifier: MIT
package com.example.atlwidgets;

import android.app.Activity;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Rect;
import android.content.Intent;
import android.os.Bundle;
import android.util.Log;
import android.view.Choreographer;
import android.view.View;
import android.view.ViewTreeObserver;
import android.widget.AbsListView;
import android.widget.ArrayAdapter;
import android.widget.Button;
import android.widget.EditText;
import android.widget.ListView;
import android.widget.TextView;
import java.nio.ByteBuffer;

/**
 * The widget gate: `xtask test window --stage widgets` (xtask-harness/src/window.rs) reads the lines below from the app log.
 * Layout: each widget's rectangle in the content (`layout`), the density. Software frame: `decor` drawn into a bitmap SETTLE_FRAMES after onResume, once the default-focus ripple has ended:
 * its FNV-1a 64 digest must equal the composer's first settled readback, and the text pixels inside the TextView and EditText are non-background (`pixels`).
 * Click: the Button's OnClickListener changes the TextView text, and the next layout differs inside the TextView (`changed`).
 * Focus: a touch on the EditText focuses it. Window: the focus starts a second activity (.Second, its own window and input channel), which finishes; a tap on the Button then still reaches Main (`click count=2`).
 * Hover and wheel reach the window as generic motion (`generic action=`); a wheel and a touchpad swipe scroll the list, the swipe flings (`list state=2`). List: a drag scrolls it (`list first= top=`), and its idle starts the cadence run.
 * When the scroll ends, the list redraws on FRAME_CALLBACKS frame callbacks (`frame`, the cadence gate: the window fixture's lines, under real redraws); the last one logs the `final frame`, the last readback's expectation.
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    /** Frame callbacks per run (xtask-harness/src/window.rs FRAME_CALLBACKS). */
    static final int FRAME_CALLBACKS = 31;
    /** Frames from onResume to the software frame: 0.5 s at 60 Hz, past the first frames and the default-focus ripple (the first readback waits 1/3 s after them). */
    static final int SETTLE_FRAMES = 30;
    /** Frames after Main resumes again before the probe taps: 0.2 s at 60 Hz, past Second's destroy and window removal. */
    static final int BACK_FRAMES = 12;
    /** The Button's click count after the idle window (widgets.rs drive): its tap posts one frame callback, the resume check's line. */
    static final int RESUME_CLICK = 3;
    /** MotionEvent.CLASSIFICATION_TWO_FINGER_SWIPE (API 34), written out. */
    static final int CLASSIFICATION_TWO_FINGER_SWIPE = 3;

    private int frameCallbacks;
    private int settleFrames;
    private boolean cadenceStarted;
    private int clicks;
    private boolean laidOut;
    private TextView label;
    private Button button;
    private EditText input;
    private ListView list;
    private Bitmap first;
    private boolean secondStarted;
    private int resumes;
    private int backFrames;
    /** The last touch gesture was a touchpad swipe: its fling does not start the cadence run, the drag after it does. */
    private boolean swipeGesture;

    // every touch the window gets, before any view takes it: the probe's evidence that a touch reached the app (`touch action=` lines); `class` 3 = a touchpad swipe's fake finger (MotionEvent.CLASSIFICATION_TWO_FINGER_SWIPE); `source` 8194 mouse, 4098 touchscreen
    @Override
    public boolean dispatchTouchEvent(android.view.MotionEvent e) {
        if (e.getActionMasked() == android.view.MotionEvent.ACTION_DOWN) {
            swipeGesture = e.getClassification() == CLASSIFICATION_TWO_FINGER_SWIPE;
        }
        Log.i(TAG, "touch action=" + e.getActionMasked() + " x=" + e.getX() + " y=" + e.getY() + " class=" + e.getClassification()
                + " source=" + e.getSource() + " pointers=" + e.getPointerCount() + " device=" + e.getDeviceId());
        return super.dispatchTouchEvent(e);
    }

    // every key the window gets (`key action=` lines): the KeyEvent's fields, the character its device's map gives (getUnicodeChar -> KeyCharacterMap.load(deviceId)), that map's keyboard type (FULL 4)
    @Override
    public boolean dispatchKeyEvent(android.view.KeyEvent e) {
        Log.i(TAG, "key action=" + e.getAction() + " code=" + e.getKeyCode() + " scan=" + e.getScanCode() + " meta=" + e.getMetaState()
                + " repeat=" + e.getRepeatCount() + " flags=" + e.getFlags() + " device=" + e.getDeviceId() + " source=" + e.getSource()
                + " char=" + e.getUnicodeChar() + " kbtype=" + e.getKeyCharacterMap().getKeyboardType());
        return super.dispatchKeyEvent(e);
    }

    // the device list (`device id=` lines, once at onResume): ids and sources as InputManager reports them
    static void logDevices() {
        for (int id : android.view.InputDevice.getDeviceIds()) {
            android.view.InputDevice d = android.view.InputDevice.getDevice(id);
            Log.i(TAG, "device id=" + id + " sources=" + d.getSources() + " kbtype=" + d.getKeyboardType());
        }
    }

    // hover and wheel events (`generic action=` lines): HOVER_ENTER 9, HOVER_MOVE 7, HOVER_EXIT 10, SCROLL 8 with its AXIS_VSCROLL
    @Override
    public boolean dispatchGenericMotionEvent(android.view.MotionEvent e) {
        Log.i(TAG, "generic action=" + e.getActionMasked() + " vscroll=" + e.getAxisValue(android.view.MotionEvent.AXIS_VSCROLL) + " x=" + e.getX() + " y=" + e.getY());
        return super.dispatchGenericMotionEvent(e);
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "onCreate reached");
        // BACK through the predictive-back path (OnBackInvokedCallback, what a targetSdk 36 app gets): the mouse's back button checks it (`back callback` line)
        getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                android.window.OnBackInvokedDispatcher.PRIORITY_DEFAULT,
                new android.window.OnBackInvokedCallback() {
                    @Override
                    public void onBackInvoked() {
                        Log.i(TAG, "back callback");
                    }
                });
        setContentView(R.layout.main);
        label = (TextView) findViewById(R.id.label);
        button = (Button) findViewById(R.id.button);
        input = (EditText) findViewById(R.id.input);
        list = (ListView) findViewById(R.id.list);
        // setText from code: the EditText must render it
        input.setText("typed from code");
        // every edit after that, with its composing span (the ime stage: the in-app input method's preedit and commit land here)
        input.addTextChangedListener(new android.text.TextWatcher() {
            @Override
            public void beforeTextChanged(CharSequence s, int start, int count, int after) {}

            @Override
            public void onTextChanged(CharSequence s, int start, int before, int count) {}

            @Override
            public void afterTextChanged(android.text.Editable s) {
                Log.i(TAG, "text input=" + s + " composing=" + android.view.inputmethod.BaseInputConnection.getComposingSpanStart(s)
                        + "," + android.view.inputmethod.BaseInputConnection.getComposingSpanEnd(s));
            }
        });
        String[] items = new String[50];
        for (int i = 0; i < items.length; i++) {
            items[i] = "Item " + i;
        }
        list.setAdapter(new ArrayAdapter<String>(this, android.R.layout.simple_list_item_1, items));
        list.setOnScrollListener(new AbsListView.OnScrollListener() {
            @Override
            public void onScrollStateChanged(AbsListView view, int scrollState) {
                Log.i(TAG, "list state=" + scrollState);
                if (scrollState == SCROLL_STATE_IDLE && view.getFirstVisiblePosition() > 0 && !cadenceStarted && !swipeGesture) {
                    cadenceStarted = true;
                    postFrame();
                }
            }

            @Override
            public void onScroll(AbsListView view, int firstVisibleItem, int visibleItemCount, int totalItemCount) {
                View top = view.getChildAt(0);
                Log.i(TAG, "list first=" + firstVisibleItem + " top=" + (top == null ? 0 : top.getTop()));
            }
        });
        input.setOnFocusChangeListener(new View.OnFocusChangeListener() {
            @Override
            public void onFocusChange(View v, boolean hasFocus) {
                Log.i(TAG, "focus input=" + hasFocus);
                if (hasFocus && !secondStarted) {
                    secondStarted = true;
                    Log.i(TAG, "window second shown");
                    startActivity(new Intent(Main.this, Second.class));
                }
            }
        });
        button.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                onButton();
            }
        });
        getWindow().getDecorView().getViewTreeObserver().addOnGlobalLayoutListener(new ViewTreeObserver.OnGlobalLayoutListener() {
            @Override
            public void onGlobalLayout() {
                onFirstLayout();
            }
        });
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i(TAG, "onResume reached");
        resumes++;
        if (resumes == 1) {
            logDevices();
            postSettle();
        } else {
            postBack();
        }
    }

    /** SETTLE_FRAMES frame callbacks, then the software frame (the expectation of the first readback) and the ink inside the text fields. */
    private void postSettle() {
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                settleFrames++;
                if (settleFrames < SETTLE_FRAMES) {
                    postSettle();
                    return;
                }
                first = render();
                Log.i(TAG, "software frame fnv1a64=" + digest(first) + " size=" + first.getWidth() + "x" + first.getHeight());
                Log.i(TAG, "pixels label=" + ink(first, boundsOf(label)) + " input=" + ink(first, boundsOf(input)));
            }
        });
    }

    /** Back from Second: BACK_FRAMES frame callbacks (Second's window is removed after Main resumes: ActivityThread destroys it, then removes its decor), then the line the probe taps after. */
    private void postBack() {
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                backFrames++;
                if (backFrames < BACK_FRAMES) {
                    postBack();
                } else {
                    Log.i(TAG, "window second closed");
                }
            }
        });
    }

    /** One frame callback per vsync, FRAME_CALLBACKS in all, each with a list redraw; each logs the frame time it was given (CLOCK_MONOTONIC ns). The last logs the final frame. */
    private void postFrame() {
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                frameCallbacks++;
                Log.i(TAG, "frame " + frameCallbacks + " time=" + frameTimeNanos);
                if (frameCallbacks < FRAME_CALLBACKS) {
                    list.invalidate();
                    postFrame();
                } else {
                    Log.i(TAG, "final frame fnv1a64=" + digest(render()));
                }
            }
        });
    }

    /** One frame callback requested by the resume tap: the idle probe's proof that input asks for a frame again (`frame resume`). Nothing else requests frames after the cadence run. */
    private void postResumeFrame() {
        Choreographer.getInstance().postFrameCallback(new Choreographer.FrameCallback() {
            @Override
            public void doFrame(long frameTimeNanos) {
                Log.i(TAG, "frame resume time=" + frameTimeNanos);
            }
        });
    }

    /** The window's focus, as the input channel's focus event delivers it (ViewRootImpl onFocusEvent). */
    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        Log.i(TAG, "window focus=" + hasFocus);
    }

    /** Once: the geometry. */
    private void onFirstLayout() {
        if (laidOut) {
            return;
        }
        laidOut = true;
        Log.i(TAG, "density=" + getResources().getDisplayMetrics().density);
        logLayout("label", label);
        logLayout("button", button);
        logLayout("input", input);
        logLayout("list", list);
    }

    private void onButton() {
        clicks++;
        label.setText("Clicked " + clicks);
        Log.i(TAG, "click count=" + clicks);
        if (clicks == RESUME_CLICK) {
            postResumeFrame();
        }
        // the next draw carries the new text; a same-size text change has no layout pass, so no global layout callback
        final ViewTreeObserver observer = label.getViewTreeObserver();
        observer.addOnPreDrawListener(new ViewTreeObserver.OnPreDrawListener() {
            @Override
            public boolean onPreDraw() {
                observer.removeOnPreDrawListener(this);
                Bitmap after = render();
                Log.i(TAG, "changed label=" + changed(first, after, boundsOf(label)));
                Log.i(TAG, "text label=" + label.getText());
                return true;
            }
        });
    }

    private void logLayout(String name, View v) {
        Rect r = boundsOf(v);
        Log.i(TAG, "layout " + name + " left=" + r.left + " top=" + r.top + " width=" + v.getWidth() + " height=" + v.getHeight());
    }

    /** The window's content drawn into a bitmap, in software: what the window surface shows, if drawing is the same. */
    private Bitmap render() {
        View decor = getWindow().getDecorView();
        Bitmap bitmap = Bitmap.createBitmap(decor.getWidth(), decor.getHeight(), Bitmap.Config.ARGB_8888);
        decor.draw(new Canvas(bitmap));
        return bitmap;
    }

    /** The view's rectangle in the window's content (the window's root view is the decor). */
    private static Rect boundsOf(View v) {
        int[] at = new int[2];
        v.getLocationInWindow(at);
        return new Rect(at[0], at[1], at[0] + v.getWidth(), at[1] + v.getHeight());
    }

    /** Pixels inside `r` that differ from its top-left pixel, the background there. */
    private static int ink(Bitmap frame, Rect r) {
        int background = frame.getPixel(r.left, r.top);
        int n = 0;
        for (int y = r.top; y < r.bottom; y++) {
            for (int x = r.left; x < r.right; x++) {
                if (frame.getPixel(x, y) != background) {
                    n++;
                }
            }
        }
        return n;
    }

    /** Pixels inside `r` that differ between the two frames. */
    private static int changed(Bitmap a, Bitmap b, Rect r) {
        int n = 0;
        for (int y = r.top; y < r.bottom; y++) {
            for (int x = r.left; x < r.right; x++) {
                if (a.getPixel(x, y) != b.getPixel(x, y)) {
                    n++;
                }
            }
        }
        return n;
    }

    /** FNV-1a 64 over the bitmap's bytes, the composer's digest (xtask-harness/src/window.rs fnv1a64 of the readback). */
    private static String digest(Bitmap frame) {
        ByteBuffer buffer = ByteBuffer.allocate(frame.getByteCount());
        frame.copyPixelsToBuffer(buffer);
        long hash = 0xcbf29ce484222325L;
        for (byte b : buffer.array()) {
            hash ^= (b & 0xff);
            hash *= 0x100000001b3L;
        }
        return String.format("%016x", hash);
    }
}
