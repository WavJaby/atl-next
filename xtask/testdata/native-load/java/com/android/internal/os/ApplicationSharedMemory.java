// SPDX-License-Identifier: MIT
// test fixture of `xtask test native-load`: exactly the natives register_com_android_internal_os_ApplicationSharedMemory binds
package com.android.internal.os;

public class ApplicationSharedMemory {
    private static native int nativeCreate();
    private static native long nativeMap(int fd, boolean isMutable);
    private static native void nativeInit(long ptr);
    private static native void nativeUnmap(long ptr);
    private static native int nativeDupAsReadOnly(int fd);
    private static native void nativeSetLatestNetworkTimeUnixEpochMillisAtZeroElapsedRealtimeMillis(long ptr, long offset);
    private static native long nativeGetLatestNetworkTimeUnixEpochMillisAtZeroElapsedRealtimeMillis(long ptr);
    private static native long nativeGetSystemNonceBlock(long ptr);
    private static native void nativeWriteSystemFeaturesCache(long ptr, int[] cache);
    private static native int[] nativeReadSystemFeaturesCache(long ptr);
}
