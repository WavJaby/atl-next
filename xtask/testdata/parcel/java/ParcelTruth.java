// SPDX-License-Identifier: MIT
// Driver of `xtask test parcel` on host ART (framework.jar on the boot classpath): for each M2 parcelable, Java's own writeToParcel bytes go through atl-parcelables' codec (libatl_parcel_truth.so:
// decode, every byte consumed, re-encode) and must come back byte-equal; Java's createFromParcel must then read the Rust bytes and write the same bytes again. Never shipped.
// Field values are distinct from the defaults, so a codec that drops, swaps or misreads a field changes the bytes. Output: `pt pass|fail <case> <detail>` lines; exit 0 = all passed.
// Bundles and Intent extras that carry objects go through Java's own Parcels (roundTripParcel: the codec reads and writes the Parcel's data and object table): a binder must come out usable, a descriptor must be reported as Java reports it.
// Self-tests (must fail as stated, else `pt fail`): a corrupted Rust encoding is caught by the comparison, a wrong codec by the consumption check, an Intent with ClipData and a Bundle with a bad mHasIntent by their refusals (each refusal is also a line on stderr, which xtask checks).

import android.app.ContentProviderHolder;
import android.app.servertransaction.ClientTransaction;
import android.app.servertransaction.LaunchActivityItem;
import android.content.ClipData;
import android.content.ComponentName;
import android.content.Intent;
import android.content.pm.ActivityInfo;
import android.content.pm.ApplicationInfo;
import android.content.pm.InstrumentationInfo;
import android.content.pm.PackageInfo;
import android.content.pm.ParceledListSlice;
import android.content.pm.ProviderInfo;
import android.content.pm.ProviderInfoList;
import android.content.pm.SharedLibraryInfo;
import android.content.res.CompatibilityInfo;
import android.content.res.Configuration;
import android.graphics.Rect;
import android.net.Uri;
import android.os.Binder;
import android.os.Bundle;
import android.os.IBinder;
import android.os.LocaleList;
import android.os.Parcel;
import android.os.ParcelFileDescriptor;
import android.os.Parcelable;
import android.window.ActivityWindowInfo;
import java.io.File;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Map;
import java.util.Objects;
import java.util.UUID;

public class ParcelTruth {
    static native byte[] roundTrip(String className, byte[] marshalled, int mode);
    static native String lastError();
    static native String refused();
    static native int startReg();
    /// The codec of `className` decodes the value at `in`'s position (every byte consumed) and encodes it into `out`, both Java Parcels with their objects: 0 = done, -1 = failed (lastError).
    static native int roundTripParcel(String className, Parcel in, Parcel out);
    /// The codec's has_file_descriptors of the value at `in`'s position: 1 or 0, -1 = failed (lastError).
    static native int rustHasFileDescriptors(String className, Parcel in);

    static final int MODE_PLAIN = 0;
    static final int MODE_CORRUPT = 1;
    /// java.io.FileDescriptor only: the codec writes a descriptor and reads it back.
    static final int MODE_FD_SELF = 3;
    /// Bundle only: the input is two marshalled Bundles, A then B; the result is A with putAll(B).
    static final int MODE_PUT_ALL = 4;
    static int failures = 0;

    public static void main(String[] args) {
        // args: libatl_parcel_truth.so, libandroid_runtime.so; the framework's natives registered as app_process does
        System.load(args[0]);
        System.load(args[1]);
        int reg = startReg();
        if (reg != 0) {
            System.out.println("pt fail startReg returned " + reg);
            System.exit(1);
        }
        ApplicationInfo ai = applicationInfo();
        ActivityInfo activity = activityInfo(ai);
        Configuration config = configuration();
        ProviderInfo provider = providerInfo(ai);
        Intent intent = intent();

        check("ApplicationInfo", ai, ApplicationInfo.CREATOR);
        check("ActivityInfo", activity, ActivityInfo.CREATOR);
        check("Configuration", config, Configuration.CREATOR);
        check("CompatibilityInfo", CompatibilityInfo.DEFAULT_COMPATIBILITY_INFO, CompatibilityInfo.CREATOR);
        check("Intent", intent, Intent.CREATOR);
        Intent withSelector = new Intent(Intent.ACTION_MAIN);
        withSelector.setSelector(new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_APP_BROWSER));
        check("Intent+selector", withSelector, Intent.CREATOR);
        // extras (Intent.writeToParcel: writeBundle(mExtras)): a non-null Bundle after the first putExtra
        check("Intent:extras", new Intent(intent).putExtra("k", 1).putExtra("s", "v"), Intent.CREATOR);
        check("ProviderInfo", provider, ProviderInfo.CREATOR);
        check("ProviderInfoList", ProviderInfoList.fromList(Arrays.asList(provider)), ProviderInfoList.CREATOR);
        ContentProviderHolder holder = new ContentProviderHolder(provider);
        holder.noReleaseNeeded = true;
        check("ContentProviderHolder", holder, ContentProviderHolder.CREATOR);
        check("PackageInfo", packageInfo(ai), PackageInfo.CREATOR);
        check("ComponentName", new ComponentName("com.example.app", "com.example.app.Runner"), ComponentName.CREATOR);
        check("InstrumentationInfo", instrumentationInfo(ai), InstrumentationInfo.CREATOR);
        // IPackageManager.queryInstrumentationAsUser's reply: the inline list, its element class written once
        check("ParceledListSlice", new ParceledListSlice<>(Arrays.asList(instrumentationInfo(ai), instrumentationInfo(ai))), ParceledListSlice.CREATOR);
        ClientTransaction transaction = new ClientTransaction();
        transaction.addTransactionItem(new LaunchActivityItem(null, intent, 3, activity, config, new Configuration(), 0,
                "com.example.referrer", null, 2, null, null, null, null, null, true, null, null, null, null, false, null, null,
                new ActivityWindowInfo()));
        check("ClientTransaction", transaction, ClientTransaction.CREATOR);
        // IPackageManager.notifyDexLoad's classLoaderContextMap: dex path -> class loader context, a null value included
        HashMap<String, String> contexts = new HashMap<>();
        contexts.put("/data/app/com.example.app/base.apk", "PCL[]");
        contexts.put("/data/app/com.example.app/split.apk", null);
        checkMap("Map", contexts);
        checkMap("Map:null", null);
        // IApplicationThread.bindApplication's coreSettings: the empty Bundle M2 sends
        check("Bundle", new Bundle(), Bundle.CREATOR);
        // a non-empty Bundle: the codec copies its body verbatim
        Bundle entries = new Bundle();
        entries.putString("key", "value");
        entries.putInt("int", -7);
        entries.putBoolean("flag", true);
        check("Bundle:entries", entries, Bundle.CREATOR);
        // AMS merges instrumentation results (finishInstrumentationLocked: putAll) and makes the crash report's Bundle (putString): the codec's typed entries against Java's own
        Bundle more = new Bundle();
        more.putString("key", "replaced");
        more.putString("shortMsg", "Process crashed.");
        more.putString("none", null);
        more.putLong("long", -5000000000L);
        checkPutAll("Bundle:put-all", entries, more);
        // objects in Bundles: androidx's InstrumentationConnection broadcasts extras that hold a binder (AOSP Bundles carry binders)
        Binder echo = new Echo();
        checkBinder("Bundle:binder", echo);
        checkDescriptors("Bundle:fd", echo);
        checkIntentBinder("Intent:binder-extras", echo);

        selfTests(config, intent, entries);
        System.out.println("pt summary " + failures + " failed");
        System.exit(failures == 0 ? 0 : 1);
    }

    /// A local binder that answers transaction 1: the string it reads, prefixed.
    static class Echo extends Binder {
        @Override
        protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) {
            reply.writeString("echo:" + data.readString());
            return true;
        }
    }

    static Bundle bundleWith(IBinder binder) {
        Bundle b = new Bundle();
        b.putString("key", "value");
        b.putBinder("binder", binder);
        return b;
    }

    /// The Parcel `p` wrote to, rewound for reading.
    static Parcel written(Parcelable p) {
        Parcel parcel = Parcel.obtain();
        p.writeToParcel(parcel, 0);
        parcel.setDataPosition(0);
        return parcel;
    }

    /// Calls `binder` as a client: transaction 1 with "ping" must answer "echo:ping".
    static boolean usable(IBinder binder) {
        Parcel data = Parcel.obtain();
        Parcel reply = Parcel.obtain();
        try {
            data.writeString("ping");
            binder.transact(1, data, reply, 0);
            reply.setDataPosition(0);
            return "echo:ping".equals(reply.readString());
        } catch (android.os.RemoteException e) {
            return false;
        } finally {
            data.recycle();
            reply.recycle();
        }
    }

    /// A Bundle holding a string and a binder goes Java Parcel -> codec -> Java Parcel (the binder object is copied with the body): Java reads the very same Binder back, and it answers.
    static void checkBinder(String name, Binder echo) {
        Parcel in = written(bundleWith(echo));
        Parcel out = Parcel.obtain();
        if (roundTripParcel("android.os.Bundle", in, out) != 0) {
            result(false, name, "rust: " + lastError());
            return;
        }
        boolean consumed = out.dataSize() == in.dataSize();
        out.setDataPosition(0);
        Bundle back = Bundle.CREATOR.createFromParcel(out);
        IBinder got = back.getBinder("binder");
        boolean ok = consumed && got == echo && "value".equals(back.getString("key")) && usable(got);
        result(ok, name, ok ? "binder survived java -> rust -> java: same object, answers a transaction" : "binder " + got + ", key " + back.getString("key") + ", sizes " + in.dataSize() + "/" + out.dataSize());
        in.recycle();
        out.recycle();
    }

    /// An Intent whose extras hold a binder, the same way.
    static void checkIntentBinder(String name, Binder echo) {
        Intent i = new Intent("com.example.BROADCAST").putExtras(bundleWith(echo));
        Parcel in = written(i);
        Parcel out = Parcel.obtain();
        if (roundTripParcel("android.content.Intent", in, out) != 0) {
            result(false, name, "rust: " + lastError());
            return;
        }
        out.setDataPosition(0);
        Intent back = Intent.CREATOR.createFromParcel(out);
        IBinder got = back.getExtras().getBinder("binder");
        boolean ok = "com.example.BROADCAST".equals(back.getAction()) && got == echo && "value".equals(back.getStringExtra("key")) && usable(got);
        result(ok, name, ok ? "Intent extras' binder survived java -> rust -> java" : "action " + back.getAction() + ", binder " + got);
        in.recycle();
        out.recycle();
    }

    /// hasFileDescriptors: the codec says what Java says (Bundle.hasFileDescriptors / Intent.hasFileDescriptors) for a Bundle with no objects, with a binder, and with a file descriptor; and the descriptor
    /// survives the codec (Java's Bundle read from the codec's output still has one).
    static void checkDescriptors(String name, Binder echo) {
        try {
            Bundle fd = new Bundle();
            fd.putParcelable("pfd", ParcelFileDescriptor.open(new File("/dev/null"), ParcelFileDescriptor.MODE_READ_ONLY));
            Bundle[] bundles = {new Bundle(), bundleWith(echo), fd};
            StringBuilder said = new StringBuilder();
            boolean ok = true;
            for (Bundle b : bundles) {
                Parcel in = written(b);
                int rust = rustHasFileDescriptors("android.os.Bundle", in);
                in.setDataPosition(0);
                Intent intent = new Intent("x").putExtras(b);
                Parcel inIntent = written(intent);
                int rustIntent = rustHasFileDescriptors("android.content.Intent", inIntent);
                int java = b.hasFileDescriptors() ? 1 : 0;
                said.append(java).append(rust).append(rustIntent).append(' ');
                ok &= rust == java && rustIntent == (intent.hasFileDescriptors() ? 1 : 0);
                in.recycle();
                inIntent.recycle();
            }
            Parcel in = written(fd);
            Parcel out = Parcel.obtain();
            boolean copied = roundTripParcel("android.os.Bundle", in, out) == 0;
            out.setDataPosition(0);
            boolean kept = copied && Bundle.CREATOR.createFromParcel(out).hasFileDescriptors();
            in.recycle();
            out.recycle();
            result(ok && kept, name, ok && kept ? "hasFileDescriptors (java rust-bundle rust-intent): " + said + "; the descriptor survives the codec" : "java/rust/rust-intent " + said + ", copied " + copied + ", kept " + kept + ", " + lastError());
        } catch (java.io.FileNotFoundException e) {
            result(false, name, "cannot open /dev/null: " + e);
        }
    }

    static SharedLibraryInfo reparceled(SharedLibraryInfo s) {
        Parcel parcel = Parcel.obtain();
        s.writeToParcel(parcel, 0);
        parcel.setDataPosition(0);
        SharedLibraryInfo read = SharedLibraryInfo.CREATOR.createFromParcel(parcel);
        parcel.recycle();
        return read;
    }

    static ApplicationInfo applicationInfo() {
        ApplicationInfo ai = new ApplicationInfo();
        ai.packageName = "com.example.app";
        ai.name = "com.example.app.App";
        ai.className = "com.example.app.App";
        ai.processName = "com.example.app:main";
        ai.nonLocalizedLabel = "Example";
        ai.uid = 10123;
        ai.minSdkVersion = 21;
        ai.targetSdkVersion = 34;
        ai.compileSdkVersion = 35;
        ai.flags = ApplicationInfo.FLAG_HAS_CODE | ApplicationInfo.FLAG_ALLOW_BACKUP;
        ai.theme = 0x7f0e0001;
        ai.icon = 0x7f080002;
        ai.sourceDir = "/data/app/com.example.app/base.apk";
        ai.publicSourceDir = ai.sourceDir;
        ai.dataDir = "/data/user/0/com.example.app";
        ai.nativeLibraryDir = "/data/app/com.example.app/lib/x86_64";
        ai.splitNames = new String[] {"config.xxhdpi"};
        ai.storageUuid = new UUID(0x0123456789abcdefL, 0x7edcba9876543210L);
        ai.setVersionCode(0x100000002L);
        ai.enabled = true;
        ai.category = ApplicationInfo.CATEGORY_PRODUCTIVITY;
        ai.appComponentFactory = "androidx.core.app.CoreComponentFactory";
        // the shape ComputerEngine gives an app of builtin libraries (SystemConfig `<library>`): a path, a name, dependencies that are themselves libraries; one with code paths instead
        SharedLibraryInfo base = new SharedLibraryInfo("/system/framework/android.test.base.jar", null, null, "android.test.base",
                SharedLibraryInfo.VERSION_UNDEFINED, SharedLibraryInfo.TYPE_BUILTIN, null, null, null, false);
        SharedLibraryInfo runner = new SharedLibraryInfo("/system/framework/android.test.runner.jar", null, null, "android.test.runner",
                SharedLibraryInfo.VERSION_UNDEFINED, SharedLibraryInfo.TYPE_BUILTIN, null, null, Arrays.asList(base), false);
        SharedLibraryInfo split = new SharedLibraryInfo(null, "com.example.lib", Arrays.asList("/data/app/a.apk", "/data/app/b.apk"), "com.example.lib",
                7L, SharedLibraryInfo.TYPE_DYNAMIC, null, null, null, false);
        ai.sharedLibraryFiles = new String[] {"/system/framework/android.test.runner.jar", "/system/framework/android.test.base.jar"};
        // as the activity manager hands them over, i.e. as read from a parcel: readParcelableList leaves an empty list where null was written, which Java then writes as a count 0
        ai.sharedLibraryInfos = Arrays.asList(reparceled(runner), reparceled(base), reparceled(split));
        return ai;
    }

    static ActivityInfo activityInfo(ApplicationInfo ai) {
        ActivityInfo a = new ActivityInfo();
        a.name = "com.example.app.Main";
        a.packageName = ai.packageName;
        a.applicationInfo = ai;
        a.processName = ai.processName;
        a.exported = true;
        a.theme = 0x7f0e0002;
        a.launchMode = ActivityInfo.LAUNCH_SINGLE_TOP;
        a.flags = ActivityInfo.FLAG_HARDWARE_ACCELERATED;
        a.screenOrientation = ActivityInfo.SCREEN_ORIENTATION_PORTRAIT;
        a.configChanges = ActivityInfo.CONFIG_ORIENTATION | ActivityInfo.CONFIG_SCREEN_SIZE;
        a.softInputMode = 0x20;
        a.taskAffinity = "com.example.task";
        a.targetActivity = "com.example.app.Target";
        return a;
    }

    static Configuration configuration() {
        Configuration c = new Configuration();
        c.fontScale = 1.25f;
        c.mcc = 466;
        c.mnc = 92;
        c.setLocales(LocaleList.forLanguageTags("zh-TW,en-US"));
        c.densityDpi = 320;
        c.screenWidthDp = 411;
        c.screenHeightDp = 891;
        c.smallestScreenWidthDp = 411;
        c.orientation = Configuration.ORIENTATION_PORTRAIT;
        c.uiMode = Configuration.UI_MODE_TYPE_NORMAL | Configuration.UI_MODE_NIGHT_YES;
        c.windowConfiguration.setBounds(new Rect(0, 0, 1080, 2340));
        c.windowConfiguration.setAppBounds(new Rect(0, 80, 1080, 2340));
        c.seq = 7;
        return c;
    }

    static ProviderInfo providerInfo(ApplicationInfo ai) {
        ProviderInfo p = new ProviderInfo();
        p.name = "com.example.app.Provider";
        p.packageName = ai.packageName;
        p.applicationInfo = ai;
        p.authority = "com.example.provider";
        p.readPermission = "com.example.READ";
        p.exported = true;
        p.grantUriPermissions = true;
        p.initOrder = 5;
        return p;
    }

    static Intent intent() {
        Intent i = new Intent(Intent.ACTION_VIEW);
        i.setDataAndType(Uri.parse("content://com.example.provider/item/1"), "text/plain");
        i.setIdentifier("id-1");
        i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        i.setComponent(new ComponentName("com.example.app", "com.example.app.Main"));
        i.addCategory(Intent.CATEGORY_DEFAULT);
        i.setSourceBounds(new Rect(1, 2, 3, 4));
        return i;
    }

    static PackageInfo packageInfo(ApplicationInfo ai) {
        PackageInfo p = new PackageInfo();
        p.packageName = ai.packageName;
        p.versionName = "1.2.3";
        p.setLongVersionCode(0x100000002L);
        p.applicationInfo = ai;
        p.firstInstallTime = 1700000000000L;
        p.lastUpdateTime = 1700000001000L;
        p.gids = new int[] {3003};
        p.requestedPermissions = new String[] {"android.permission.INTERNET"};
        p.requestedPermissionsFlags = new int[] {PackageInfo.REQUESTED_PERMISSION_GRANTED};
        p.compileSdkVersion = 35;
        return p;
    }

    /// The test package's InstrumentationInfo (AMS's instrumentation record), each field distinct from its default.
    static InstrumentationInfo instrumentationInfo(ApplicationInfo ai) {
        InstrumentationInfo i = new InstrumentationInfo();
        i.name = "com.example.app.test.Runner";
        i.packageName = "com.example.app.test";
        i.nonLocalizedLabel = "Example tests";
        i.icon = 0x7f080004;
        i.targetPackage = ai.packageName;
        i.targetProcesses = "*";
        i.sourceDir = ai.sourceDir;
        i.publicSourceDir = ai.publicSourceDir;
        i.splitNames = new String[] {"config.xxhdpi"};
        i.splitSourceDirs = new String[] {"/data/app/com.example.app/split_config.xxhdpi.apk"};
        i.splitPublicSourceDirs = i.splitSourceDirs;
        i.dataDir = ai.dataDir;
        i.deviceProtectedDataDir = "/data/user_de/0/com.example.app";
        i.credentialProtectedDataDir = ai.dataDir;
        i.primaryCpuAbi = "x86_64";
        i.secondaryCpuAbi = "x86";
        i.nativeLibraryDir = ai.nativeLibraryDir;
        i.secondaryNativeLibraryDir = "/data/app/com.example.app/lib/x86";
        i.handleProfiling = true;
        i.functionalTest = true;
        return i;
    }

    static byte[] marshal(Parcelable p) {
        Parcel parcel = Parcel.obtain();
        p.writeToParcel(parcel, 0);
        byte[] b = parcel.marshall();
        parcel.recycle();
        return b;
    }

    /// Java's reader on `bytes` (every byte consumed), then its writer again.
    static byte[] javaAgain(Parcelable.Creator<?> creator, byte[] bytes) {
        Parcel parcel = Parcel.obtain();
        parcel.unmarshall(bytes, 0, bytes.length);
        parcel.setDataPosition(0);
        Parcelable p = (Parcelable) creator.createFromParcel(parcel);
        int stopped = parcel.dataPosition();
        parcel.recycle();
        if (stopped != bytes.length) {
            throw new IllegalStateException("Java read " + stopped + " of " + bytes.length + " bytes");
        }
        return marshal(p);
    }

    static int firstDiff(byte[] a, byte[] b) {
        int n = Math.min(a.length, b.length);
        for (int i = 0; i < n; i++) {
            if (a[i] != b[i]) return i;
        }
        return n;
    }

    static void result(boolean ok, String name, String detail) {
        System.out.println("pt " + (ok ? "pass " : "fail ") + name + " " + detail);
        if (!ok) failures++;
    }

    static void check(String name, Parcelable p, Parcelable.Creator<?> creator) {
        String cls = p.getClass().getName();
        if (cls.indexOf('$') >= 0) cls = p.getClass().getSuperclass().getName(); // CompatibilityInfo.DEFAULT_COMPATIBILITY_INFO is an anonymous subclass
        byte[] java = marshal(p);
        byte[] rust = roundTrip(cls, java, MODE_PLAIN);
        if (rust == null) {
            result(false, name, "rust: " + lastError());
            return;
        }
        if (!Arrays.equals(java, rust)) {
            result(false, name, "rust bytes differ from java's at byte " + firstDiff(java, rust) + " (" + rust.length + " vs " + java.length + ")");
            return;
        }
        byte[] again;
        try {
            again = javaAgain(creator, rust);
        } catch (RuntimeException e) {
            result(false, name, "java cannot read rust's bytes: " + e);
            return;
        }
        boolean ok = Arrays.equals(java, again);
        result(ok, name, ok ? java.length + " bytes, java -> rust -> java byte-equal" : "java re-encodes rust's bytes differently at byte " + firstDiff(java, again));
    }

    /// Bundle.putAll: the codec merges A and B (both marshalled into one input); the bytes must equal Java's merged Bundle's, and Java must read them back as the merged map.
    static void checkPutAll(String name, Bundle a, Bundle b) {
        Bundle merged = new Bundle(a);
        merged.putAll(b);
        byte[] ab = marshal(a);
        byte[] bb = marshal(b);
        byte[] input = Arrays.copyOf(ab, ab.length + bb.length);
        System.arraycopy(bb, 0, input, ab.length, bb.length);
        byte[] rust = roundTrip("android.os.Bundle", input, MODE_PUT_ALL);
        if (rust == null) {
            result(false, name, "rust: " + lastError());
            return;
        }
        byte[] java = marshal(merged);
        if (!Arrays.equals(java, rust)) {
            result(false, name, "rust bytes differ from java's at byte " + firstDiff(java, rust) + " (" + rust.length + " vs " + java.length + ")");
            return;
        }
        boolean ok;
        try {
            ok = Arrays.equals(java, javaAgain(Bundle.CREATOR, rust));
        } catch (RuntimeException e) {
            result(false, name, "java cannot read rust's bytes: " + e);
            return;
        }
        result(ok, name, ok ? java.length + " bytes, rust putAll == java putAll, byte-equal" : "java re-encodes rust's bytes differently");
    }

    /// java.util.Map, the Java-only builtin, as Parcel.writeMap writes it; Java reads Rust's bytes back with readHashMap.
    static void checkMap(String name, Map<String, String> m) {
        Parcel parcel = Parcel.obtain();
        parcel.writeMap(m);
        byte[] java = parcel.marshall();
        parcel.recycle();
        byte[] rust = roundTrip("java.util.Map", java, MODE_PLAIN);
        if (rust == null) {
            result(false, name, "rust: " + lastError());
            return;
        }
        if (!Arrays.equals(java, rust)) {
            result(false, name, "rust bytes differ from java's at byte " + firstDiff(java, rust) + " (" + rust.length + " vs " + java.length + ")");
            return;
        }
        Parcel in = Parcel.obtain();
        in.unmarshall(rust, 0, rust.length);
        in.setDataPosition(0);
        HashMap back = in.readHashMap(null);
        boolean consumed = in.dataPosition() == rust.length;
        in.recycle();
        boolean ok = consumed && Objects.equals(m, back);
        result(ok, name, ok ? java.length + " bytes, java -> rust byte-equal, java reads rust's map back equal" : "java reads rust's bytes as " + back + " (consumed " + consumed + ")");
    }

    static void selfTests(Configuration config, Intent intent, Bundle entries) {
        byte[] bytes = marshal(config);
        byte[] corrupt = roundTrip("android.content.res.Configuration", bytes, MODE_CORRUPT);
        result(corrupt != null && !Arrays.equals(bytes, corrupt), "self-test:corrupt", "a corrupted encoding differs from java's bytes");
        byte[] wrong = roundTrip("android.content.res.CompatibilityInfo", bytes, MODE_PLAIN);
        result(wrong == null && lastError().contains("stopped at byte"), "self-test:wrong-codec",
                "Configuration bytes through CompatibilityInfo's codec: " + (wrong == null ? lastError() : "accepted"));
        // ClipData is not modelled: refused, counted, and a line on stderr (xtask checks it)
        Intent clip = new Intent(intent);
        clip.setClipData(ClipData.newPlainText("label", "text"));
        byte[] refusedBytes = roundTrip("android.content.Intent", marshal(clip), MODE_PLAIN);
        String counters = refused();
        result(refusedBytes == null && counters.contains("android.content.Intent.clipData="), "self-test:refusal",
                "Intent ClipData refused and counted: " + counters.replace('\n', ' ').trim());
        // mHasIntent 2: Java writes 0 or 1 only (writeBoolean), so the codec refuses the value and names the field
        byte[] flag = marshal(entries);
        ByteBuffer.wrap(flag).order(ByteOrder.nativeOrder()).putInt(flag.length - 4, 2);
        byte[] badFlag = roundTrip("android.os.Bundle", flag, MODE_PLAIN);
        result(badFlag == null && refused().contains("android.os.Bundle.mHasIntent="), "self-test:bundle-flag",
                "mHasIntent 2: " + (badFlag == null ? lastError() : "accepted"));
        // the codec writes a descriptor, then reads it back from the parcel that holds the object (no Java bytes involved)
        byte[] fdSelf = roundTrip("java.io.FileDescriptor", new byte[0], MODE_FD_SELF);
        result(fdSelf != null, "self-test:fd-read",
                "descriptor written and read back by the codec: " + (fdSelf == null ? lastError() : "ok"));
    }
}
