// SPDX-License-Identifier: MIT
// bridge-1 fixture, C side. Uses the bionic-only surface on purpose (errno through __errno, bionic pthread/dlopen, __thread) so a glibc host cannot satisfy it.
// Java side: xtask/testdata/app/java/atl/bridge1/testlib/AtlJniTest.java. C++ side: atl_jni_cxx.cpp. Built by xtask/src/jni_fixture.rs.
#include <android/asset_manager.h>
#include <android/asset_manager_jni.h>
#include <android/bitmap.h>
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

// After glesPbuffer: 1 when the process maps the host GL driver (a /proc/self/maps line naming "mesa", e.g. libEGL_mesa.so), 0 when not, -1 when maps cannot be read: the guest's GL reached the real driver, not a stand-in.
JNI_FN(jint, mesaMapped)(JNIEnv *env, jclass cls)
{
    FILE *f = fopen("/proc/self/maps", "re");
    if (f == NULL)
        return -1;
    char line[4096];
    jint found = 0;
    while (!found && fgets(line, sizeof(line), f) != NULL)
        found = strstr(line, "mesa") != NULL;
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
