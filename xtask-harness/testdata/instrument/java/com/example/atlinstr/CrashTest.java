// SPDX-License-Identifier: MIT
package com.example.atlinstr;

import android.os.Process;
import junit.framework.TestCase;

/** `xtask test instrument`: a process that dies in the middle of a run (SIGKILL: no exception, no finishInstrumentation). The activity manager must report the instrumentation as cancelled. */
public class CrashTest extends TestCase {
    public void testCrash() {
        Process.killProcess(Process.myPid());
    }
}
