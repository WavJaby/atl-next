// SPDX-License-Identifier: MIT
// test fixture of `xtask test native-load`: exactly the natives register_android_os_SystemClock binds
package android.os;

import dalvik.annotation.optimization.CriticalNative;

public final class SystemClock {
    @CriticalNative public static native long uptimeMillis();
    @CriticalNative public static native long uptimeNanos();
    @CriticalNative public static native long elapsedRealtime();
    @CriticalNative public static native long elapsedRealtimeNanos();
    @CriticalNative public static native long currentThreadTimeMillis();
    @CriticalNative public static native long currentThreadTimeMicro();
    @CriticalNative public static native long currentTimeMicro();
}
