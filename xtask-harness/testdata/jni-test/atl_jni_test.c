// SPDX-License-Identifier: MIT
// bridge-1 fixture, C side. Uses the bionic-only surface on purpose (errno through __errno, bionic pthread/dlopen, __thread) so a glibc host cannot satisfy it.
// Java side: xtask-harness/testdata/app/java/atl/bridge1/testlib/AtlJniTest.java. C++ side: atl_jni_cxx.cpp. Built by xtask-harness/src/jni_fixture.rs.
#include <android/asset_manager.h>
#include <android/asset_manager_jni.h>
#include <android/bitmap.h>
#include <android/log.h>
#include <android/looper.h>
#include <android/native_window.h>
#include <android/native_window_jni.h>
#include <media/NdkMediaFormat.h>
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>
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

static pthread_key_t registered_key;

// Bound by RegisterNatives, so no exported Java_ symbol for this one.
// Crosses up: entered without ART's trampoline it runs on the host thread pointer, where a same-ISA guest's pure `return` still works and only a crossing finds no record.
static jint registeredAnswer(JNIEnv *env, jclass cls)
{
    return pthread_getspecific(registered_key) == NULL ? 7 : -1;
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
    if (pthread_key_create(&registered_key, NULL) != 0)
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

// GLSurfaceView's ComponentSizeChooser: the first listed config with RGBA exactly 8 each. EGL's *_SIZE attributes are
// minimums, so the list leads with 16-bit configs, which read back zeros on the test host.
static EGLConfig exact_8888_config(EGLDisplay dpy, const EGLint *attr)
{
    EGLConfig all[64];
    EGLint n = 0;
    if (!eglChooseConfig(dpy, attr, all, 64, &n))
        return NULL;
    if (n > 64)
        n = 64;
    for (EGLint i = 0; i < n; i++) {
        EGLint r = 0, g = 0, b = 0, a = 0;
        eglGetConfigAttrib(dpy, all[i], EGL_RED_SIZE, &r);
        eglGetConfigAttrib(dpy, all[i], EGL_GREEN_SIZE, &g);
        eglGetConfigAttrib(dpy, all[i], EGL_BLUE_SIZE, &b);
        eglGetConfigAttrib(dpy, all[i], EGL_ALPHA_SIZE, &a);
        if (r == 8 && g == 8 && b == 8 && a == 8)
            return all[i];
    }
    return NULL;
}

// The first config eglChooseConfig lists for RGB 8 minimum on pbuffer surfaces: its sizes and EGL_NATIVE_VISUAL_ID (the HAL format the driver maps it to), `r=.. g=.. b=.. a=.. visual=..`.
// app.rs judges the visual against PixelFormat.aidl's list; the line carries the sizes for the record. The window default (no EGL_SURFACE_TYPE) leaves out the pbuffer-only 16-bit configs, so it cannot show them first.
JNI_FN(jstring, firstConfig)(JNIEnv *env, jclass cls)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL))
        return (*env)->NewStringUTF(env, "no display");
    const EGLint attr[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    EGLConfig first = NULL;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, attr, &first, 1, &n) || n == 0)
        return (*env)->NewStringUTF(env, "no config");
    EGLint r = 0, g = 0, b = 0, a = 0, visual = 0;
    eglGetConfigAttrib(dpy, first, EGL_RED_SIZE, &r);
    eglGetConfigAttrib(dpy, first, EGL_GREEN_SIZE, &g);
    eglGetConfigAttrib(dpy, first, EGL_BLUE_SIZE, &b);
    eglGetConfigAttrib(dpy, first, EGL_ALPHA_SIZE, &a);
    eglGetConfigAttrib(dpy, first, EGL_NATIVE_VISUAL_ID, &visual);
    char out[96];
    snprintf(out, sizeof out, "r=%d g=%d b=%d a=%d visual=%d", r, g, b, a, visual);
    return (*env)->NewStringUTF(env, out);
}

JNI_FN(jint, glesPbuffer)(JNIEnv *env, jclass cls)
{
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL))
        return -1;
    const EGLint cfg_attr[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                                EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_ALPHA_SIZE, 8, EGL_NONE };
    EGLConfig cfg = exact_8888_config(dpy, cfg_attr);
    if (cfg == NULL)
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

// After glesPbuffer: 1 when the process maps libGLES_atl.so (a /proc/self/maps line naming it), 0 when not, -1 when maps cannot be read: the guest's GL reached the driver, not a stand-in.
JNI_FN(jint, driverMapped)(JNIEnv *env, jclass cls)
{
    FILE *f = fopen("/proc/self/maps", "re");
    if (f == NULL)
        return -1;
    char line[4096];
    jint found = 0;
    while (!found && fgets(line, sizeof(line), f) != NULL)
        found = strstr(line, "libGLES_atl.so") != NULL;
    fclose(f);
    return found;
}

/* C3: libandroid's asset API and libjnigraphics' AndroidBitmap, through the guest. app.rs writes the assets (stored.bin, stored by `aapt2 -0 bin`; deflated.txt) and checks the SHA-256 values it computes over the same bytes. */
#define ASSET_STORED "stored.bin"
#define ASSET_DEFLATED "deflated.txt"
#define ASSET_SEEK 1000
#ifdef ATL_JNI_SEEK_BREAK
/* deliberate break (xtask `ATL_JNI_BREAK=seek`): one byte off, so the tail's hash and remaining length go red */
#define ASSET_SEEK_TO (ASSET_SEEK + 1)
#else
#define ASSET_SEEK_TO ASSET_SEEK
#endif

typedef struct {
    uint32_t h[8];
    uint64_t bits;
    unsigned char block[64];
    size_t fill;
} sha256_ctx;

static const uint32_t sha256_k[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
};

static uint32_t sha256_rotr(uint32_t x, unsigned n)
{
    return (x >> n) | (x << (32 - n));
}

static void sha256_init(sha256_ctx *c)
{
    static const uint32_t h0[8] = {
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    };
    memcpy(c->h, h0, sizeof c->h);
    c->bits = 0;
    c->fill = 0;
}

static void sha256_block(sha256_ctx *c, const unsigned char *p)
{
    uint32_t w[64];
    for (int i = 0; i < 16; i++)
        w[i] = ((uint32_t)p[4 * i] << 24) | ((uint32_t)p[4 * i + 1] << 16) | ((uint32_t)p[4 * i + 2] << 8) | p[4 * i + 3];
    for (int i = 16; i < 64; i++) {
        uint32_t s0 = sha256_rotr(w[i - 15], 7) ^ sha256_rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
        uint32_t s1 = sha256_rotr(w[i - 2], 17) ^ sha256_rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    uint32_t a = c->h[0], b = c->h[1], cc = c->h[2], d = c->h[3];
    uint32_t e = c->h[4], f = c->h[5], g = c->h[6], hh = c->h[7];
    for (int i = 0; i < 64; i++) {
        uint32_t s1 = sha256_rotr(e, 6) ^ sha256_rotr(e, 11) ^ sha256_rotr(e, 25);
        uint32_t ch = (e & f) ^ (~e & g);
        uint32_t t1 = hh + s1 + ch + sha256_k[i] + w[i];
        uint32_t s0 = sha256_rotr(a, 2) ^ sha256_rotr(a, 13) ^ sha256_rotr(a, 22);
        uint32_t maj = (a & b) ^ (a & cc) ^ (b & cc);
        uint32_t t2 = s0 + maj;
        hh = g;
        g = f;
        f = e;
        e = d + t1;
        d = cc;
        cc = b;
        b = a;
        a = t1 + t2;
    }
    c->h[0] += a;
    c->h[1] += b;
    c->h[2] += cc;
    c->h[3] += d;
    c->h[4] += e;
    c->h[5] += f;
    c->h[6] += g;
    c->h[7] += hh;
}

static void sha256_update(sha256_ctx *c, const unsigned char *p, size_t n)
{
    c->bits += (uint64_t)n * 8;
    while (n > 0) {
        size_t take = 64 - c->fill;
        if (take > n)
            take = n;
        memcpy(c->block + c->fill, p, take);
        c->fill += take;
        p += take;
        n -= take;
        if (c->fill == 64) {
            sha256_block(c, c->block);
            c->fill = 0;
        }
    }
}

/* out: 64 hex digits and NUL */
static void sha256_final_hex(sha256_ctx *c, char out[65])
{
    static const unsigned char pad[64] = { 0x80 };
    uint64_t bits = c->bits;
    unsigned char len[8];
    for (int i = 0; i < 8; i++)
        len[i] = (unsigned char)(bits >> (56 - 8 * i));
    sha256_update(c, pad, c->fill < 56 ? 56 - c->fill : 120 - c->fill);
    sha256_update(c, len, 8);
    for (int i = 0; i < 8; i++)
        snprintf(out + 8 * i, 9, "%08x", c->h[i]);
}

static void sha256_hex_of(const unsigned char *p, size_t n, char out[65])
{
    sha256_ctx c;
    sha256_init(&c);
    sha256_update(&c, p, n);
    sha256_final_hex(&c, out);
}

/* Streams an asset to its end in 4096-byte AAsset_read calls: its SHA-256 into out, the bytes read (-1 on a read error). */
static long long asset_hash(AAsset *a, char out[65])
{
    sha256_ctx c;
    unsigned char buf[4096];
    long long total = 0;
    sha256_init(&c);
    for (;;) {
        int n = AAsset_read(a, buf, sizeof buf);
        if (n < 0)
            return -1;
        if (n == 0)
            break;
        sha256_update(&c, buf, (size_t)n);
        total += n;
    }
    sha256_final_hex(&c, out);
    return total;
}

// AtlJniTest.assetProbe: the stored and deflated assets streamed (their SHA-256), the tail after a seek to ASSET_SEEK (remaining length, SHA-256), AAsset_getBuffer of the stored one (SHA-256), and AAssetManager_open of a missing name (NULL).
JNI_FN(jstring, assetProbe)(JNIEnv *env, jclass cls, jobject assetManager)
{
    AAssetManager *mgr = AAssetManager_fromJava(env, assetManager);
    if (mgr == NULL)
        return (*env)->NewStringUTF(env, "no AAssetManager");
    char stored[65] = "null", deflated[65] = "null", tail[65] = "null", buffer[65] = "null";
    char missing[16] = "null";
    long long remaining = -1;
    AAsset *a = AAssetManager_open(mgr, ASSET_STORED, AASSET_MODE_STREAMING);
    if (a != NULL) {
        asset_hash(a, stored);
        AAsset_close(a);
    }
    a = AAssetManager_open(mgr, ASSET_DEFLATED, AASSET_MODE_STREAMING);
    if (a != NULL) {
        asset_hash(a, deflated);
        AAsset_close(a);
    }
    a = AAssetManager_open(mgr, ASSET_STORED, AASSET_MODE_STREAMING);
    if (a != NULL) {
        AAsset_seek(a, ASSET_SEEK_TO, SEEK_SET);
        remaining = AAsset_getRemainingLength(a);
        asset_hash(a, tail);
        AAsset_close(a);
    }
    a = AAssetManager_open(mgr, ASSET_STORED, AASSET_MODE_BUFFER);
    if (a != NULL) {
        const unsigned char *p = AAsset_getBuffer(a);
        if (p != NULL)
            sha256_hex_of(p, (size_t)AAsset_getLength(a), buffer);
        AAsset_close(a);
    }
    a = AAssetManager_open(mgr, "missing.bin", AASSET_MODE_BUFFER);
    snprintf(missing, sizeof missing, "%s", a == NULL ? "null" : "found");
    if (a != NULL)
        AAsset_close(a);
    char line[512];
    snprintf(line, sizeof line, "stream %s deflated %s seek %lld %s buffer %s missing %s", stored, deflated, remaining, tail, buffer, missing);
    return (*env)->NewStringUTF(env, line);
}

// AtlJniTest.bitmapProbe: AndroidBitmap_getInfo and the locked pixel bytes of a bitmap, as "info <width> <height> <stride> <format> <flags> bytes <hex>".
JNI_FN(jstring, bitmapProbe)(JNIEnv *env, jclass cls, jobject bitmap)
{
    AndroidBitmapInfo info;
    if (AndroidBitmap_getInfo(env, bitmap, &info) != ANDROID_BITMAP_RESULT_SUCCESS)
        return (*env)->NewStringUTF(env, "getInfo failed");
    size_t n = (size_t)info.stride * info.height;
    if (n > 64)
        return (*env)->NewStringUTF(env, "too big");
    void *px = NULL;
    if (AndroidBitmap_lockPixels(env, bitmap, &px) != ANDROID_BITMAP_RESULT_SUCCESS)
        return (*env)->NewStringUTF(env, "lockPixels failed");
    char hex[2 * 64 + 1] = "";
    const unsigned char *p = px;
    for (size_t i = 0; i < n; i++)
        snprintf(hex + 2 * i, 3, "%02x", p[i]);
    AndroidBitmap_unlockPixels(env, bitmap);
    char line[256];
    snprintf(line, sizeof line, "info %u %u %u %d %u bytes %s", info.width, info.height, info.stride, info.format, info.flags, hex);
    return (*env)->NewStringUTF(env, line);
}

// AtlJniTest.windowProbe (tr-first blockers 2, 3): ANativeWindow_fromSurface (an env-class row: its JNIEnv* mapped to the host's), then rows of native_window.h bound generically, as "window <width> <height> <format>".
JNI_FN(jstring, windowProbe)(JNIEnv *env, jclass cls, jobject surface)
{
    ANativeWindow *w = ANativeWindow_fromSurface(env, surface);
    if (w == NULL)
        return (*env)->NewStringUTF(env, "fromSurface null");
    int32_t width = ANativeWindow_getWidth(w);
    int32_t height = ANativeWindow_getHeight(w);
    int32_t format = ANativeWindow_getFormat(w);
    ANativeWindow_release(w);
    char line[64];
    snprintf(line, sizeof line, "window %d %d %d", width, height, format);
    return (*env)->NewStringUTF(env, line);
}

// tr-first blocker 5 (GS2.2 G2, callback-param): ALooper_addFd's callback is a guest function the host libandroid calls; the bridge hands it a down shell (engine/ndk.rs `call_mapped`). One record per case; the user data is the record's address, so a lost or swapped pointer counts in g_cb_bad instead.
struct cb_rec {
    int calls;
    pid_t tid;
    int wfd;
};
static struct cb_rec g_cb_now = {0, 0, -1}, g_cb_later = {0, 0, -1};
static int g_cb_bad;
static ALooper *g_main_looper;
static pid_t g_main_tid, g_arm_tid;

static int looper_cb(int fd, int events, void *data)
{
    struct cb_rec *r = data == &g_cb_now ? &g_cb_now : data == &g_cb_later ? &g_cb_later : NULL;
    char b;
    read(fd, &b, 1);
    if (r == NULL || events != ALOOPER_EVENT_INPUT) {
        g_cb_bad++;
    } else {
        r->calls++;
        r->tid = gettid();
        close(r->wfd);
    }
    close(fd);
    return 0; // unregister
}

// AtlJniTest.looperNow, on the main thread: (a) the host calls the callback during the call, on the calling thread, inside ALooper_pollOnce. "now <calls> <same thread> <pollOnce result> <bad>"; also keeps the main looper for looperArm.
JNI_FN(jstring, looperNow)(JNIEnv *env, jclass cls)
{
    g_main_looper = ALooper_forThread();
    g_main_tid = gettid();
    int p[2];
    if (g_main_looper == NULL || pipe(p) != 0)
        return (*env)->NewStringUTF(env, "no looper or pipe");
    g_cb_now.wfd = p[1];
    ALooper_addFd(g_main_looper, p[0], ALOOPER_POLL_CALLBACK, ALOOPER_EVENT_INPUT, looper_cb, &g_cb_now);
    write(p[1], "x", 1);
    int r = ALooper_pollOnce(1000, NULL, NULL, NULL);
    char line[64];
    snprintf(line, sizeof line, "now %d %d %d %d", g_cb_now.calls, g_cb_now.tid == g_main_tid, r, g_cb_bad);
    return (*env)->NewStringUTF(env, line);
}

// AtlJniTest.looperArm, on a fresh Java thread: registers on the main looper and makes the fd ready, then returns; the main thread's Java Looper calls the callback later, outside this call (looperLater reads it).
JNI_FN(jstring, looperArm)(JNIEnv *env, jclass cls)
{
    g_arm_tid = gettid();
    int p[2];
    if (g_main_looper == NULL || pipe(p) != 0)
        return (*env)->NewStringUTF(env, "no looper or pipe");
    g_cb_later.wfd = p[1];
    ALooper_addFd(g_main_looper, p[0], ALOOPER_POLL_CALLBACK, ALOOPER_EVENT_INPUT, looper_cb, &g_cb_later);
    write(p[1], "x", 1);
    return (*env)->NewStringUTF(env, "armed");
}

// AtlJniTest.looperLater, on the main thread after its Looper ran: (b) "later <calls> <ran on main> <main is not the arming thread> <bad>".
JNI_FN(jstring, looperLater)(JNIEnv *env, jclass cls)
{
    char line[64];
    snprintf(line, sizeof line, "later %d %d %d %d", g_cb_later.calls, g_cb_later.tid == g_main_tid, g_arm_tid != g_main_tid, g_cb_bad);
    return (*env)->NewStringUTF(env, line);
}

// AtlJniTest.mediandkLinked (G6b): this library has DT_NEEDED libmediandk.so, as every Unity libunity.so does, so it loads only when the guest finds that proxy; 1 when its AMediaFormat_new resolved.
JNI_FN(jint, mediandkLinked)(JNIEnv *env, jclass cls)
{
    AMediaFormat *(*volatile f)(void) = AMediaFormat_new;
    return f != NULL;
}

// AtlJniTest.resolverProbe (net-bp, engine/netdb.rs): the libc resolver family through the bridge, every answer read in bionic's layout and values. Fields: numeric 1 (AF_INET 127.0.0.1:80, one node, no canonical name); passive 1 (AI_PASSIVE null host, 0.0.0.0:8080); service = EAI_SERVICE (9); numerichost = EAI_NONAME (8, AI_NUMERICHOST on a name); text 1 (gai_strerror of EAI_NODATA and EAI_NONAME is bionic's); loop 1000 (1000 numeric lookups, each read back and freed); list ok (AF_UNSPEC passive list walked and freed, its length host-dependent, not printed); herr 1 (h_errno set to 77, then a failed gethostbyname of the unresolvable name leaves HOST_NOT_FOUND or TRY_AGAIN in it; a name that resolves here reads 0, so a DNS that answers every name fails this field). The getaddrinfo answer for the unresolvable name is logged, not gated: its code depends on the sandbox network.
static int rp_numeric(void)
{
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo("127.0.0.1", "80", &hints, &res) != 0 || res == NULL)
        return 0;
    const struct sockaddr_in *sin = (const struct sockaddr_in *)res->ai_addr;
    int ok = res->ai_family == AF_INET && res->ai_socktype == SOCK_STREAM && res->ai_addrlen == sizeof(struct sockaddr_in) && sin != NULL && sin->sin_family == AF_INET && sin->sin_port == htons(80) && sin->sin_addr.s_addr == htonl(INADDR_LOOPBACK) && res->ai_next == NULL && res->ai_canonname == NULL;
    freeaddrinfo(res);
    return ok;
}

static int rp_passive(void)
{
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_flags = AI_PASSIVE;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(NULL, "8080", &hints, &res) != 0 || res == NULL)
        return 0;
    const struct sockaddr_in *sin = (const struct sockaddr_in *)res->ai_addr;
    int ok = sin->sin_addr.s_addr == htonl(INADDR_ANY) && sin->sin_port == htons(8080);
    freeaddrinfo(res);
    return ok;
}

static int rp_service(void)
{
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    int rc = getaddrinfo("127.0.0.1", "nosuchservice", &hints, &res);
    if (rc == 0)
        freeaddrinfo(res);
    return rc;
}

static int rp_numerichost(void)
{
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_flags = AI_NUMERICHOST;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    int rc = getaddrinfo("nonexistent.invalid", "80", &hints, &res);
    if (rc == 0)
        freeaddrinfo(res);
    return rc;
}

static int rp_text(void)
{
    return strcmp(gai_strerror(EAI_NODATA), "No address associated with hostname") == 0 && strcmp(gai_strerror(EAI_NONAME), "hostname nor servname provided, or not known") == 0;
}

static int rp_loop(void)
{
    for (int i = 0; i < 1000; i++) {
        struct addrinfo hints = {0}, *res = NULL;
        hints.ai_family = AF_INET;
        hints.ai_socktype = SOCK_STREAM;
        if (getaddrinfo("127.0.0.1", "80", &hints, &res) != 0 || res == NULL)
            return 0;
        int ok = ((const struct sockaddr_in *)res->ai_addr)->sin_port == htons(80);
        freeaddrinfo(res);
        if (!ok)
            return 0;
    }
    return 1000;
}

static int rp_list(void)
{
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_flags = AI_PASSIVE;
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(NULL, "80", &hints, &res) != 0 || res == NULL)
        return 0;
    int ok = 1, n = 0;
    for (struct addrinfo *a = res; a != NULL; a = a->ai_next) {
        n++;
        if (a->ai_addr == NULL || (a->ai_family != AF_INET && a->ai_family != AF_INET6))
            ok = 0;
        else if (a->ai_family == AF_INET && ((const struct sockaddr_in *)a->ai_addr)->sin_port != htons(80))
            ok = 0;
    }
    freeaddrinfo(res);
    return ok && n > 0;
}

// net-bp (build.rs `GLIBC` `__get_h_errno`): the guest's h_errno is the slot the host's gethostbyname sets. 77 is a sentinel no lookup writes, so 1 or 2 is the failed lookup's own value.
static int rp_herrno(void)
{
    h_errno = 77;
    struct hostent *he = gethostbyname("nonexistent.invalid");
    __android_log_print(ANDROID_LOG_INFO, "atl-resolver", "gethostbyname %s h_errno=%d", he == NULL ? "failed" : "answered", h_errno);
    return he == NULL && (h_errno == HOST_NOT_FOUND || h_errno == TRY_AGAIN);
}

static void rp_unresolved(void)
{
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    int rc = getaddrinfo("nonexistent.invalid", "80", &hints, &res);
    __android_log_print(ANDROID_LOG_INFO, "atl-resolver", "unresolved rc=%d text=%s", rc, rc != 0 ? gai_strerror(rc) : "(success)");
    if (rc == 0)
        freeaddrinfo(res);
}

JNI_FN(jstring, resolverProbe)(JNIEnv *env, jclass cls)
{
    char line[256];
    rp_unresolved();
    snprintf(line, sizeof line, "numeric %d passive %d service %d numerichost %d text %d loop %d list %s herr %d",
             rp_numeric(), rp_passive(), rp_service(), rp_numerichost(), rp_text(), rp_loop(), rp_list() ? "ok" : "fail", rp_herrno());
    return (*env)->NewStringUTF(env, line);
}
