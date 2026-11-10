// SPDX-License-Identifier: MIT
// Guest side of the JNI gate (xtask `test qemu-jni`, QEMU bridge plan Q6-2 and Q6-3): a bionic PIE executable that the guest linker loads; the host calls qj_run with a guest env (tests/qemu_jni.rs), and each JNI call below crosses to the fake host JNIEnv there.
// Calls no libc symbol: only the stub libc's intercepted stubs resolve in the guest (see qemu-thread's fixture.c).
// Results go to out[] (one per check in the gate), not a packed return, so each check names its own value.
#include <jni.h>
#include <stdarg.h>
#include <stdint.h>

// the fake host's CallStaticIntMethod answers a * 10 + b; a swapped pair shows as 32
#define ADD_A 2
#define ADD_B 3
// the object and field the fake's SetDoubleField expects
#define OBJ 0xb7e
#define FIELD 0xf1d

// refuse: the third argument of qj_run; each value calls a JNI entry the host refuses by name (dispatch.rs `refuse`)
#define REFUSE_NONE 0
#define REFUSE_RESERVED 1
#define REFUSE_VALIST 2
#define REFUSE_FORGED 3

// the first variadic argument of CallStaticIntMethodV is a va_list: the call is refused before it is read
static void call_valist(JNIEnv *env, ...) {
    va_list ap;
    va_start(ap, env);
    (*env)->CallStaticIntMethodV(env, NULL, NULL, ap);
    va_end(ap);
}

// out[8] (in) the pool address of the snprintf binding, out[9] (in) its buffer, out[11] (in) the buffer size, out[10] (out) snprintf's result: the `p` row.
// out[0] GetVersion, out[1] CallStaticIntMethod(ADD_A, ADD_B),out[2] GetJavaVM's result, out[3] the vm it gave, out[4] 1 when NewStringUTF("abc") reads back "abc",
// out[5] the bits of CallStaticDoubleMethod's result, out[6] the bits of CallStaticFloatMethod's result, out[7] (refuse == REFUSE_FORGED) the address of a JNI stub past the table
void qj_run(JNIEnv *env, long long *out, long long refuse) {
    if (refuse == REFUSE_RESERVED) {
        // jni.h reserved3: a `void *` field, so the call goes through a cast
        ((void (*)(JNIEnv *))(*env)->reserved3)(env);
    }
    if (refuse == REFUSE_VALIST) {
        call_valist(env);
    }
    if (refuse == REFUSE_FORGED) {
        ((void (*)(JNIEnv *))(intptr_t)out[7])(env);
    }
    out[0] = (*env)->GetVersion(env);
    jclass cls = (*env)->FindClass(env, "atl/Adder");
    jmethodID mid = (*env)->GetStaticMethodID(env, cls, "add", "(II)I");
    out[1] = (*env)->CallStaticIntMethod(env, cls, mid, ADD_A, ADD_B);
    JavaVM *vm = NULL;
    out[2] = (*env)->GetJavaVM(env, &vm);
    out[3] = (long long)(intptr_t)vm;
    jstring s = (*env)->NewStringUTF(env, "abc");
    const char *chars = (*env)->GetStringUTFChars(env, s, NULL);
    out[4] = chars != NULL && chars[0] == 'a' && chars[1] == 'b' && chars[2] == 'c' && chars[3] == 0;
    (*env)->ReleaseStringUTFChars(env, s, chars);
    // Q6-3: a fixed fp parameter; a variadic call whose integers 8, 9, 10 spill to the stack after the eight integer registers, with fp arguments 2.25 and 7.5 in v0 and v1; an fp result; an fp result of a float
    (*env)->SetDoubleField(env, (jobject)(intptr_t)OBJ, (jfieldID)(intptr_t)FIELD, 2.5);
    jdouble d = (*env)->CallStaticDoubleMethod(env, cls, mid, 1, 2.25, 3, 4, 5, 6, 7.5, 8, 9, 10);
    union {
        jdouble d;
        long long i;
    } du;
    du.d = d;
    out[5] = du.i;
    jfloat f = (*env)->CallStaticFloatMethod(env, cls, mid, 7);
    union {
        jfloat f;
        int i;
    } fu;
    fu.f = f;
    out[6] = fu.i;
    // Q6-3 part 2b, the snprintf row (p): glibc's snprintf through the binding the host made (out[8]); its buffer (out[9]) and size (out[11]) are host memory the host call writes; the double vararg needs the host call's %al
    out[10] = ((int (*)(char *, unsigned long, const char *, ...))(intptr_t)out[8])((char *)(intptr_t)out[9], (unsigned long)out[11], "%d %.2f", 7, 2.5);
    // the atof row (a): glibc's atof through the binding the host made with result class Fp (out[12]); its argument (out[13]) is host memory; the double comes back in v0
    double a = ((double (*)(const char *))(intptr_t)out[12])((const char *)(intptr_t)out[13]);
    union {
        double d;
        long long i;
    } au;
    au.d = a;
    out[14] = au.i;
}

// the executable needs a main (as qemu-thread's fixture has one); the guest linker starts it, the host calls qj_run
int main(void) {
    return 0;
}
