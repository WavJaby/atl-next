// SPDX-License-Identifier: MIT
// bridge-1 fixture, C side. Uses the bionic-only surface on purpose (errno through __errno, bionic pthread/dlopen, __thread) so a glibc host cannot satisfy it.
// Java side: xtask/testdata/app/java/atl/bridge1/testlib/AtlJniTest.java. C++ side: atl_jni_cxx.cpp. Built by xtask/src/jni_fixture.rs.
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <jni.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
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
