// SPDX-License-Identifier: MIT
// test fixture of `xtask test native-load`: exactly the natives register_android_util_CharsetUtils binds
package android.util;

import dalvik.annotation.optimization.FastNative;

public class CharsetUtils {
    @FastNative public static native int toModifiedUtf8Bytes(String src, int srcLen, long dest, int destOff, int destLen);
    @FastNative public static native String fromModifiedUtf8Bytes(long src, int srcOff, int srcLen);
}
