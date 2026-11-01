// SPDX-License-Identifier: MIT
// test fixture of `xtask test native-load`: exactly what register_android_util_Log reads and binds
package android.util;

public final class Log {
    public static final int VERBOSE = 2;
    public static final int DEBUG = 3;
    public static final int INFO = 4;
    public static final int WARN = 5;
    public static final int ERROR = 6;
    public static final int ASSERT = 7;

    public static native boolean isLoggable(String tag, int level);
    public static native int println_native(int bufID, int priority, String tag, String msg);
    public static native int logger_entry_max_payload_native();
}
