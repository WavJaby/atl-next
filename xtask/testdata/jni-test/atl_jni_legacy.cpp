// SPDX-License-Identifier: MIT
// C12 legacy fixture (gs3 C12, game-support §4): the calls old NDK binaries make -- pre-23 stdio (`stdout` = `&__sF[1]`), the `_ctype_` table, fortified string calls, C++ new/delete and a guarded static, zlib -- linked against libstdc++.so and libz.so (NDK stubs; the guest's AOSP libstdc++ and zlib answer them). Built by jni_fixture.rs as libatl-jni-legacy.so; expected values are app.rs BRIDGE1_CALLS. C++ because new/delete and the function-local static need it.
#include <errno.h>
#include <fcntl.h>
#include <jni.h>
#include <math.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <zlib.h>

extern "C" {
// bionic declares `_ctype_` as a pointer to the table (libc/include/ctype.h:74, upstream-openbsd ctype_.c:86); the table is indexed from 1, index 0 is EOF.
extern const char* _ctype_;
}

namespace {

// bionic ctype.h `_CTYPE_U`.
constexpr int kCtypeUpper = 0x01;

// Written and read back: keeps the new/delete pair and the static's initialiser from being folded away.
volatile void* g_sink;
volatile int g_seed = 40;

// A dynamic initialiser (a volatile read), so the static takes the __cxa_guard_acquire path.
int guardedAnswer() {
    static const int answer = g_seed + 2;
    return answer;
}

}  // namespace

// "legacy-stdout 42\n" is 17 bytes; the line also goes to stdout (the app log's stdout tag). The other six values: _ctype_ upper bit of 'A', strlen, strchr offset, strlen after strncpy, fpclassify(NAN), errno after a failing open.
extern "C" JNIEXPORT jstring JNICALL Java_atl_bridge1_testlib_AtlLegacyTest_legacyStdio(JNIEnv* env, jclass) {
    int printed = fprintf(stdout, "legacy-stdout %d\n", 42);
    int upper = ((_ctype_ + 1)['A'] & kCtypeUpper) != 0;
    // the contents come from a volatile pointer, so the fortified calls cannot fold to constants
    const char* volatile src = "abcdef";
    char buf[16];
    strcpy(buf, src);
    long len = strlen(buf);
    long at = strchr(buf, 'd') - buf;
    char dst[8];
    strncpy(dst, "xy", sizeof dst);
    long dlen = strlen(dst);
    volatile double nanv = NAN;
    int cls = fpclassify(nanv);
    errno = 0;
    int fd = open("/nonexistent/atl-jni-legacy", O_RDONLY);
    int err = fd < 0 ? errno : 0;
    if (fd >= 0) {
        close(fd);
    }
    char out[64];
    snprintf(out, sizeof out, "%d %d %ld %ld %ld %d %d", printed, upper, len, at, dlen, cls, err);
    return env->NewStringUTF(out);
}

// operator new(size_t) = _Znwm, operator delete(void*) = _ZdlPv, __cxa_guard_acquire through guardedAnswer.
extern "C" JNIEXPORT jstring JNICALL Java_atl_bridge1_testlib_AtlLegacyTest_legacyNew(JNIEnv* env, jclass) {
    int* p = new int(g_seed);
    g_sink = p;
    int v = *p;
    delete p;
    char out[32];
    snprintf(out, sizeof out, "%d %d", v, guardedAnswer());
    return env->NewStringUTF(out);
}

// compress + uncompress of the CRC-32 check input; crc32 of the round trip is the published check value 0xCBF43926.
extern "C" JNIEXPORT jlong JNICALL Java_atl_bridge1_testlib_AtlLegacyTest_zRoundTrip(JNIEnv*, jclass) {
    static const unsigned char check[] = "123456789";
    uLong n = sizeof(check) - 1;
    Bytef packed[64];
    uLongf plen = sizeof packed;
    if (compress(packed, &plen, check, n) != Z_OK) {
        return -1;
    }
    Bytef unpacked[16];
    uLongf ulen = sizeof unpacked;
    if (uncompress(unpacked, &ulen, packed, plen) != Z_OK || ulen != n) {
        return -2;
    }
    return static_cast<jlong>(crc32(0L, unpacked, static_cast<uInt>(ulen)));
}
