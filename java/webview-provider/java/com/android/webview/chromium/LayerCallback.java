// SPDX-License-Identifier: MIT
package com.android.webview.chromium;

import android.view.SurfaceHolder;

/**
 * The SurfaceHolder.Callback of the SurfaceView that carries one WebView's layer (webview plan S2.2 amended). A forwarder (AGENTS Languages): surfaceCreated hands the new Surface to the
 * natives, which set the view's sideband stream on it; the other two have nothing to forward (the layer follows the SurfaceView, the stream goes with the Surface).
 */
final class LayerCallback implements SurfaceHolder.Callback {
    // read by the natives: whose view the stream shows
    private final WebViewProviderImpl mProvider;

    // made by the native init
    private LayerCallback(WebViewProviderImpl provider) {
        mProvider = provider;
    }

    @Override public native void surfaceCreated(SurfaceHolder holder);
    @Override public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {}
    @Override public void surfaceDestroyed(SurfaceHolder holder) {}
}
