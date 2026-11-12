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

// Q6-4: the down calls (tests/qemu_jni.rs `started`) run JNI_OnLoad and two native methods here; each records what it was given, and qj_run copies the record to out[15..20]
static long long qj_seen[6];

// refuse: the third argument of qj_run; each value calls a JNI entry the host refuses by name (dispatch.rs `refuse`)
#define REFUSE_NONE 0
#define REFUSE_RESERVED 1
#define REFUSE_VALIST 2
#define REFUSE_FORGED 3

// REFUSE_VALIST: the gate gave the engine no shorty source in this case, so the V call is refused before its va_list is read
static void call_valist(JNIEnv *env, ...) {
    va_list ap;
    va_start(ap, env);
    (*env)->CallStaticIntMethodV(env, NULL, NULL, ap);
    va_end(ap);
}

// Q6-7, the `...V` calls (check V): each helper takes the method's arguments variadically and passes its va_list on, as an app's JNI wrapper does.
// V_ARGS matches the fake's shorty for V_MID, "IDJFLIDJFIDJFIDJFLD" (tests/qemu_jni.rs V_SHORTY): 10 integer-class arguments, past the five or four x registers each helper has left, and 9 fp ones (floats promoted to double), past v0..v7, so the va_list's __stack is read for both.
// The fake's `...A` slots compare each jvalue with tests/qemu_jni.rs V_* and answer that slot's V_RET_*.
#define V_MID 0x7a11
#define V_ARGS 11, 1.25, 0x100000002LL, 0.5f, (jobject)(intptr_t)0xb01, -12, 2.5, -3LL, 1.75f, 13, 3.75, 0x700000004LL, 2.25f, 14, 4.5, 5LL, 3.125f, (jobject)(intptr_t)0xb02, 5.875

static jdouble v_static(JNIEnv *env, jclass cls, jmethodID mid, ...) {
    va_list ap;
    va_start(ap, mid);
    jdouble r = (*env)->CallStaticDoubleMethodV(env, cls, mid, ap);
    va_end(ap);
    return r;
}

static jlong v_virtual(JNIEnv *env, jobject obj, jmethodID mid, ...) {
    va_list ap;
    va_start(ap, mid);
    jlong r = (*env)->CallLongMethodV(env, obj, mid, ap);
    va_end(ap);
    return r;
}

static jint v_nonvirtual(JNIEnv *env, jobject obj, jclass cls, jmethodID mid, ...) {
    va_list ap;
    va_start(ap, mid);
    jint r = (*env)->CallNonvirtualIntMethodV(env, obj, cls, mid, ap);
    va_end(ap);
    return r;
}

static jobject v_new(JNIEnv *env, jclass cls, jmethodID mid, ...) {
    va_list ap;
    va_start(ap, mid);
    jobject r = (*env)->NewObjectV(env, cls, mid, ap);
    va_end(ap);
    return r;
}

// out[8] (in) the pool address of the snprintf binding, out[9] (in) its buffer, out[11] (in) the buffer size, out[10] (out) snprintf's result: the `p` row.
// out[0] GetVersion, out[1] CallStaticIntMethod(ADD_A, ADD_B),out[2] GetJavaVM's result, out[3] the vm it gave, out[4] 1 when NewStringUTF("abc") reads back "abc",
// out[5] the bits of CallStaticDoubleMethod's result, out[6] the bits of CallStaticFloatMethod's result, out[7] (refuse == REFUSE_FORGED) the address of a JNI stub past the table; out[21..24] the V calls (below)
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
    // Q6-4: JNI_OnLoad's vm and reserved, nativeMix's env and class, nativeScale's env and class
    for (int k = 0; k < 6; k++) {
        out[15 + k] = qj_seen[k];
    }
    // Q6-7: out[21] the bits of CallStaticDoubleMethodV's result, out[22] CallLongMethodV's, out[23] CallNonvirtualIntMethodV's, out[24] NewObjectV's object
    jmethodID vmid = (jmethodID)(intptr_t)V_MID;
    jobject vobj = (jobject)(intptr_t)OBJ;
    union {
        jdouble d;
        long long i;
    } vu;
    vu.d = v_static(env, cls, vmid, V_ARGS);
    out[21] = vu.i;
    out[22] = v_virtual(env, vobj, vmid, V_ARGS);
    out[23] = v_nonvirtual(env, vobj, cls, vmid, V_ARGS);
    out[24] = (long long)(intptr_t)v_new(env, cls, vmid, V_ARGS);
}

// JNI_OnLoad(vm, reserved): ART calls it with no shorty, the host through a down shell (Vm kind); records both, returns JNI_VERSION_1_6 (jni.h)
jint JNI_OnLoad(JavaVM *vm, void *reserved) {
    qj_seen[0] = (long long)(intptr_t)vm;
    qj_seen[1] = (long long)(intptr_t)reserved;
    return JNI_VERSION_1_6;
}

// nativeMix: 14 parameters, ten of integer class and four double, so the call spills to the stack on both ABIs (AAPCS64 takes eight integers in registers, SysV six)
// Each argument has its own weight, 10^0 .. 10^11 by position, so a misplaced one changes the sum (expected value: tests/qemu_jni.rs MIX_EXPECT)
jdouble nativeMix(JNIEnv *env, jclass cls, jint a, jdouble b, jlong c, jint d, jdouble e, jlong f, jint g, jdouble h, jlong i, jint j, jdouble k, jlong l) {
    qj_seen[2] = (long long)(intptr_t)env;
    qj_seen[3] = (long long)(intptr_t)cls;
    return a + b * 1e1 + c * 1e2 + d * 1e3 + e * 1e4 + f * 1e5 + g * 1e6 + h * 1e7 + i * 1e8 + j * 1e9 + k * 1e10 + l * 1e11;
}

// nativeScale: a float result from a float and an int (expected value: tests/qemu_jni.rs SCALE_EXPECT)
jfloat nativeScale(JNIEnv *env, jclass cls, jfloat x, jint n) {
    qj_seen[4] = (long long)(intptr_t)env;
    qj_seen[5] = (long long)(intptr_t)cls;
    return x * n + 0.25f;
}

// the executable needs a main (as qemu-thread's fixture has one); the guest linker starts it, the host calls qj_run
int main(void) {
    return 0;
}
