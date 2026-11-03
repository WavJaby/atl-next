// SPDX-License-Identifier: MIT
// Driver of `xtask test compositor` on host ART (framework.jar on the boot classpath): atl-compositor's codecs against AOSP's own libgui C++ (libandroid_runtime's SurfaceControl JNI over libgui), both directions. Never shipped.
//   writer:*   Java builds a SurfaceControl.Transaction with real setters (distinct values); libgui's C++ writer marshals it; libatl_parcel_truth decodes it with atl-compositor, checks every value Java set,
//              and re-encodes: bytes and object table must equal libgui's.
//   reader:*   atl-compositor encodes a fully populated apply-shaped TransactionState; libgui's C++ reader (Transaction.CREATOR) decodes it and its C++ writer encodes it again: equal except fd numbers.
//   callback:* atl-compositor encodes onTransactionCompleted / onReleaseBuffer; libgui's C++ BnTransactionCompletedListener reads them and runs the callbacks Java registered.
//   self-test:* each comparison must fail where a field is corrupted, swapped or dropped (else `ct fail`).
// Output: `ct pass|fail <case> <detail>` lines; `ct summary` last; exit 0 = all passed.
// Hardware buffers need gralloc (M3 slice 2): BufferData with a GraphicBuffer is checked against the source only (atl-compositor's unit tests); the null-buffer and cached forms are checked here.

import android.graphics.Rect;
import android.os.Binder;
import android.os.IBinder;
import android.os.Parcel;
import android.view.SurfaceControl;
import android.view.WindowInsets;
import java.io.FileDescriptor;
import java.io.FileInputStream;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.concurrent.atomic.AtomicLong;

public class CompositorTruth {
    static native int startReg();
    static native String lastError();
    /// The data and object table of a Java Parcel: [u32 len][data][u32 n][u32 offsets...] (WireParcel::to_dump).
    static native byte[] dump(Parcel p);
    /// A Java `Transaction.writeToParcel` dump decoded with atl-compositor (state + libgui's listener suffix, every byte consumed) and re-encoded; `expected` = name, value pairs. "ok ..." or "fail ...".
    static native String checkTransaction(byte[] dump, String[] expected, int mode);
    /// atl-compositor's encoding of a fully populated apply-shaped transaction (scenario 0), or the same with a field dropped (scenario 1), as Java's Transaction parcel (leading int 1, state, empty listener suffix).
    static native byte[] rustTransaction(int scenario);
    /// Equal data and object offsets, except the fd number inside fd objects: "ok" or the first difference.
    static native String compareDumps(byte[] a, byte[] b);
    /// Byte offset in a Java Transaction dump where libgui's listener suffix starts; -1 on failure (lastError).
    static native int suffixOffset(byte[] dump);
    /// onTransactionCompleted's call parcel for the callbacks and surfaces in `txDump`'s suffix, with the given latch time and frame number.
    static native byte[] rustCompleted(byte[] txDump, long latchTime, long frameNumber);
    /// onReleaseBuffer's call parcel.
    static native byte[] rustRelease(long bufferId, long frameNumber, int maxAcquired);

    static final int MODE_PLAIN = 0;
    static final int MODE_CORRUPT = 1;
    static final int MODE_SWAP_EXPECTED = 2;
    static final int BINDER_TYPE_FD = 0x66642a85;
    static final int ON_TRANSACTION_COMPLETED = 1;
    static final int ON_RELEASE_BUFFER = 2;
    /// Generous for a few dozen JNI calls (the whole run takes seconds).
    static final long WATCHDOG_MS = 40_000;
    static volatile String step = "start";
    static int failures = 0;
    static int cases = 0;

    public static void main(String[] args) throws Exception {
        // args: libatl_parcel_truth.so, libandroid_runtime.so [, directory to write the libgui-written dumps to]
        System.load(args[0]);
        System.load(args[1]);
        int reg = startReg();
        if (reg != 0) {
            System.out.println("ct fail startReg returned " + reg);
            System.exit(1);
        }
        String goldenDir = args.length > 2 ? args[2] : null;
        // a driver stuck in libgui or binder must end the run with its threads' stacks, not hang the stage
        Thread watchdog = new Thread(() -> {
            try {
                Thread.sleep(WATCHDOG_MS);
            } catch (InterruptedException e) {
                return;
            }
            System.out.println("ct fail watchdog no progress after " + WATCHDOG_MS + " ms; last step: " + step);
            for (java.util.Map.Entry<Thread, StackTraceElement[]> t : Thread.getAllStackTraces().entrySet()) {
                System.out.println("ct thread " + t.getKey().getName());
                for (StackTraceElement e : t.getValue()) System.out.println("ct   at " + e);
            }
            System.out.flush();
            Runtime.getRuntime().halt(2);
        });
        watchdog.setDaemon(true);
        watchdog.start();

        // libgui's SurfaceComposerClient waits for SurfaceFlinger's AIDL service; nothing is ever called on these
        step = "register stand-in SurfaceFlinger services";
        android.os.ServiceManager.addService("SurfaceFlingerAIDL", new Binder());
        android.os.ServiceManager.addService("SurfaceFlinger", new Binder());
        step = "fixture";
        Fixture f = new Fixture();
        step = "write transaction";
        byte[] written = f.transaction.writeToParcelDump();
        step = "writer cases";
        if (goldenDir != null) {
            java.nio.file.Files.write(java.nio.file.Paths.get(goldenDir, "transaction_libgui_writer.dump"), written);
        }
        report("writer:layer-fields", checkTransaction(written, f.expected(false), MODE_PLAIN));
        // a transaction at AOSP's defaults but for one position: the defaults of every other field are libgui's own
        report("writer:minimal", checkTransaction(f.minimalDump, f.minimalExpected(), MODE_PLAIN));

        // reader direction: Rust encodes everything, libgui reads and writes it back
        step = "reader case";
        readerCase("reader:full-state", 0, true);

        // callbacks: libgui's listener runs what Rust encodes
        step = "completed case";
        completedCase(f);
        step = "release case";
        releaseCase(f);
        step = "self-tests";

        // self-tests: the comparisons must be able to fail
        selfTest("self-test:corrupt", checkTransaction(written, f.expected(false), MODE_CORRUPT), "differ");
        selfTest("self-test:swapped-values", checkTransaction(written, f.expected(false), MODE_SWAP_EXPECTED), "expected");
        readerCase("self-test:dropped-field", 1, false);

        System.out.println("ct summary " + failures + " failed of " + cases);
        System.exit(failures == 0 ? 0 : 1);
    }

    // ---- reporting

    static void report(String name, String result) {
        cases++;
        if (result != null && result.startsWith("ok")) {
            System.out.println("ct pass " + name + " " + result.substring(2).trim());
        } else {
            failures++;
            System.out.println("ct fail " + name + " " + result + " | " + lastError());
        }
    }

    static void selfTest(String name, String result, String mustMention) {
        cases++;
        if (result != null && result.startsWith("fail") && result.contains(mustMention)) {
            System.out.println("ct pass " + name + " refused as intended: " + result);
        } else {
            failures++;
            System.out.println("ct fail " + name + " expected a failure mentioning '" + mustMention + "', got: " + result);
        }
    }

    // ---- fixture: a Transaction with distinct values

    static SurfaceControl surface(String name, int layerId, IBinder handle) {
        Parcel p = Parcel.obtain();
        p.writeString8(name);
        p.writeInt(100);
        p.writeInt(50);
        p.writeInt(1);
        p.writeStrongBinder(new Binder()); // the ISurfaceComposerClient
        p.writeStrongBinder(handle);
        p.writeInt(layerId);
        p.writeString(name);
        p.writeInt(0); // transform hint
        p.writeInt(100);
        p.writeInt(50);
        p.writeInt(1); // format
        p.setDataPosition(0);
        SurfaceControl sc = SurfaceControl.CREATOR.createFromParcel(p);
        p.recycle();
        return sc;
    }

    static long f(float v) {
        return Float.floatToRawIntBits(v) & 0xffffffffL;
    }

    static class TxHolder {
        final SurfaceControl.Transaction t;
        TxHolder(SurfaceControl.Transaction t) { this.t = t; }
        byte[] writeToParcelDump() {
            Parcel out = Parcel.obtain();
            t.writeToParcel(out, 0);
            byte[] d = dump(out);
            out.recycle();
            return d;
        }
    }

    static class Fixture {
        final Binder h1 = new Binder();
        final Binder h2 = new Binder();
        final Binder h3 = new Binder();
        final Binder h4 = new Binder();
        final SurfaceControl sc1 = surface("layer-one", 11, h1);
        final SurfaceControl sc2 = surface("layer-two", 12, h2);
        final SurfaceControl sc3 = surface("layer-three", 13, h3);
        final SurfaceControl parent = surface("parent", 14, h4);
        final TxHolder transaction;
        final byte[] minimalDump;
        final AtomicLong committed = new AtomicLong();

        Fixture() {
            SurfaceControl.Transaction t = new SurfaceControl.Transaction();
            t.setPosition(sc1, 10.5f, -3.25f)
                    .setLayer(sc1, 7)
                    .setAlpha(sc1, 0.25f)
                    .setMatrix(sc1, 2f, 0.5f, 0.75f, 3f)
                    .setWindowCrop(sc1, new Rect(1, 2, 301, 202))
                    .setCornerRadius(sc1, 4.5f)
                    .setBackgroundBlurRadius(sc1, 9)
                    .setShadowRadius(sc1, 6.5f)
                    .setLayerStack(sc1, 5)
                    .setOpaque(sc1, true)
                    .setSecure(sc1, true)
                    .setSkipScreenshot(sc1, true)
                    .setColor(sc1, new float[] {0.5f, 0.25f, 0.75f})
                    .setFrameRate(sc1, 59.5f, 1, 1)
                    .setFrameRateCategory(sc1, 3, true)
                    .setFrameRateSelectionPriority(sc1, 12)
                    .setFrameRateSelectionStrategy(sc1, 1)
                    .setDefaultFrameRateCompatibility(sc1, 2)
                    .setMetadata(sc1, 3, 77)
                    .setStretchEffect(sc1, 100f, 50f, 0.5f, 0.25f, 0.75f, 0.125f, 5f, 6f, 7f, 8f)
                    .setEdgeExtensionEffect(sc1, WindowInsets.Side.LEFT | WindowInsets.Side.BOTTOM)
                    .setTrustedOverlay(sc1, 2)
                    .setDropInputMode(sc1, 1)
                    .setDestinationFrame(sc1, new Rect(3, 4, 203, 104))
                    .setBlurRegions(sc1, new float[][] {{20f, 0.5f, 1f, 2f, 30f, 40f, 3.5f, 4.5f, 5.5f, 6.5f}})
                    .reparent(sc1, parent)
                    .setRelativeLayer(sc2, sc1, -3)
                    .unsetBuffer(sc3)
                    .setFrameTimelineVsync(1234L)
                    .setDesiredPresentTimeNanos(987654L)
                    .setEarlyWakeupStart()
                    .setAnimationTransaction();
            t.addTransactionCommittedListener(Runnable::run, () -> committed.incrementAndGet());
            transaction = new TxHolder(t);

            SurfaceControl.Transaction only = new SurfaceControl.Transaction();
            only.setPosition(sc3, 1.5f, 2.5f);
            minimalDump = new TxHolder(only).writeToParcelDump();
        }

        /// name=value pairs (decimal, signed ints sign-extended, floats as raw IEEE bits) for what the Transaction above set.
        String[] expected(boolean unused) {
            List<String> e = new ArrayList<>();
            kv(e, "tx.flags", 0x0a);
            kv(e, "tx.desiredPresentTime", 987654L);
            kv(e, "tx.isAutoTimestamp", 0);
            kv(e, "tx.vsyncId", 1234L);
            kv(e, "tx.composerStates", 3);
            kv(e, "tx.listenerSuffix", 1);
            kv(e, "L0.layerId", 11);
            kv(e, "L0.x", f(10.5f));
            kv(e, "L0.y", f(-3.25f));
            kv(e, "L0.z", 7);
            kv(e, "L0.layerStack", 5);
            kv(e, "L0.flags", 0x2 | 0x80 | 0x40);
            kv(e, "L0.mask", 0x2 | 0x80 | 0x40);
            kv(e, "L0.matrix.dsdx", f(2f));
            kv(e, "L0.matrix.dtdx", f(0.5f));
            kv(e, "L0.matrix.dtdy", f(0.75f));
            kv(e, "L0.matrix.dsdy", f(3f));
            kv(e, "L0.crop.left", f(1f));
            kv(e, "L0.crop.top", f(2f));
            kv(e, "L0.crop.right", f(301f));
            kv(e, "L0.crop.bottom", f(202f));
            kv(e, "L0.color.r", f(0.5f));
            kv(e, "L0.color.g", f(0.25f));
            kv(e, "L0.color.b", f(0.75f));
            kv(e, "L0.color.a", f(0.25f));
            kv(e, "L0.cornerRadius", f(4.5f));
            kv(e, "L0.backgroundBlurRadius", 9);
            kv(e, "L0.shadowRadius", f(6.5f));
            kv(e, "L0.frameRate", f(59.5f));
            kv(e, "L0.frameRateCompatibility", 1);
            kv(e, "L0.changeFrameRateStrategy", 1);
            kv(e, "L0.frameRateCategory", 3);
            kv(e, "L0.frameRateCategorySmoothSwitchOnly", 1);
            kv(e, "L0.frameRateSelectionPriority", 12);
            kv(e, "L0.frameRateSelectionStrategy", 1);
            kv(e, "L0.defaultFrameRateCompatibility", 2);
            kv(e, "L0.metadata.count", 1);
            kv(e, "L0.metadata.key0", 3);
            kv(e, "L0.metadata.len0", 4);
            kv(e, "L0.metadata.byte0", 77);
            kv(e, "L0.stretch.0", f(100f));
            kv(e, "L0.stretch.1", f(50f));
            kv(e, "L0.stretch.2", f(0.5f));
            kv(e, "L0.stretch.3", f(0.25f));
            kv(e, "L0.stretch.4", f(0.75f));
            kv(e, "L0.stretch.5", f(0.125f));
            kv(e, "L0.stretch.6", f(5f));
            kv(e, "L0.stretch.7", f(6f));
            kv(e, "L0.stretch.8", f(7f));
            kv(e, "L0.stretch.9", f(8f));
            kv(e, "L0.edge.left", 1);
            kv(e, "L0.edge.right", 0);
            kv(e, "L0.edge.top", 0);
            kv(e, "L0.edge.bottom", 1);
            kv(e, "L0.trustedOverlay", 2);
            kv(e, "L0.dropInputMode", 1);
            kv(e, "L0.destinationFrame.left", 3);
            kv(e, "L0.destinationFrame.top", 4);
            kv(e, "L0.destinationFrame.right", 203);
            kv(e, "L0.destinationFrame.bottom", 104);
            kv(e, "L0.blur.count", 1);
            kv(e, "L0.blur0.radius", 20);
            kv(e, "L0.blur0.alpha", f(0.5f));
            kv(e, "L0.blur0.left", 1);
            kv(e, "L0.blur0.top", 2);
            kv(e, "L0.blur0.right", 30);
            kv(e, "L0.blur0.bottom", 40);
            kv(e, "L0.blur0.cornerRadiusTL", f(3.5f));
            kv(e, "L0.blur0.cornerRadiusTR", f(4.5f));
            kv(e, "L0.blur0.cornerRadiusBL", f(5.5f));
            kv(e, "L0.blur0.cornerRadiusBR", f(6.5f));
            kv(e, "L0.parent.layerId", 14);
            kv(e, "L1.layerId", 12);
            kv(e, "L1.z", -3);
            kv(e, "L1.relative.layerId", 11);
            kv(e, "L2.layerId", 13);
            // unsetBuffer resets the Transaction's buffer state only: the layer state carries no BufferData
            kv(e, "L2.bufferData.present", 0);
            return e.toArray(new String[0]);
        }

        String[] minimalExpected() {
            List<String> e = new ArrayList<>();
            kv(e, "tx.composerStates", 1);
            kv(e, "tx.isAutoTimestamp", 1);
            kv(e, "tx.flags", 0);
            kv(e, "L0.layerId", 13);
            kv(e, "L0.x", f(1.5f));
            kv(e, "L0.y", f(2.5f));
            kv(e, "L0.bufferData.present", 0);
            return e.toArray(new String[0]);
        }
    }

    static void kv(List<String> out, String name, long value) {
        out.add(name);
        out.add(Long.toString(value));
    }

    // ---- replaying Rust's bytes into a Java Parcel with real binders and fds

    static ByteBuffer le(byte[] b) {
        return ByteBuffer.wrap(b).order(ByteOrder.LITTLE_ENDIAN);
    }

    /// Builds a Java Parcel from a dump: data in chunks, each listed binder object replaced by the next of `binders` (cycling) and each fd object by a real descriptor, so libbinder's own object table is right.
    static Parcel replay(byte[] dump, IBinder[] binders) throws Exception {
        ByteBuffer b = le(dump);
        int len = b.getInt();
        byte[] data = new byte[len];
        b.get(data);
        int n = b.getInt();
        int[] offsets = new int[n];
        for (int i = 0; i < n; i++) offsets[i] = b.getInt();
        Parcel dst = Parcel.obtain();
        int pos = 0;
        int nextBinder = 0;
        for (int i = 0; i < n; i++) {
            int o = offsets[i];
            appendBytes(dst, data, pos, o - pos);
            int type = le(data).getInt(o);
            Parcel obj = Parcel.obtain();
            if (type == BINDER_TYPE_FD) {
                obj.writeFileDescriptor(new FileInputStream("/dev/null").getFD());
                pos = o + 24;
            } else {
                obj.writeStrongBinder(binders[nextBinder++ % binders.length]);
                pos = o + 28; // flat_binder_object + the stability word
            }
            dst.appendFrom(obj, 0, obj.dataSize());
            obj.recycle();
        }
        appendBytes(dst, data, pos, len - pos);
        dst.setDataPosition(0);
        return dst;
    }

    static void appendBytes(Parcel dst, byte[] data, int from, int count) {
        if (count == 0) return;
        Parcel tmp = Parcel.obtain();
        tmp.unmarshall(data, from, count);
        dst.appendFrom(tmp, 0, count);
        tmp.recycle();
    }

    /// libgui turns the binders of BufferData.releaseBufferListener and TrustedPresentationListener into null unless their descriptor is ITransactionCompletedListener's (checked_interface_cast), so the stand-ins carry it.
    static final String LISTENER_DESCRIPTOR = "android.gui.ITransactionComposerListener";

    static IBinder[] fresh(int n) {
        IBinder[] bs = new IBinder[n];
        for (int i = 0; i < n; i++) {
            Binder b = new Binder();
            b.attachInterface(null, LISTENER_DESCRIPTOR);
            bs[i] = b;
        }
        return bs;
    }

    // ---- reader direction

    static void readerCase(String name, int scenario, boolean mustMatch) {
        try {
            byte[] rust = rustTransaction(scenario);
            if (rust == null) {
                report(name, "fail rustTransaction returned null");
                return;
            }
            Parcel in = replay(rust, fresh(64));
            byte[] before = dump(in);
            in.setDataPosition(0);
            SurfaceControl.Transaction t = null;
            String how;
            try {
                t = SurfaceControl.Transaction.CREATOR.createFromParcel(in);
                how = "libgui read it";
            } catch (RuntimeException e) {
                how = "libgui refused it: " + e;
            }
            Parcel out = Parcel.obtain();
            if (t != null) {
                t.writeToParcel(out, 0);
            }
            byte[] after = dump(out);
            String cmp = compareDumps(before, after);
            if (mustMatch) {
                report(name, "ok".equals(cmp) ? "ok " + before.length + " bytes, " + how + ", libgui wrote them back equal" : "fail " + cmp + " (" + how + ")");
            } else {
                cases++;
                if ("ok".equals(cmp)) {
                    failures++;
                    System.out.println("ct fail " + name + " a dropped field still round-tripped through libgui (" + how + ")");
                } else {
                    System.out.println("ct pass " + name + " libgui does not round-trip it: " + cmp + " (" + how + ")");
                }
            }
        } catch (Exception e) {
            report(name, "fail " + e);
        }
    }

    // ---- callbacks

    static void completedCase(Fixture f) {
        String name = "callback:transaction-completed";
        try {
            // the listener binder must be libgui's own object, so it is read from the Parcel libgui wrote, not from a replay
            Parcel p = Parcel.obtain();
            f.transaction.t.writeToParcel(p, 0);
            byte[] tx = dump(p);
            int suffix = suffixOffset(tx);
            if (suffix < 0) {
                report(name, "fail suffixOffset");
                return;
            }
            p.setDataPosition(suffix);
            int listeners = p.readInt();
            IBinder listener = p.readStrongBinder();
            if (listeners < 1 || listener == null) {
                report(name, "fail the transaction registered no listener: " + listeners);
                return;
            }
            byte[] call = rustCompleted(tx, 4242L, 17L);
            if (call == null) {
                report(name, "fail rustCompleted returned null");
                return;
            }
            Parcel data = replay(call, new IBinder[] {f.h1});
            long before = f.committed.get();
            listener.transact(ON_TRANSACTION_COMPLETED, data, null, IBinder.FLAG_ONEWAY);
            long after = f.committed.get();
            report(name, after == before + 1 ? "ok libgui's listener read atl-compositor's ListenerStats and ran the committed callback once" : "fail committed callbacks run: " + (after - before));
        } catch (Exception e) {
            report(name, "fail " + e);
        }
    }

    static void releaseCase(Fixture f) {
        String name = "callback:release-buffer";
        try {
            SurfaceControl.Transaction t = new SurfaceControl.Transaction();
            t.addTransactionCommittedListener(Runnable::run, () -> { });
            Parcel out = Parcel.obtain();
            t.writeToParcel(out, 0);
            byte[] tx = dump(out);
            int suffix = suffixOffset(tx);
            out.setDataPosition(suffix);
            out.readInt();
            IBinder listener = out.readStrongBinder();
            byte[] call = rustRelease(0xAABBCCDD11223344L, 5L, 2);
            Parcel data = replay(call, fresh(1));
            // no release callback was registered for this id, so libgui's reader parses and finds nothing to call; a parse error would throw
            listener.transact(ON_RELEASE_BUFFER, data, null, IBinder.FLAG_ONEWAY);
            report(name, "ok libgui's reader accepted onReleaseBuffer (callbackId, fence, max acquired)");
        } catch (Exception e) {
            report(name, "fail " + e);
        }
    }
}
