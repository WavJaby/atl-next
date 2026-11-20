// SPDX-License-Identifier: MIT
package com.example.atlkeystore;

import android.os.IBinder;
import android.util.Log;

import java.io.File;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

/**
 * `xtask test keystore`: keystore2 and its KeyMint HAL as an app sees them over binder. Run as a plain app process (boot class loader: the framework's hidden AIDL classes resolve, reached by
 * reflection since the APK compiles against the public SDK). One line per check, `AtlKeystore: <check> <value>`; the harness judges them.
 */
public final class Probe {
    private static final String TAG = "AtlKeystore";
    private static final String KEYSTORE = "android.system.keystore2.IKeystoreService/default";
    private static final String KEYMINT = "android.hardware.security.keymint.IKeyMintDevice/default";
    /** android.hardware.security.keymint.SecurityLevel.TRUSTED_ENVIRONMENT */
    private static final int TRUSTED_ENVIRONMENT = 1;
    /** android.system.keystore2.Domain.APP */
    private static final int DOMAIN_APP = 0;

    private static void line(String text) {
        Log.i(TAG, text);
    }

    private static Object service(String name, String stub) throws Exception {
        Class<?> sm = Class.forName("android.os.ServiceManager");
        String[] all = (String[]) sm.getMethod("listServices").invoke(null);
        line("listed " + name + " " + java.util.Arrays.asList(all).contains(name) + " of " + all.length);
        IBinder b = (IBinder) sm.getMethod("checkService", String.class).invoke(null, name);
        if (b == null) {
            return null;
        }
        return Class.forName(stub).getMethod("asInterface", IBinder.class).invoke(null, b);
    }

    /** The cause of a reflective call's failure: what the binder call threw. */
    private static Throwable cause(Throwable t) {
        return t instanceof InvocationTargetException ? t.getCause() : t;
    }

    public static void main(String[] args) {
        line("start uid " + android.os.Process.myUid());
        line("keystore-dir " + (new File("/data/misc/keystore").exists() ? "visible" : "absent"));
        try {
            Object ks = service(KEYSTORE, "android.system.keystore2.IKeystoreService$Stub");
            line("keystore " + (ks == null ? "missing" : "found"));
            if (ks != null) {
                Method level = ks.getClass().getMethod("getSecurityLevel", int.class);
                try {
                    Object sl = level.invoke(ks, TRUSTED_ENVIRONMENT);
                    line("security-level " + (sl == null ? "null" : "binder"));
                } catch (Throwable t) {
                    line("security-level threw " + cause(t));
                }
                Class<?> kdClass = Class.forName("android.system.keystore2.KeyDescriptor");
                Object kd = kdClass.getConstructor().newInstance();
                kdClass.getField("domain").setInt(kd, DOMAIN_APP);
                kdClass.getField("nspace").setLong(kd, -1);
                kdClass.getField("alias").set(kd, "atl-absent-alias");
                try {
                    ks.getClass().getMethod("getKeyEntry", kdClass).invoke(ks, kd);
                    line("get-key-entry found");
                } catch (Throwable t) {
                    Throwable c = cause(t);
                    // android.os.ServiceSpecificException is not in the public SDK
                    if (c.getClass().getName().equals("android.os.ServiceSpecificException")) {
                        line("get-key-entry error " + c.getClass().getField("errorCode").getInt(c));
                    } else {
                        line("get-key-entry threw " + c);
                    }
                }
            }
            Object km = service(KEYMINT, "android.hardware.security.keymint.IKeyMintDevice$Stub");
            line("keymint " + (km == null ? "missing" : "found"));
            if (km != null) {
                try {
                    Object info = km.getClass().getMethod("getHardwareInfo").invoke(km);
                    line("hardware-level " + info.getClass().getField("securityLevel").getInt(info));
                } catch (Throwable t) {
                    line("hardware-level threw " + cause(t));
                }
            }
        } catch (Throwable t) {
            line("failed " + t);
            System.exit(1);
        }
        line("done");
        System.exit(0);
    }
}
