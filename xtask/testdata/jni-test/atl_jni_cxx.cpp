// SPDX-License-Identifier: MIT
// bridge-1 fixture, C++ side: throw/catch through libc++ (linked as libc++_shared.so).
#include <jni.h>
#include <cstring>
#include <stdexcept>

extern "C" JNIEXPORT jint JNICALL
Java_atl_bridge1_testlib_AtlJniTest_cppThrowCatch(JNIEnv *, jclass)
{
    try {
        throw std::runtime_error("bridge1");
    } catch (const std::runtime_error &e) {
        return std::strcmp(e.what(), "bridge1") == 0 ? 1 : -1;
    }
    return -2;
}
