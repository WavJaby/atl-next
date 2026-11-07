// SPDX-License-Identifier: MIT
package atl.bridge1.testlib;

/**
 * Java side of the C12 legacy fixture: libatl-jni-legacy.so (xtask/testdata/jni-test/atl_jni_legacy.cpp, built by xtask/src/jni_fixture.rs with DT_NEEDED libstdc++.so and libz.so). Own class, so a failed load fails only its rows (Main.bridge1Legacy).
 * Expected (app.rs BRIDGE1_CALLS): legacyStdio "17 1 6 3 2 2 2", legacyNew "40 42", zRoundTrip 3421780262.
 */
public final class AtlLegacyTest {
    static {
        System.loadLibrary("atl-jni-legacy");
    }

    private AtlLegacyTest() {}

    /** Pre-23 stdio, the _ctype_ table, fortified strlen/strchr/strncpy, fpclassify(NAN), errno after a failing open: seven values in one line. */
    public static native String legacyStdio();

    /** new int(40) through operator new/delete, then a guarded function-local static (42): "<value> <static>". */
    public static native String legacyNew();

    /** compress + uncompress of "123456789", then crc32 of the result (the CRC-32 check value 0xCBF43926). */
    public static native long zRoundTrip();
}
