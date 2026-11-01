// SPDX-License-Identifier: MIT
// test fixture of `xtask test native-load`: exactly what register_android_util_EventLog reads and binds
package android.util;

import java.util.Collection;

public class EventLog {
    public static final class Event {
        Event(byte[] data) {}
    }

    public static native int writeEvent(int tag, int value);
    public static native int writeEvent(int tag, long value);
    public static native int writeEvent(int tag, float value);
    public static native int writeEvent(int tag, String str);
    public static native int writeEvent(int tag, Object... list);
    public static native void readEvents(int[] tags, Collection<Event> output);
    public static native void readEventsOnWrapping(int[] tags, long timestamp, Collection<Event> output);
}
