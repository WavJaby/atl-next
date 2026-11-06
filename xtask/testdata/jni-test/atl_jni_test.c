// SPDX-License-Identifier: MIT
// bridge-1 fixture, C side. Uses the bionic-only surface on purpose (errno through __errno, bionic pthread/dlopen, __thread) so a glibc host cannot satisfy it.
// Java side: xtask/testdata/app/java/atl/bridge1/testlib/AtlJniTest.java. C++ side: atl_jni_cxx.cpp. Built by xtask/src/jni_fixture.rs.
#include <android/log.h>
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
