// SPDX-License-Identifier: MIT
// `xtask test window --stage native-activity`: a pure NativeActivity (no Java, no native_app_glue) standing in for a native game's entry point.
// Logs every ANativeActivityCallbacks call as `AtlWindow: native <callback>`, draws the window's quadrants with the CPU (ANativeWindow_lock / unlockAndPost) on each redraw request,
// and logs each AInputQueue motion event in the window fixture's touch format (window.rs touch_line), so the gate's checks read both fixtures alike.

#include <android/input.h>
#include <android/log.h>
#include <android/looper.h>
#include <android/native_activity.h>
#include <android/native_window.h>
#include <stdint.h>

#define TAG "AtlWindow"
#define LOG(...) __android_log_print(ANDROID_LOG_INFO, TAG, __VA_ARGS__)

// The looper ident the input queue is attached under (any value >= 0 with a callback).
#define INPUT_IDENT 1

// RGBA_8888 bytes in memory order as a little-endian word: the window fixture's quadrant colours (window.rs quadrant_pixels).
#define RED 0xff0000ffu
#define GREEN 0xff00ff00u
#define BLUE 0xffff0000u
#define WHITE 0xffffffffu

static void draw(ANativeWindow* window) {
    // the window's own size, RGBA_8888 (NativeActivity sets the window format to RGB_565; an app asks for what it draws)
    if (ANativeWindow_setBuffersGeometry(window, 0, 0, WINDOW_FORMAT_RGBA_8888) != 0) {
        LOG("native draw failed: setBuffersGeometry");
        return;
    }
    ANativeWindow_Buffer b;
    if (ANativeWindow_lock(window, &b, NULL) != 0) {
        LOG("native draw failed: lock");
        return;
    }
    int32_t half_w = b.width / 2, half_h = b.height / 2;
    for (int32_t y = 0; y < b.height; y++) {
        uint32_t* row = (uint32_t*)b.bits + (size_t)y * (size_t)b.stride;
        for (int32_t x = 0; x < b.width; x++) {
            row[x] = y < half_h ? (x < half_w ? RED : GREEN) : (x < half_w ? BLUE : WHITE);
        }
    }
    if (ANativeWindow_unlockAndPost(window) != 0) {
        LOG("native draw failed: unlockAndPost");
        return;
    }
    LOG("native drew size=%dx%d stride=%d format=%d", b.width, b.height, b.stride, b.format);
}

static int on_input(int fd, int events, void* data) {
    (void)fd;
    (void)events;
    AInputQueue* queue = data;
    AInputEvent* event = NULL;
    while (AInputQueue_getEvent(queue, &event) >= 0) {
        if (AInputQueue_preDispatchEvent(queue, event)) {
            continue;
        }
        int handled = 0;
        if (AInputEvent_getType(event) == AINPUT_EVENT_TYPE_MOTION) {
            LOG("touch action=%d x=%.1f y=%.1f pointers=%zu",
                AMotionEvent_getAction(event) & AMOTION_EVENT_ACTION_MASK,
                (double)AMotionEvent_getX(event, 0), (double)AMotionEvent_getY(event, 0),
                AMotionEvent_getPointerCount(event));
            handled = 1;
        }
        AInputQueue_finishEvent(queue, event, handled);
    }
    return 1;
}

static void on_start(ANativeActivity* a) { (void)a; LOG("native onStart"); }
static void on_resume(ANativeActivity* a) { (void)a; LOG("native onResume"); }
static void on_pause(ANativeActivity* a) { (void)a; LOG("native onPause"); }
static void on_stop(ANativeActivity* a) { (void)a; LOG("native onStop"); }
static void on_destroy(ANativeActivity* a) { (void)a; LOG("native onDestroy"); }

static void* on_save(ANativeActivity* a, size_t* size) {
    (void)a;
    LOG("native onSaveInstanceState");
    *size = 0;
    return NULL;
}

static void on_focus(ANativeActivity* a, int focused) {
    (void)a;
    LOG("native onWindowFocusChanged focused=%d", focused);
}

static void on_window_created(ANativeActivity* a, ANativeWindow* w) {
    (void)a;
    LOG("native onNativeWindowCreated size=%dx%d", ANativeWindow_getWidth(w), ANativeWindow_getHeight(w));
}

static void on_window_resized(ANativeActivity* a, ANativeWindow* w) {
    (void)a;
    LOG("native onNativeWindowResized size=%dx%d", ANativeWindow_getWidth(w), ANativeWindow_getHeight(w));
}

static void on_redraw(ANativeActivity* a, ANativeWindow* w) {
    (void)a;
    LOG("native onNativeWindowRedrawNeeded");
    draw(w);
}

static void on_window_destroyed(ANativeActivity* a, ANativeWindow* w) {
    (void)a;
    (void)w;
    LOG("native onNativeWindowDestroyed");
}

static void on_queue_created(ANativeActivity* a, AInputQueue* q) {
    (void)a;
    LOG("native onInputQueueCreated");
    // the callback's looper is the calling (main) thread's, as native_app_glue does it on its own thread
    AInputQueue_attachLooper(q, ALooper_forThread(), INPUT_IDENT, on_input, q);
}

static void on_queue_destroyed(ANativeActivity* a, AInputQueue* q) {
    (void)a;
    LOG("native onInputQueueDestroyed");
    AInputQueue_detachLooper(q);
}

static void on_content_rect(ANativeActivity* a, const ARect* r) {
    (void)a;
    LOG("native onContentRectChanged rect=%d,%d,%d,%d", r->left, r->top, r->right, r->bottom);
}

static void on_configuration(ANativeActivity* a) { (void)a; LOG("native onConfigurationChanged"); }
static void on_low_memory(ANativeActivity* a) { (void)a; LOG("native onLowMemory"); }

// The entry point NativeActivity looks up (android.app.func_name unset: the default name).
__attribute__((visibility("default"))) void ANativeActivity_onCreate(ANativeActivity* a, void* saved, size_t saved_size) {
    (void)saved;
    a->callbacks->onStart = on_start;
    a->callbacks->onResume = on_resume;
    a->callbacks->onSaveInstanceState = on_save;
    a->callbacks->onPause = on_pause;
    a->callbacks->onStop = on_stop;
    a->callbacks->onDestroy = on_destroy;
    a->callbacks->onWindowFocusChanged = on_focus;
    a->callbacks->onNativeWindowCreated = on_window_created;
    a->callbacks->onNativeWindowResized = on_window_resized;
    a->callbacks->onNativeWindowRedrawNeeded = on_redraw;
    a->callbacks->onNativeWindowDestroyed = on_window_destroyed;
    a->callbacks->onInputQueueCreated = on_queue_created;
    a->callbacks->onInputQueueDestroyed = on_queue_destroyed;
    a->callbacks->onContentRectChanged = on_content_rect;
    a->callbacks->onConfigurationChanged = on_configuration;
    a->callbacks->onLowMemory = on_low_memory;
    LOG("native onCreate sdk=%d saved=%zu", a->sdkVersion, saved_size);
}
