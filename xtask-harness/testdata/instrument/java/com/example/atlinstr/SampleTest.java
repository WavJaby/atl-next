// SPDX-License-Identifier: MIT
package com.example.atlinstr;

import junit.framework.TestCase;

/** `xtask test instrument` (xtask-harness/src/instrument.rs): one test of each outcome InstrumentationTestRunner reports: passes (status 0), fails an assertion (-2), throws (-1). */
public class SampleTest extends TestCase {
    public void testPass() {
        assertEquals(4, 2 + 2);
    }

    public void testFail() {
        assertEquals("deliberate failure", 1, 2);
    }

    public void testError() {
        throw new IllegalStateException("deliberate error");
    }
}
