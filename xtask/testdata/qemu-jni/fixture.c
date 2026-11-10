// SPDX-License-Identifier: MIT
// Guest side of the Q6-2 JNI gate (xtask `test qemu-jni`, QEMU bridge plan Q6-2): a bionic PIE executable that the guest linker loads; the host calls qj_run with a guest env (tests/qemu_jni.rs), and each JNI call below crosses to the fake host JNIEnv there.
// Calls no libc symbol: only the stub libc's intercepted stubs resolve in the guest (see qemu-thread's fixture.c).
// Results go to out[] (one per check in the gate), not a packed return, so each check names its own value.
#include <jni.h>
#include <stdint.h>

// the fake host's CallStaticIntMethod answers a * 10 + b; a swapped pair shows as 32
#define ADD_A 2
#define ADD_B 3

// out[0] GetVersion, out[1] CallStaticIntMethod(ADD_A, ADD_B), out[2] GetJavaVM's result, out[3] the vm it gave, out[4] 1 when NewStringUTF("abc") reads back "abc"
void qj_run(JNIEnv *env, long long *out) {
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
}

// the executable needs a main (as qemu-thread's fixture has one); the guest linker starts it, the host calls qj_run
int main(void) {
    return 0;
}
