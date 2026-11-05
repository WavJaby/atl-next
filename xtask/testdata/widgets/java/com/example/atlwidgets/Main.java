// SPDX-License-Identifier: MIT
package com.example.atlwidgets;

import android.app.Activity;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Rect;
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
 * The widget gate: `xtask test window --stage widgets` (xtask/src/window.rs) reads the lines below from the app log.
 * Layout: each widget's rectangle in the content (`layout`), the density, and the software frame (`decor` drawn into a bitmap at first layout):
 * its FNV-1a 64 digest must equal the composer's readback at the capture frame, and the text pixels inside the TextView and EditText are non-background (`pixels`).
 * Click: the Button's OnClickListener changes the TextView text, and the next layout differs inside the TextView (`changed`).
 * Focus: a touch on the EditText focuses it. List: a drag scrolls it (`list first`). The frame callbacks are logged as in the window fixture (`frame`).
 */
public class Main extends Activity {
    static final String TAG = "AtlWindow";
    /** Frame callbacks per run (xtask/src/window.rs FRAME_CALLBACKS). */
    static final int FRAME_CALLBACKS = 31;

    private int frameCallbacks;
    private int clicks;
    private boolean laidOut;
    private TextView label;
    private Button button;
    private EditText input;
    private ListView list;
    private Bitmap first;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i(TAG, "onCreate reached");
        setContentView(R.layout.main);
        label = (TextView) findViewById(R.id.label);
        button = (Button) findViewById(R.id.button);
        input = (EditText) findViewById(R.id.input);
        list = (ListView) findViewById(R.id.list);
        // setText from code: the EditText must render it
        input.setText("typed from code");
        String[] items = new String[50];
        for (int i = 0; i < items.length; i++) {
            items[i] = "Item " + i;
        }
        list.setAdapter(new ArrayAdapter<String>(this, android.R.layout.simple_list_item_1, items));
        list.setOnScrollListener(new AbsListView.OnScrollListener() {
            @Override
            public void onScrollStateChanged(AbsListView view, int scrollState) {
            }

            @Override
            public void onScroll(AbsListView view, int firstVisibleItem, int visibleItemCount, int totalItemCount) {
                Log.i(TAG, "list first=" + firstVisibleItem);
            }
        });
        input.setOnFocusChangeListener(new View.OnFocusChangeListener() {
            @Override
            public void onFocusChange(View v, boolean hasFocus) {
                Log.i(TAG, "focus input=" + hasFocus);
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
        postFrame();
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

    /** Once: the geometry, then the software frame (the expectation of the readback) and the ink inside the text fields. */
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
        first = render();
        Log.i(TAG, "software frame fnv1a64=" + digest(first) + " size=" + first.getWidth() + "x" + first.getHeight());
        Log.i(TAG, "pixels label=" + ink(first, boundsOf(label)) + " input=" + ink(first, boundsOf(input)));
    }

    private void onButton() {
        clicks++;
        label.setText("Clicked " + clicks);
        Log.i(TAG, "click count=" + clicks);
        // the next layout carries the new text: render it then
        final ViewTreeObserver observer = label.getViewTreeObserver();
        observer.addOnGlobalLayoutListener(new ViewTreeObserver.OnGlobalLayoutListener() {
            @Override
            public void onGlobalLayout() {
                observer.removeOnGlobalLayoutListener(this);
                Bitmap after = render();
                Log.i(TAG, "changed label=" + changed(first, after, boundsOf(label)));
                Log.i(TAG, "text label=" + label.getText());
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

    /** FNV-1a 64 over the bitmap's bytes, the composer's digest (xtask/src/window.rs fnv1a64 of the readback). */
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
