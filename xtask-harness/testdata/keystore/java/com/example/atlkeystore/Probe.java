// SPDX-License-Identifier: MIT
package com.example.atlkeystore;

import android.os.IBinder;
import android.util.Log;

import java.io.File;
import java.lang.reflect.Array;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

/**
 * `xtask test keystore`: keystore2 as an app sees it over binder. Run as a plain app process (boot class loader: the framework's hidden AIDL classes resolve, reached by reflection since the APK
 * compiles against the public SDK). One line per check, `AtlKeystore: <check> <value>`; the harness judges them. The KeyMint HAL is reached only through keystore2, as an app on a device is
 * (sepolicy keeps apps from finding hal_keymint_service).
 */
public final class Probe {
    private static final String TAG = "AtlKeystore";
    private static final String KEYSTORE = "android.system.keystore2.IKeystoreService/default";
    private static final String KM = "android.hardware.security.keymint.";
    private static final String KS = "android.system.keystore2.";
    /** keymint SecurityLevel.TRUSTED_ENVIRONMENT */
    private static final int TRUSTED_ENVIRONMENT = 1;
    /** keystore2 Domain.APP */
    private static final int DOMAIN_APP = 0;
    /** keystore2 Domain.SELINUX (Domain.aidl:25) */
    private static final int DOMAIN_SELINUX = 2;
    /** system/sepolicy private/keystore2_key_contexts:21: namespace 102 = wifi_key */
    private static final long WIFI_NAMESPACE = 102;
    /** keymint Tag values (Tag.aidl: TagType << 28 | n): ALGORITHM = ENUM|2, PURPOSE = ENUM_REP|1, EC_CURVE = ENUM|10, DIGEST = ENUM_REP|5, NO_AUTH_REQUIRED = BOOL|503, ORIGIN = ENUM|702. */
    private static final int TAG_ORIGIN = (1 << 28) | 702;
    private static final int TAG_ALGORITHM = (1 << 28) | 2;
    private static final int TAG_PURPOSE = (2 << 28) | 1;
    private static final int TAG_EC_CURVE = (1 << 28) | 10;
    private static final int TAG_DIGEST = (2 << 28) | 5;
    private static final int TAG_NO_AUTH_REQUIRED = (7 << 28) | 503;
    /** Algorithm.EC, KeyPurpose.SIGN, EcCurve.P_256, Digest.SHA_2_256 */
    private static final int EC = 3;
    private static final int SIGN = 2;
    private static final int P_256 = 1;
    private static final int SHA_2_256 = 4;

    private static void line(String text) {
        Log.i(TAG, text);
    }

    /** The cause of a reflective call's failure: what the binder call threw. */
    private static Throwable cause(Throwable t) {
        return t instanceof InvocationTargetException ? t.getCause() : t;
    }

    /** A ServiceSpecificException's code (not in the public SDK), else the throwable itself. */
    private static String outcome(Throwable t) throws Exception {
        Throwable c = cause(t);
        if (c.getClass().getName().equals("android.os.ServiceSpecificException")) {
            return "error " + c.getClass().getField("errorCode").getInt(c);
        }
        return "threw " + c;
    }

    private static Object descriptor(String alias) throws Exception {
        Class<?> c = Class.forName(KS + "KeyDescriptor");
        Object kd = c.getConstructor().newInstance();
        c.getField("domain").setInt(kd, DOMAIN_APP);
        c.getField("nspace").setLong(kd, -1);
        c.getField("alias").set(kd, alias);
        return kd;
    }

    /** keymint KeyParameter {tag, value = KeyParameterValue.<field>(v)}. */
    private static Object param(int tag, String field, Class<?> type, Object v) throws Exception {
        Class<?> pc = Class.forName(KM + "KeyParameter");
        Class<?> vc = Class.forName(KM + "KeyParameterValue");
        Object p = pc.getConstructor().newInstance();
        pc.getField("tag").setInt(p, tag);
        pc.getField("value").set(p, vc.getMethod(field, type).invoke(null, v));
        return p;
    }

    /**
     * An EC signing key generated through keystore2's TEE slot: its ORIGIN authorization's keymint SecurityLevel (0 SOFTWARE), the one KeyInfo.getSecurityLevel reports
     * (AndroidKeyStoreSecretKeyFactorySpi.getKeyInfo). KeyMetadata.keySecurityLevel is the slot keystore2 called (security_level.rs), TRUSTED_ENVIRONMENT here as on a device.
     */
    private static void generated(Object ks, Object level) throws Exception {
        Class<?> pc = Class.forName(KM + "KeyParameter");
        Object params = Array.newInstance(pc, 5);
        Array.set(params, 0, param(TAG_ALGORITHM, "algorithm", int.class, EC));
        Array.set(params, 1, param(TAG_PURPOSE, "keyPurpose", int.class, SIGN));
        Array.set(params, 2, param(TAG_EC_CURVE, "ecCurve", int.class, P_256));
        Array.set(params, 3, param(TAG_DIGEST, "digest", int.class, SHA_2_256));
        Array.set(params, 4, param(TAG_NO_AUTH_REQUIRED, "boolValue", boolean.class, true));
        Class<?> kdc = Class.forName(KS + "KeyDescriptor");
        Object kd = descriptor("atl-probe-key");
        Object md;
        try {
            md = level.getClass().getMethod("generateKey", kdc, kdc, params.getClass(), int.class, byte[].class)
                    .invoke(level, kd, null, params, 0, new byte[0]);
        } catch (Throwable t) {
            line("generate-key " + outcome(t));
            return;
        }
        int originLevel = -1;
        for (Object a : (Object[]) md.getClass().getField("authorizations").get(md)) {
            Object kp = a.getClass().getField("keyParameter").get(a);
            if (kp.getClass().getField("tag").getInt(kp) == TAG_ORIGIN) {
                originLevel = a.getClass().getField("securityLevel").getInt(a);
            }
        }
        line("origin-level " + originLevel);
        try {
            // IKeystoreSecurityLevel.deleteKey takes Domain::BLOB keys only; an APP key is deleted through the service
            ks.getClass().getMethod("deleteKey", kdc).invoke(ks, kd);
            line("delete-key ok");
        } catch (Throwable t) {
            line("delete-key " + outcome(t));
        }
    }

    public static void main(String[] args) {
        line("start uid " + android.os.Process.myUid());
        line("keystore-dir " + (new File("/data/misc/keystore").exists() ? "visible" : "absent"));
        try {
            IBinder b = (IBinder) Class.forName("android.os.ServiceManager").getMethod("checkService", String.class).invoke(null, KEYSTORE);
            line("keystore " + (b == null ? "missing" : "found"));
            if (b != null) {
                Object ks = Class.forName(KS + "IKeystoreService$Stub").getMethod("asInterface", IBinder.class).invoke(null, b);
                Object level = null;
                try {
                    level = ks.getClass().getMethod("getSecurityLevel", int.class).invoke(ks, TRUSTED_ENVIRONMENT);
                    line("security-level " + (level == null ? "null" : "binder"));
                } catch (Throwable t) {
                    line("security-level " + outcome(t));
                }
                try {
                    ks.getClass().getMethod("getKeyEntry", Class.forName(KS + "KeyDescriptor")).invoke(ks, descriptor("atl-absent-alias"));
                    line("get-key-entry found");
                } catch (Throwable t) {
                    line("get-key-entry " + outcome(t));
                }
                try {
                    // a SELINUX namespace an app has no grant on (keystore2_key_contexts 102 = wifi_key): denied, and keystore2 keeps serving (the key below is made after it)
                    Object[] l = (Object[]) ks.getClass().getMethod("listEntries", int.class, long.class).invoke(ks, DOMAIN_SELINUX, WIFI_NAMESPACE);
                    line("selinux-list " + l.length + " entries");
                } catch (Throwable t) {
                    line("selinux-list " + outcome(t));
                }
                if (level != null) {
                    generated(ks, level);
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
