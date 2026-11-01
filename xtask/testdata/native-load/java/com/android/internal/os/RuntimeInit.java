// SPDX-License-Identifier: MIT
// test fixture of `xtask test native-load`: exactly the natives register_com_android_internal_os_RuntimeInit binds
package com.android.internal.os;

public class RuntimeInit {
    static native void nativeFinishInit();
    static native void nativeSetExitWithoutCleanup(boolean exitWithoutCleanup);
}
