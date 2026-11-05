// SPDX-License-Identifier: MIT
// Driver of `xtask test parcel` on host ART (framework.jar on the boot classpath): for each M2 parcelable, Java's own writeToParcel bytes go through atl-parcelables' codec (libatl_parcel_truth.so:
// decode, every byte consumed, re-encode) and must come back byte-equal; Java's createFromParcel must then read the Rust bytes and write the same bytes again. Never shipped.
// Field values are distinct from the defaults, so a codec that drops, swaps or misreads a field changes the bytes. Output: `pt pass|fail <case> <detail>` lines; exit 0 = all passed.
// Bundles and Intent extras that carry objects go through Java's own Parcels (roundTripParcel: the codec reads and writes the Parcel's data and object table): a binder must come out usable, a descriptor must be reported as Java reports it.
// Self-tests (must fail as stated, else `pt fail`): a corrupted Rust encoding is caught by the comparison, a wrong codec by the consumption check, an Intent with ClipData and a Bundle with a bad mHasIntent by their refusals (each refusal is also a line on stderr, which xtask checks).

import android.app.ActivityManager;
import android.app.ApplicationErrorReport;
import android.app.ContentProviderHolder;
import android.app.servertransaction.ActivityConfigurationChangeItem;
import android.app.servertransaction.ActivityLifecycleItem;
import android.app.servertransaction.ActivityRelaunchItem;
import android.app.servertransaction.ClientTransaction;
import android.app.servertransaction.ClientTransactionItem;
import android.app.servertransaction.DestroyActivityItem;
import android.app.servertransaction.LaunchActivityItem;
import android.app.servertransaction.PauseActivityItem;
import android.app.servertransaction.ResumeActivityItem;
import android.app.servertransaction.StartActivityItem;
import android.app.servertransaction.StopActivityItem;
import android.content.ClipData;
import android.content.ComponentName;
import android.content.Intent;
import android.content.IntentFilter;
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
import android.view.Display;
import android.view.DisplayInfo;
import android.view.InputChannel;
import android.view.InputDevice;
import android.view.KeyCharacterMap;
import android.view.InsetsSource;
import android.view.InsetsSourceControl;
import android.view.InsetsState;
import android.view.PointerIcon;
import android.os.UserHandle;
import android.text.InputType;
import android.view.autofill.AutofillId;
import android.view.inputmethod.EditorInfo;
import android.window.ImeOnBackInvokedDispatcher;
import android.view.SurfaceControl;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.view.WindowRelayoutResult;
import android.window.ClientWindowFrames;
import android.util.MergedConfiguration;
import android.net.Uri;
import android.os.Binder;
import android.os.Bundle;
import android.os.IBinder;
import android.os.LocaleList;
import android.os.Parcel;
import android.os.ParcelFileDescriptor;
import android.os.Parcelable;
import android.os.UserHandle;
import android.os.storage.StorageVolume;
import android.window.ActivityWindowInfo;
import android.window.SizeConfigurationBuckets;
import java.io.File;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.UUID;

public class ParcelTruth {
    static native byte[] roundTrip(String className, byte[] marshalled, int mode);
    /// The KeyCharacterMap's Parcel form (atl-kcm-parcel on Virtual.kcm, args[2]): the InputDevice codec reads its bytes against this. 0 when set, else -1 (lastError).
    static native int setKeyCharacterMap(byte[] map);
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
        // args[2]: the virtual keyboard's map as AOSP's loader writes it; every InputDevice carries these bytes
        byte[] keymap = readFile(args[2]);
        if (setKeyCharacterMap(keymap) != 0) {
            System.out.println("pt fail keymap " + lastError());
            System.exit(1);
        }
        ApplicationInfo ai = applicationInfo();
        ActivityInfo activity = activityInfo(ai);
        Configuration config = configuration();
        ProviderInfo provider = providerInfo(ai);
        Intent intent = intent();

        check("ApplicationInfo", ai, ApplicationInfo.CREATOR);
        check("ActivityInfo", activity, ActivityInfo.CREATOR);
        // the values an activity sends with setTaskDescription (Activity.onApplyThemeResource, every themed launch)
        check("ActivityManager.TaskDescription", taskDescription(), ActivityManager.TaskDescription.CREATOR);
        // the crash report an app's process sends as it dies (RuntimeInit's uncaught-exception handler): nine strings and one int, nulls where unset
        check("ApplicationErrorReport.ParcelableCrashInfo", crashInfo(), ApplicationErrorReport.ParcelableCrashInfo.CREATOR);
        // IDisplayManager.getDisplayInfo's record (DisplayInfo.writeToParcel): every field set off its default, its Mode arrays in use
        check("DisplayInfo", displayInfo(), DisplayInfo.CREATOR);
        check("Configuration", config, Configuration.CREATOR);
        check("CompatibilityInfo", CompatibilityInfo.DEFAULT_COMPATIBILITY_INFO, CompatibilityInfo.CREATOR);
        check("Intent", intent, Intent.CREATOR);
        Intent withSelector = new Intent(Intent.ACTION_MAIN);
        withSelector.setSelector(new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_APP_BROWSER));
        check("Intent+selector", withSelector, Intent.CREATOR);
        // extras (Intent.writeToParcel: writeBundle(mExtras)): a non-null Bundle after the first putExtra
        check("Intent:extras", new Intent(intent).putExtra("k", 1).putExtra("s", "v"), Intent.CREATOR);
        // IActivityManager.registerReceiverWithFeature's filter: the lists the codec models, priority, order, autoVerify
        IntentFilter filter = new IntentFilter("com.example.A");
        filter.addAction("com.example.B");
        filter.addCategory(Intent.CATEGORY_DEFAULT);
        filter.addDataScheme("content");
        try {
            filter.addDataType("text/plain");
        } catch (IntentFilter.MalformedMimeTypeException e) {
            throw new IllegalStateException(e);
        }
        filter.setPriority(5);
        filter.setOrder(3);
        filter.setAutoVerify(true);
        check("IntentFilter", filter, IntentFilter.CREATOR);
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
        // the lifecycle item a launch is followed by (ActivityTaskSupervisor.realStartActivityLocked: Launch, then Resume)
        checkTransactionResume("ClientTransaction:resume");
        // the items of a configuration change (ActivityRecord.scheduleConfigurationChanged, relaunchActivityLocked)
        IBinder itemToken = new Echo();
        checkTransactionItem("ClientTransaction:configuration-change",
                new ActivityConfigurationChangeItem(itemToken, config, new ActivityWindowInfo()), itemToken);
        checkTransactionItem("ClientTransaction:relaunch",
                new ActivityRelaunchItem(itemToken, null, null, ActivityInfo.CONFIG_SCREEN_SIZE, new MergedConfiguration(config, config), true, new ActivityWindowInfo()), itemToken);
        // the lifecycle items of an activity switch (ActivityRecord.startPausing, stopIfPossible, destroyIfPossible; a start of a stopped record: ActivityRecord.java:6055)
        checkTransactionItem("ClientTransaction:pause", new PauseActivityItem(itemToken, true, false, true, false), itemToken);
        checkTransactionItem("ClientTransaction:stop", new StopActivityItem(itemToken), itemToken);
        checkTransactionItem("ClientTransaction:destroy", new DestroyActivityItem(itemToken, true), itemToken);
        checkTransactionItem("ClientTransaction:start", new StartActivityItem(itemToken, null), itemToken);
        // ActivityThread.reportSizeConfigurations: an app with no size-qualified resource sends the flags alone; a full one every array and the long bit
        check("SizeConfigurationBuckets:none", new SizeConfigurationBuckets(new Configuration[0]), SizeConfigurationBuckets.CREATOR);
        Configuration sized = new Configuration();
        sized.screenWidthDp = 800;
        sized.screenHeightDp = 480;
        sized.smallestScreenWidthDp = 600;
        sized.screenLayout = Configuration.SCREENLAYOUT_SIZE_LARGE | Configuration.SCREENLAYOUT_LONG_YES;
        check("SizeConfigurationBuckets:all", new SizeConfigurationBuckets(new Configuration[] {sized}), SizeConfigurationBuckets.CREATOR);
        Configuration smallestOnly = new Configuration();
        smallestOnly.screenWidthDp = Configuration.SCREEN_WIDTH_DP_UNDEFINED;
        smallestOnly.smallestScreenWidthDp = 600;
        check("SizeConfigurationBuckets:smallest", new SizeConfigurationBuckets(new Configuration[] {smallestOnly}), SizeConfigurationBuckets.CREATOR);
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

        // the window path (M3 slice 5): the LayoutParams IWindowSession receives, and the replies it sends (addToDisplayAsUser, relayout), each field off its default
        check("WindowManager.LayoutParams", layoutParams(), WindowManager.LayoutParams.CREATOR);
        check("InsetsState", insetsState(), InsetsState.CREATOR);
        check("ClientWindowFrames", clientWindowFrames(), ClientWindowFrames.CREATOR);
        check("MergedConfiguration", mergedConfiguration(config), MergedConfiguration.CREATOR);
        check("SurfaceControl", new SurfaceControl(), SurfaceControl.CREATOR);
        checkSurfaceControlPresent("SurfaceControl:native");
        check("InsetsSourceControl.Array", new InsetsSourceControl.Array(), InsetsSourceControl.Array.CREATOR);
        check("WindowRelayoutResult", windowRelayoutResult(config), WindowRelayoutResult.CREATOR);
        // the desktop's device list (atl-services input.rs): the virtual keyboard (-1) and the pointer, both with the staged map
        check("InputDevice", inputDevice(-1, "Virtual", "atl:virtual-keyboard", InputDevice.SOURCE_KEYBOARD, InputDevice.KEYBOARD_TYPE_ALPHABETIC, keymap), InputDevice.CREATOR);
        check("InputDevice:pointer", inputDevice(1, "ATL pointer", "atl:pointer", InputDevice.SOURCE_MOUSE, 0, keymap), InputDevice.CREATOR);
        check("Rect", new Rect(1, 2, 3, 4), Rect.CREATOR);
        // the mount service's volume as StorageService builds it (StorageVolume.Builder: internal path = path; owner = user 0)
        check("StorageVolume", new StorageVolume.Builder("emulated", new File("/storage/emulated/0"), "Internal shared storage", UserHandle.SYSTEM, "mounted").setPrimary(true).setEmulated(true).build(), StorageVolume.CREATOR);
        checkInputChannel("InputChannel");
        check("PointerIcon", systemIcon(PointerIcon.TYPE_ARROW), PointerIcon.CREATOR);
        // the focus path: InputMethodManager.startInputInner sends both on every window focus gain (atl-services input_method.rs)
        check("EditorInfo", editorInfo(), EditorInfo.CREATOR);
        check("EditorInfo:empty", new EditorInfo(), EditorInfo.CREATOR);
        checkImeDispatcher("ImeOnBackInvokedDispatcher");

        selfTests(config, intent, entries);
        System.out.println("pt summary " + failures + " failed");
        System.exit(failures == 0 ? 0 : 1);
    }

    /// An EditorInfo with every field off its default: autofill id, extras, surrounding text, hint locales, MIME types, target user.
    static EditorInfo editorInfo() {
        EditorInfo e = new EditorInfo();
        e.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES;
        e.imeOptions = EditorInfo.IME_ACTION_DONE;
        e.privateImeOptions = "com.example.opt";
        e.actionLabel = "Go";
        e.actionId = 7;
        e.initialSelStart = 1;
        e.initialSelEnd = 3;
        e.initialCapsMode = 0x2000;
        e.hintText = "hint";
        e.label = "label";
        e.packageName = "com.example.app";
        e.autofillId = new AutofillId(42);
        e.fieldId = 42;
        e.fieldName = "name";
        e.extras = new Bundle();
        e.extras.putString("key", "value");
        e.setInitialSurroundingText("hello world");
        e.hintLocales = LocaleList.forLanguageTags("en-US,fr");
        e.contentMimeTypes = new String[] {"image/png"};
        e.targetInputMethodUser = UserHandle.of(10);
        return e;
    }

    /// ImeOnBackInvokedDispatcher carries its ResultReceiver's binder: Java Parcel -> codec -> Java Parcel with its objects (roundTripParcel); every byte consumed, Java reads it back,
    /// and the binder Java re-writes from the read value is the original one.
    static void checkImeDispatcher(String name) {
        ImeOnBackInvokedDispatcher d = new ImeOnBackInvokedDispatcher(null);
        Parcel in = written(d);
        in.readInt();
        IBinder sent = in.readStrongBinder();
        in.setDataPosition(0);
        Parcel out = Parcel.obtain();
        if (roundTripParcel("android.window.ImeOnBackInvokedDispatcher", in, out) != 0) {
            result(false, name, "rust: " + lastError());
            return;
        }
        boolean consumed = out.dataSize() == in.dataSize();
        out.setDataPosition(0);
        ImeOnBackInvokedDispatcher back = ImeOnBackInvokedDispatcher.CREATOR.createFromParcel(out);
        Parcel again = written(back);
        again.readInt();
        IBinder got = again.readStrongBinder();
        boolean ok = consumed && sent != null && got == sent;
        result(ok, name, ok ? "result receiver's binder survived java -> rust -> java" : "sizes " + in.dataSize() + "/" + out.dataSize() + ", binder " + got + " vs " + sent);
        in.recycle();
        out.recycle();
        again.recycle();
    }

    /// A system icon as a receiver gets one: getSystemIcon(int) is private, and the public overload loads resources through a Context.
    static PointerIcon systemIcon(int type) {
        Parcel p = Parcel.obtain();
        p.writeInt(type);
        p.setDataPosition(0);
        PointerIcon icon = PointerIcon.CREATOR.createFromParcel(p);
        p.recycle();
        return icon;
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

    /// A ClientTransaction holding a ResumeActivityItem (the item ATL sends after a launch): its activity token is a binder (ActivityTransactionItem requires one), so the transaction goes
    /// Java Parcel -> codec -> Java Parcel with its objects (roundTripParcel). Java must read back one item equal to the original (same token object, same procState, isForward and
    /// compat flag, equals() compares them), a lifecycle item whose target state is ON_RESUME, and a token that still answers.
    static void checkTransactionResume(String name) {
        IBinder token = new Echo();
        ResumeActivityItem resume = new ResumeActivityItem(token, ActivityManager.PROCESS_STATE_UNKNOWN, true, false);
        ClientTransaction transaction = new ClientTransaction();
        transaction.addTransactionItem(resume);
        Parcel in = written(transaction);
        Parcel out = Parcel.obtain();
        if (roundTripParcel("android.app.servertransaction.ClientTransaction", in, out) != 0) {
            result(false, name, "rust: " + lastError());
            return;
        }
        out.setDataPosition(0);
        List<ClientTransactionItem> items = ClientTransaction.CREATOR.createFromParcel(out).getTransactionItems();
        boolean ok = items.size() == 1 && resume.equals(items.get(0))
                && ((ActivityLifecycleItem) items.get(0)).getTargetState() == ActivityLifecycleItem.ON_RESUME
                && usable(token);
        boolean consumed = out.dataSize() == in.dataSize();
        result(ok && consumed, name, ok && consumed ? "resume item survived java -> rust -> java: equal fields, same token, ON_RESUME, answers a transaction"
                : "items " + items + ", sizes " + in.dataSize() + "/" + out.dataSize());
        in.recycle();
        out.recycle();
    }

    /// A ClientTransaction holding `item`, whose token is `token`: Java Parcel -> codec -> Java Parcel with its objects, as checkTransactionResume. Java must read back one equal item (its equals() compares every field), the same size, and a token that answers.
    static void checkTransactionItem(String name, ClientTransactionItem item, IBinder token) {
        ClientTransaction transaction = new ClientTransaction();
        transaction.addTransactionItem(item);
        Parcel in = written(transaction);
        Parcel out = Parcel.obtain();
        if (roundTripParcel("android.app.servertransaction.ClientTransaction", in, out) != 0) {
            result(false, name, "rust: " + lastError());
            return;
        }
        out.setDataPosition(0);
        List<ClientTransactionItem> items = ClientTransaction.CREATOR.createFromParcel(out).getTransactionItems();
        boolean ok = items.size() == 1 && item.equals(items.get(0)) && usable(token) && out.dataSize() == in.dataSize();
        result(ok, name, ok ? "item survived java -> rust -> java: equal fields, same token, answers a transaction" : "items " + items + ", sizes " + in.dataSize() + "/" + out.dataSize());
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

    /// Sets a private or package field of a framework object (the values the public API does not set; a field the release lacks fails the run).
    static void setField(Object target, String field, Object value) {
        try {
            java.lang.reflect.Field f = target.getClass().getDeclaredField(field);
            f.setAccessible(true);
            f.set(target, value);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(field + ": " + e, e);
        }
    }

    /// Every LayoutParams field is off its default, the nullable ones included where a non-null value is modelled; providedInsets and paramsForRotation stay null.
    static WindowManager.LayoutParams layoutParams() {
        WindowManager.LayoutParams lp = new WindowManager.LayoutParams(WindowManager.LayoutParams.TYPE_BASE_APPLICATION, 0x01020304);
        lp.width = 1280;
        lp.height = 800;
        lp.x = -3;
        lp.y = 5;
        setField(lp, "privateFlags", 0x10);
        lp.softInputMode = 0x21;
        lp.layoutInDisplayCutoutMode = 1;
        lp.gravity = 0x33;
        lp.horizontalMargin = 0.25f;
        lp.verticalMargin = 0.5f;
        lp.format = 1;
        lp.windowAnimations = 0x7f010001;
        lp.alpha = 0.75f;
        lp.dimAmount = 0.5f;
        lp.screenBrightness = 0.125f;
        lp.buttonBrightness = 0.875f;
        lp.rotationAnimation = 2;
        lp.packageName = "com.example.atlwindow";
        setField(lp, "mTitle", "com.example.atlwindow/Main");
        lp.screenOrientation = 1;
        lp.preferredRefreshRate = 60f;
        lp.preferredDisplayModeId = 3;
        lp.preferredMinDisplayRefreshRate = 30f;
        lp.preferredMaxDisplayRefreshRate = 120f;
        lp.systemUiVisibility = 0x100;
        lp.subtreeSystemUiVisibility = 0x200;
        setField(lp, "hasSystemUiListeners", true);
        lp.inputFeatures = 0x4;
        lp.userActivityTimeout = 5000000000L;
        lp.surfaceInsets.set(1, 2, 3, 4);
        setField(lp, "hasManualSurfaceInsets", true);
        setField(lp, "receiveInsetsIgnoringZOrder", true);
        setField(lp, "preservePreviousSurfaceInsets", false);
        setField(lp, "accessibilityIdOfAnchor", 0x1122334455L);
        setField(lp, "accessibilityTitle", "a11y title");
        setField(lp, "mColorMode", 1);
        setField(lp, "hideTimeoutMilliseconds", 7000L);
        setField(lp.insetsFlags, "appearance", 0x5);
        setField(lp.insetsFlags, "behavior", 0x3);
        setField(lp, "mFitInsetsTypes", 0x7);
        setField(lp, "mFitInsetsSides", 0xf);
        setField(lp, "mFitInsetsIgnoringVisibility", true);
        setField(lp, "preferMinimalPostProcessing", true);
        setField(lp, "mBlurBehindRadius", 9);
        setField(lp, "mWallpaperTouchEventsEnabled", true);
        setField(lp, "forciblyShownTypes", 0x2);
        setField(lp, "mDisplayFlags", 0x3);
        setField(lp, "mDesiredHdrHeadroom", 2.5f);
        setField(lp, "mFrameRateBoostOnTouch", true);
        setField(lp, "mIsFrameRatePowerSavingsBalanced", true);
        return lp;
    }

    /// A bar and a navigation bar: one with a visible frame, flags, side hint and bounding rects; the other with the defaults.
    static InsetsSource insetsSource() {
        InsetsSource bar = new InsetsSource(0x11, WindowInsets.Type.statusBars());
        bar.setFrame(0, 0, 1280, 24);
        bar.setVisibleFrame(new Rect(0, 0, 1280, 20));
        bar.setVisible(true);
        bar.setFlags(0x2);
        setField(bar, "mSideHint", 0x5);
        bar.setBoundingRects(new Rect[] {new Rect(1, 2, 3, 4)});
        return bar;
    }

    static InsetsState insetsState() {
        InsetsState s = new InsetsState();
        s.setDisplayFrame(new Rect(0, 0, 1280, 800));
        s.setRoundedCornerFrame(new Rect(0, 0, 1280, 800));
        s.setSeq(7);
        s.addSource(insetsSource());
        InsetsSource nav = new InsetsSource(0x22, WindowInsets.Type.navigationBars());
        nav.setFrame(0, 776, 1280, 800);
        s.addSource(nav);
        return s;
    }

    static ClientWindowFrames clientWindowFrames() {
        ClientWindowFrames f = new ClientWindowFrames();
        f.frame.set(0, 0, 1280, 800);
        f.displayFrame.set(0, 0, 1280, 800);
        f.parentFrame.set(0, 24, 1280, 776);
        f.attachedFrame = new Rect(10, 20, 30, 40);
        f.isParentFrameClippedByDisplayCutout = true;
        f.compatScale = 0.75f;
        f.seq = 5;
        return f;
    }

    static MergedConfiguration mergedConfiguration(Configuration config) {
        Configuration override = new Configuration();
        override.densityDpi = 240;
        override.screenWidthDp = 411;
        return new MergedConfiguration(config, override);
    }

    static WindowRelayoutResult windowRelayoutResult(Configuration config) {
        WindowRelayoutResult r = new WindowRelayoutResult(clientWindowFrames(), mergedConfiguration(config), new SurfaceControl(), insetsState(), new InsetsSourceControl.Array());
        r.syncSeqId = 9;
        r.activityWindowInfo = new ActivityWindowInfo();
        return r;
    }

    /// A SurfaceControl with a native layer, as the window path's relayout reply carries it. The reference is written by hand in the layout SurfaceControl.java (writeToParcel)
    /// and libgui's SurfaceControl::writeToParcel define; Rust decodes it and re-encodes it, word for word, binder objects included. Not read with the CREATOR here: that makes
    /// libgui's SurfaceComposerClient, which waits for "SurfaceFlingerAIDL" (absent in this gate). The window gate has AOSP's Java and libgui read the bytes Rust writes.
    static void checkSurfaceControlPresent(String name) {
        IBinder client = new Binder();
        IBinder handle = new Binder();
        Parcel reference = Parcel.obtain();
        reference.writeString8("window-layer");
        reference.writeInt(1280);
        reference.writeInt(800);
        reference.writeInt(1);
        reference.writeStrongBinder(client);
        reference.writeStrongBinder(handle);
        reference.writeInt(7);
        reference.writeString("window-layer");
        reference.writeInt(0);
        reference.writeInt(1280);
        reference.writeInt(800);
        reference.writeInt(1);
        int[] want = words(reference);
        Parcel rust = Parcel.obtain();
        if (roundTripParcel("android.view.SurfaceControl", reference, rust) != 0) {
            result(false, name, "rust: " + lastError());
            return;
        }
        int[] back = words(rust);
        boolean ok = Arrays.equals(want, back);
        result(ok, name, ok ? want.length * 4 + " bytes: java's reference -> rust -> byte-equal, native layer and binders kept" : "rust re-encodes " + back.length + " words, the reference " + want.length + ", first difference at word " + firstWordDiff(want, back));
        reference.recycle();
        rust.recycle();
    }

    /// The parcel's data as ints from position 0; the position is left at 0.
    static int[] words(Parcel p) {
        p.setDataPosition(0);
        int[] w = new int[p.dataSize() / 4];
        for (int i = 0; i < w.length; i++) {
            w[i] = p.readInt();
        }
        p.setDataPosition(0);
        return w;
    }

    static int firstWordDiff(int[] a, int[] b) {
        int n = Math.min(a.length, b.length);
        for (int i = 0; i < n; i++) {
            if (a[i] != b[i]) {
                return i;
            }
        }
        return n;
    }

    /// An input channel pair's client end: Java -> codec -> Java with its descriptor and token (roundTripParcel); Java reads the copy back with the same name and a token.
    static void checkInputChannel(String name) {
        InputChannel[] pair = InputChannel.openInputChannelPair("atl-test");
        InputChannel client = pair[1];
        Parcel in = written(client);
        Parcel out = Parcel.obtain();
        if (roundTripParcel("android.view.InputChannel", in, out) != 0) {
            result(false, name, "rust: " + lastError());
            dumpParcel(name, in);
            return;
        }
        boolean consumed = out.dataSize() == in.dataSize();
        out.setDataPosition(0);
        InputChannel back = InputChannel.CREATOR.createFromParcel(out);
        boolean ok = consumed && "atl-test".equals(back.getName()) && back.getToken() != null;
        result(ok, name, ok ? "input channel survived java -> rust -> java: same name, a token, descriptor copied" : "name " + back.getName() + ", consumed " + consumed + ", sizes " + in.dataSize() + "/" + out.dataSize());
        in.recycle();
        out.recycle();
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

    /// The default display's DisplayInfo as a test value (1280x800 at 60 Hz, 160 dpi: the model default of atl-services' display_model.rs): each field off its default, the nested
    /// parcelables null (the codec models them null only).
    static DisplayInfo displayInfo() {
        DisplayInfo info = new DisplayInfo();
        info.layerStack = 1;
        info.flags = 0x20;
        info.type = Display.TYPE_INTERNAL;
        info.displayId = Display.DEFAULT_DISPLAY;
        info.displayGroupId = 3;
        info.name = "ATL display";
        info.uniqueId = "local:0";
        info.appWidth = 1280;
        info.appHeight = 800;
        info.smallestNominalAppWidth = 800;
        info.smallestNominalAppHeight = 800;
        info.largestNominalAppWidth = 1280;
        info.largestNominalAppHeight = 1280;
        info.logicalWidth = 1280;
        info.logicalHeight = 800;
        info.rotation = 1;
        info.modeId = 1;
        info.renderFrameRate = 60f;
        info.hasArrSupport = true;
        info.supportedRefreshRates = new float[] {60f, 30f};
        info.defaultModeId = 1;
        info.userPreferredModeId = 2;
        info.supportedModes = new Display.Mode[] {
            new Display.Mode(1, 1280, 800, 60f, 60f, false, new float[] {30f, 48f}, new int[] {1, 3}),
            new Display.Mode(2, 1920, 1080, 50f, 49.9f, true, new float[0], new int[0]),
        };
        info.appsSupportedModes = new Display.Mode[] {
            new Display.Mode(1, 1280, 800, 60f),
        };
        info.colorMode = 1;
        info.supportedColorModes = new int[] {0, 7};
        info.isForceSdr = true;
        info.minimalPostProcessingSupported = true;
        info.logicalDensityDpi = 160;
        info.physicalXDpi = 159.5f;
        info.physicalYDpi = 160.5f;
        info.appVsyncOffsetNanos = 1000L;
        info.presentationDeadlineNanos = 2000000L;
        info.state = Display.STATE_ON;
        info.committedState = Display.STATE_ON;
        info.ownerUid = 10123;
        info.ownerPackageName = "com.example.owner";
        info.removeMode = 1;
        info.refreshRateOverride = 0f;
        info.brightnessMinimum = 0.1f;
        info.brightnessMaximum = 1.0f;
        info.brightnessDefault = 0.5f;
        info.brightnessDim = 0.2f;
        info.userDisabledHdrTypes = new int[] {2};
        info.installOrientation = 1;
        info.hdrSdrRatio = 1.5f;
        info.thermalBrightnessThrottlingDataId = "default";
        info.canHostTasks = true;
        return info;
    }

    /// ApplicationErrorReport.ParcelableCrashInfo as the uncaught-exception handler fills it: text in UTF-16 with one non-ASCII character, the handler's class set and the tag null.
    static ApplicationErrorReport.ParcelableCrashInfo crashInfo() {
        ApplicationErrorReport.ParcelableCrashInfo c = new ApplicationErrorReport.ParcelableCrashInfo();
        c.exceptionHandlerClassName = "com.android.internal.os.RuntimeInit$KillApplicationHandler";
        c.exceptionClassName = "java.lang.IllegalStateException";
        c.exceptionMessage = "IInputMethodManager is not available é";
        c.throwFileName = "InputMethodManager.java";
        c.throwClassName = "android.view.inputmethod.InputMethodManager";
        c.throwMethodName = "createRealInstance";
        c.throwLineNumber = 1640;
        c.stackTrace = "java.lang.IllegalStateException: IInputMethodManager is not available é\n\tat android.view.inputmethod.InputMethodManager.createRealInstance(InputMethodManager.java:1640)";
        c.crashTag = null;
        return c;
    }

    /// ActivityManager.TaskDescription with every field off its default (colours opaque, as the setters require; no icon, which the codec does not model).
    static ActivityManager.TaskDescription taskDescription() {
        ActivityManager.TaskDescription td = new ActivityManager.TaskDescription();
        td.setLabel("ATL window test");
        td.setPrimaryColor(0xff1565c0);
        td.setBackgroundColor(0xfffafafa);
        td.setBackgroundColorFloating(0xfff0f0f0);
        td.setStatusBarColor(0xff0d47a1);
        td.setNavigationBarColor(0xff000000);
        td.setSystemBarsAppearance(8);
        td.setTopOpaqueSystemBarsAppearance(4);
        td.setEnsureStatusBarContrastWhenTransparent(true);
        td.setEnsureNavigationBarContrastWhenTransparent(false);
        td.setResizeMode(2);
        td.setMinWidth(320);
        td.setMinHeight(480);
        td.setIconFilename("icon.png");
        return td;
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

    static byte[] readFile(String path) {
        try {
            return java.nio.file.Files.readAllBytes(java.nio.file.Paths.get(path));
        } catch (java.io.IOException e) {
            throw new IllegalStateException(path + ": " + e);
        }
    }

    /// KeyCharacterMap's Java framing (android_view_KeyCharacterMap.cpp nativeWriteToParcel): deviceId, present, then libinput's form (the staged bytes).
    static byte[] framedMap(int deviceId, byte[] keymap) {
        return java.nio.ByteBuffer.allocate(8 + keymap.length).order(java.nio.ByteOrder.LITTLE_ENDIAN)
                .putInt(deviceId).putInt(1).put(keymap).array();
    }

    /// An InputDevice as the services build it (Builder values; the map read through KeyCharacterMap.CREATOR from the framed staged bytes, as Java's own reader does).
    static InputDevice inputDevice(int id, String name, String descriptor, int sources, int keyboardType, byte[] keymap) {
        Parcel in = Parcel.obtain();
        byte[] framed = framedMap(id, keymap);
        in.unmarshall(framed, 0, framed.length);
        in.setDataPosition(0);
        KeyCharacterMap kcm = KeyCharacterMap.CREATOR.createFromParcel(in);
        in.recycle();
        return new InputDevice.Builder()
                .setId(id)
                .setGeneration(1)
                .setName(name)
                .setDescriptor(descriptor)
                .setSources(sources)
                .setKeyboardType(keyboardType)
                .setKeyCharacterMap(kcm)
                .setEnabled(true)
                .build();
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

    /// Failure dump: `bytes` as hex, 16 to a line (`hex <case> <offset>: <words>`), so a codec can be walked field by field against the bytes Java wrote.
    static void dumpHex(String name, byte[] bytes) {
        for (int off = 0; off < bytes.length; off += 16) {
            StringBuilder sb = new StringBuilder();
            for (int i = off; i < Math.min(off + 16, bytes.length); i++) {
                if (i % 4 == 0) sb.append(' ');
                sb.append(String.format("%02x", bytes[i] & 0xff));
            }
            System.out.println("hex " + name + " " + String.format("%04x", off) + ":" + sb);
        }
    }

    /// Failure dump of a Parcel that holds objects (binders, FDs), which marshall() refuses: its data is read back word by word from the start.
    static void dumpParcel(String name, Parcel p) {
        int n = p.dataSize() / 4 * 4;
        int at = p.dataPosition();
        p.setDataPosition(0);
        byte[] bytes = new byte[n];
        for (int i = 0; i < n; i += 4) {
            int w = p.readInt();
            bytes[i] = (byte) w;
            bytes[i + 1] = (byte) (w >> 8);
            bytes[i + 2] = (byte) (w >> 16);
            bytes[i + 3] = (byte) (w >> 24);
        }
        p.setDataPosition(at);
        dumpHex(name, bytes);
    }

    static void result(boolean ok, String name, String detail) {
        System.out.println("pt " + (ok ? "pass " : "fail ") + name + " " + detail);
        if (!ok) failures++;
    }

    static void check(String name, Parcelable p, Parcelable.Creator<?> creator) {
        String cls = p.getClass().getName();
        if (p.getClass().isAnonymousClass()) cls = p.getClass().getSuperclass().getName(); // CompatibilityInfo.DEFAULT_COMPATIBILITY_INFO is an anonymous subclass; a nested class keeps its `$` name
        byte[] java = marshal(p);
        byte[] rust = roundTrip(cls, java, MODE_PLAIN);
        if (rust == null) {
            result(false, name, "rust: " + lastError());
            dumpHex(name, java);
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
