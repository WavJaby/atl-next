// SPDX-License-Identifier: MIT
// bridge-1 fixture, C side. Uses the bionic-only surface on purpose (errno through __errno, bionic pthread/dlopen, __thread) so a glibc host cannot satisfy it.
// Java side: xtask/testdata/app/java/atl/bridge1/testlib/AtlJniTest.java. C++ side: atl_jni_cxx.cpp. Built by xtask/src/jni_fixture.rs.
#include <android/log.h>
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <jni.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/system_properties.h>
#include <unistd.h>

#define JNI_CLS "atl/bridge1/testlib/AtlJniTest"
#define JNI_FN(ret, name) JNIEXPORT ret JNICALL Java_atl_bridge1_testlib_AtlJniTest_##name

static __thread int tls_counter;

JNI_FN(jint, errnoRoundTrip)(JNIEnv *env, jclass cls)
{
    errno = 0;
    int fd = open("/atl-bridge1-no-such-path", O_RDONLY);
    if (fd >= 0) {
        close(fd);
        return -1;
    }
    return errno; // ENOENT = 2
}

// Guest liblog (AOSP's, built for the guest) -> /dev/socket/logdw; returns __android_log_print's 1, which it returns whether or not the line reached anyone, so the gate reads the line from the app log
JNI_FN(jint, guestLog)(JNIEnv *env, jclass cls)
{
    return __android_log_print(ANDROID_LOG_WARN, "AtlJniGuest", "guest liblog %d", 42);
}

// Property callbacks the host provider calls back into guest code: each must run on the caller's guest thread pointer, seen as the same errno address (bionic's errno lives in the thread's bionic TLS)
struct prop_check {
    int *errno_at;
    int calls;
    int wrong;
};

static void read_cb(void *cookie, const char *name, const char *value, uint32_t serial)
{
    struct prop_check *c = cookie;
    c->calls++;
    if (&errno != c->errno_at || strcmp(name, "ro.dalvik.vm.native.bridge") != 0)
        c->wrong++;
}

static void each_cb(const prop_info *pi, void *cookie)
{
    struct prop_check *c = cookie;
    c->calls++;
    if (&errno != c->errno_at)
        c->wrong++;
}

// 10 * (read_callback called once, on this thread) + (foreach called at least once, every call on this thread): 11 = both
JNI_FN(jint, propCallbacks)(JNIEnv *env, jclass cls)
{
    const prop_info *pi = __system_property_find("ro.dalvik.vm.native.bridge");
    if (pi == NULL)
        return -1;
    // API 26 (availability guard): a minSdk-21 build looks it up, as such an app does
    void (*read_callback)(const prop_info *, void (*)(void *, const char *, const char *, uint32_t), void *) =
        dlsym(RTLD_DEFAULT, "__system_property_read_callback");
    if (read_callback == NULL)
        return -2;
    struct prop_check r = {&errno, 0, 0};
    read_callback(pi, read_cb, &r);
    struct prop_check e = {&errno, 0, 0};
    __system_property_foreach(each_cb, &e);
    return 10 * (r.calls == 1 && r.wrong == 0) + (e.calls > 0 && e.wrong == 0);
}

JNI_FN(jint, tlsCounter)(JNIEnv *env, jclass cls)
{
    return ++tls_counter;
}

static void *sum_worker(void *arg)
{
    int n = *(int *)arg;
    intptr_t total = 0;
    for (int i = 1; i <= n; i++)
        total += i;
    tls_counter = (int)total; // per-thread copy: the caller's counter is untouched
    return (void *)total;
}

JNI_FN(jint, threadSum)(JNIEnv *env, jclass cls, jint n)
{
    pthread_t t;
    void *ret = NULL;
    int arg = n;
    if (pthread_create(&t, NULL, sum_worker, &arg) != 0)
        return -1;
    if (pthread_join(t, &ret) != 0)
        return -1;
    return (jint)(intptr_t)ret;
}

JNI_FN(jstring, greet)(JNIEnv *env, jclass cls, jstring name)
{
    const char *in = (*env)->GetStringUTFChars(env, name, NULL);
    if (in == NULL)
        return NULL;
    char buf[256];
    snprintf(buf, sizeof buf, "hello, %s", in);
    (*env)->ReleaseStringUTFChars(env, name, in);
    return (*env)->NewStringUTF(env, buf);
}

JNI_FN(jint, callJavaAdd)(JNIEnv *env, jclass cls, jint a, jint b)
{
    jmethodID mid = (*env)->GetStaticMethodID(env, cls, "javaAdd", "(II)I");
    if (mid == NULL)
        return -1;
    return (*env)->CallStaticIntMethod(env, cls, mid, a, b);
}

// Up-calls whose host target is JIT-compiled Java (AtlJniTest.jitUpCall warms it first): compiled code reads ART's Thread* through %gs.
JNI_FN(jlong, jitUpCallNative)(JNIEnv *env, jclass cls, jint calls)
{
    jmethodID mid = (*env)->GetStaticMethodID(env, cls, "jitWork", "(I)I");
    if (mid == NULL)
        return -1;
    jlong s = 0;
    for (jint i = 0; i < calls; i++)
        s += (*env)->CallStaticIntMethod(env, cls, mid, i & 7);
    return s;
}

JNI_FN(jint, dlopenSibling)(JNIEnv *env, jclass cls)
{
    void *h = dlopen("libatl-jni-sibling.so", RTLD_NOW | RTLD_LOCAL);
    if (h == NULL)
        return -1;
    int (*answer)(void) = (int (*)(void))dlsym(h, "atl_sibling_answer");
    int v = answer != NULL ? answer() : -2;
    dlclose(h);
    return v;
}

static sigjmp_buf segv_env;
static volatile sig_atomic_t segv_armed;

// Only the fault sigsegvRecover raises may reach it: anything else means the host's claim on SIGSEGV (ART's implicit null checks) was lost to this handler.
static void segv_handler(int sig, siginfo_t *info, void *uc)
{
    if (!segv_armed)
        abort();
    segv_armed = 0;
    siglongjmp(segv_env, 1);
}

// 1 = a guest SIGSEGV handler ran and siglongjmp'd back. The handler stays installed, so the Java null check after this runs with a guest handler claimed.
JNI_FN(jint, sigsegvRecover)(JNIEnv *env, jclass cls)
{
    struct sigaction sa = { 0 };
    sa.sa_sigaction = segv_handler;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, NULL) != 0)
        return -1;
    if (sigsetjmp(segv_env, 1) == 0) {
        segv_armed = 1;
        *(volatile int *)(uintptr_t)16 = 1;
        return -2;
    }
    return 1;
}

// Bound by RegisterNatives, so no exported Java_ symbol for this one.
static jint registeredAnswer(JNIEnv *env, jclass cls)
{
    return 7;
}

static const JNINativeMethod k_methods[] = {
    { "registeredAnswer", "()I", (void *)registeredAnswer },
};

JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM *vm, void *reserved)
{
    JNIEnv *env;
    if ((*vm)->GetEnv(vm, (void **)&env, JNI_VERSION_1_6) != JNI_OK)
        return JNI_ERR;
    jclass cls = (*env)->FindClass(env, JNI_CLS);
    if (cls == NULL)
        return JNI_ERR;
    if ((*env)->RegisterNatives(env, cls, k_methods, 1) != JNI_OK)
        return JNI_ERR;
    return JNI_VERSION_1_6;
}

// GLES2 through the GL proxies (libEGL and libGLESv2 are DT_NEEDED: the guest stubs bind to the host fronts). A 64x64 pbuffer: blue clear, red clear inside [8,56)^2 under a scissor, one green quad over [16,48)^2 (edges on pixel boundaries, so no partial pixels), read back.
// Returns the pixels that equal the scene's expectation (4096 = all), -1 on an EGL or GL error. The expectation is the geometry, not the driver's output.
#define GLES_W 64
#define GLES_H 64
static const char *gles_vs = "attribute vec2 pos;\nvoid main() { gl_Position = vec4(pos, 0.0, 1.0); }\n";
static const char *gles_fs = "precision mediump float;\nvoid main() { gl_FragColor = vec4(0.0, 1.0, 0.0, 1.0); }\n";

static GLuint gles_shader(GLenum type, const char *src)
{
    GLuint s = glCreateShader(type);
    glShaderSource(s, 1, &src, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    return ok ? s : 0;
}

JNI_FN(jint, glesPbuffer)(JNIEnv *env, jclass cls)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL))
        return -1;
    const EGLint cfg_attr[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                                EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE };
    EGLConfig cfg;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, cfg_attr, &cfg, 1, &n) || n < 1)
        return -1;
    const EGLint pb_attr[] = { EGL_WIDTH, GLES_W, EGL_HEIGHT, GLES_H, EGL_NONE };
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, pb_attr);
    const EGLint ctx_attr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attr);
    if (surf == EGL_NO_SURFACE || ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, surf, surf, ctx))
        return -1;

    glViewport(0, 0, GLES_W, GLES_H);
    glClearColor(0.0f, 0.0f, 1.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glEnable(GL_SCISSOR_TEST);
    glScissor(8, 8, 48, 48);
    glClearColor(1.0f, 0.0f, 0.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);

    static const GLfloat quad[] = { -0.5f, -0.5f, 0.5f, -0.5f, -0.5f, 0.5f, 0.5f, -0.5f, 0.5f, 0.5f, -0.5f, 0.5f };
    GLuint vs = gles_shader(GL_VERTEX_SHADER, gles_vs);
    GLuint fs = gles_shader(GL_FRAGMENT_SHADER, gles_fs);
    GLuint prog = glCreateProgram();
    glAttachShader(prog, vs);
    glAttachShader(prog, fs);
    glBindAttribLocation(prog, 0, "pos");
    glLinkProgram(prog);
    glUseProgram(prog);
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, quad);
    glEnableVertexAttribArray(0);
    glDrawArrays(GL_TRIANGLES, 0, 6);

    static unsigned char px[GLES_W * GLES_H * 4];
    glReadPixels(0, 0, GLES_W, GLES_H, GL_RGBA, GL_UNSIGNED_BYTE, px);
    if (glGetError() != GL_NO_ERROR)
        return -1;
    jint matches = 0;
    for (int y = 0; y < GLES_H; y++) {
        for (int x = 0; x < GLES_W; x++) {
            unsigned char want[4] = { 0, 0, 255, 255 };
            if (x >= 8 && x < 56 && y >= 8 && y < 56) {
                want[0] = 255;
                want[2] = 0;
            }
            if (x >= 16 && x < 48 && y >= 16 && y < 48) {
                want[0] = 0;
                want[1] = 255;
            }
            if (memcmp(&px[(y * GLES_W + x) * 4], want, 4) == 0)
                matches++;
        }
    }
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(dpy, ctx);
    eglDestroySurface(dpy, surf);
    eglTerminate(dpy);
    return matches;
}
