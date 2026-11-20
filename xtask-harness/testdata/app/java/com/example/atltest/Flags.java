// SPDX-License-Identifier: MIT
package com.example.atltest;

import android.app.Activity;
import android.content.ComponentName;
import android.content.Intent;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Intent launch-flag scenarios (app.rs FLAG_LINES), run one after another from Main after the permission callback. Each starts A from Main; every fixture activity runs its scenario's steps at
 * its first resume. The last step (or any start that throws) begins the unwind: from then on each fixture activity finishes in onResume, so the stack empties back to Main. When the last live
 * instance is destroyed the scenario logs "flags-<name>: <instances>", each instance as Id[events] in creation order (A1 = first A). Events: c create (c<X: getCallingActivity is X), r onResume,
 * n onNewIntent, s<request>:<code>:<from> onActivityResult (from = the instance that set the result), x the step's start threw, d onDestroy. Only per-instance order is logged: the order between
 * instances depends on when the services get each ack.
 */
public final class Flags {
    private Flags() {}

    /** One start an instance makes at its first resume: `start` with `flags`, for a result when `request` >= 0, then finish when `finish`; `last` begins the unwind. */
    private static final class Step {
        final String who;
        final Class<?> start;
        final int flags;
        final int request;
        final boolean finish;
        final boolean last;

        Step(String who, Class<?> start, int flags, int request, boolean finish, boolean last) {
            this.who = who;
            this.start = start;
            this.flags = flags;
            this.request = request;
            this.finish = finish;
            this.last = last;
        }
    }

    private static Step last(String who, Class<?> start, int flags) {
        return new Step(who, start, flags, -1, false, true);
    }

    private static final class Scenario {
        final String name;
        final Step[] steps;

        Scenario(String name, Step... steps) {
            this.name = name;
            this.steps = steps;
        }
    }

    /** Intent.FLAG_ACTIVITY_NEW_DOCUMENT: a new task, which the services refuse (the control row). */
    private static final int NEW_DOCUMENT = Intent.FLAG_ACTIVITY_NEW_DOCUMENT;

    private static final Scenario[] SCENARIOS = {
        new Scenario("clear-top", new Step("A1", B.class, 0, -1, false, false), last("B1", A.class, Intent.FLAG_ACTIVITY_CLEAR_TOP)),
        new Scenario("clear-top-single-top", new Step("A1", B.class, 0, -1, false, false),
                last("B1", A.class, Intent.FLAG_ACTIVITY_CLEAR_TOP | Intent.FLAG_ACTIVITY_SINGLE_TOP)),
        new Scenario("single-top", last("A1", A.class, Intent.FLAG_ACTIVITY_SINGLE_TOP)),
        new Scenario("no-history", new Step("A1", B.class, Intent.FLAG_ACTIVITY_NO_HISTORY, -1, false, false), last("B1", C.class, 0)),
        new Scenario("reorder-to-front", new Step("A1", B.class, 0, -1, false, false), last("B1", A.class, Intent.FLAG_ACTIVITY_REORDER_TO_FRONT)),
        new Scenario("forward-result", new Step("A1", B.class, 0, 3, false, false),
                new Step("B1", C.class, Intent.FLAG_ACTIVITY_FORWARD_RESULT, -1, true, true)),
        new Scenario("refused-new-document", last("A1", B.class, NEW_DOCUMENT)),
    };

    private static Activity main;
    private static int index = -1;
    private static boolean unwinding;
    private static int live;
    private static final Map<String, Integer> COUNTS = new LinkedHashMap<>();
    private static final Map<String, StringBuilder> EVENTS = new LinkedHashMap<>();

    /** Runs the scenarios with `from` (Main) as every scenario's first source. */
    static void start(Activity from) {
        main = from;
        next();
    }

    private static void next() {
        index++;
        if (index >= SCENARIOS.length) {
            return;
        }
        unwinding = false;
        COUNTS.clear();
        EVENTS.clear();
        main.startActivity(new Intent(main, A.class));
    }

    private static void record(String id, String event) {
        StringBuilder events = EVENTS.get(id);
        events.append(events.length() == 0 ? "" : ",").append(event);
    }

    /** The class's short name: Flags$A -> A. */
    private static String shortName(String className) {
        return className.substring(className.lastIndexOf('$') + 1);
    }

    /** A fixture activity: records its events and runs its scenario's steps. */
    public static class Base extends Activity {
        private String id;
        private boolean resumedOnce;

        @Override
        protected void onCreate(Bundle savedInstanceState) {
            super.onCreate(savedInstanceState);
            String letter = shortName(getClass().getName());
            // no lambdas or method refs: the fixture compiles against a bootclasspath without LambdaMetafactory
            Integer prev = COUNTS.get(letter);
            int n = prev == null ? 1 : prev + 1;
            COUNTS.put(letter, n);
            id = letter + n;
            EVENTS.put(id, new StringBuilder());
            live++;
            ComponentName calling = getCallingActivity();
            record(id, calling == null ? "c" : "c<" + shortName(calling.getClassName()));
            setResult(RESULT_OK, new Intent().putExtra("from", id));
        }

        @Override
        protected void onResume() {
            super.onResume();
            record(id, "r");
            if (unwinding) {
                finish();
                return;
            }
            if (resumedOnce) {
                return;
            }
            resumedOnce = true;
            for (Step step : SCENARIOS[index].steps) {
                if (step.who.equals(id)) {
                    run(step);
                }
            }
        }

        private void run(Step step) {
            Intent intent = new Intent(this, step.start).addFlags(step.flags);
            if (step.last) {
                unwinding = true;
            }
            try {
                if (step.request >= 0) {
                    startActivityForResult(intent, step.request);
                } else {
                    startActivity(intent);
                }
            } catch (RuntimeException e) {
                record(id, "x");
                unwinding = true;
                finish();
                return;
            }
            if (step.finish) {
                finish();
            }
        }

        @Override
        protected void onNewIntent(Intent intent) {
            super.onNewIntent(intent);
            record(id, "n");
        }

        @Override
        protected void onActivityResult(int requestCode, int resultCode, Intent data) {
            super.onActivityResult(requestCode, resultCode, data);
            record(id, "s" + requestCode + ":" + resultCode + ":" + (data == null ? "none" : data.getStringExtra("from")));
        }

        @Override
        protected void onDestroy() {
            super.onDestroy();
            record(id, "d");
            live--;
            if (live == 0) {
                StringBuilder line = new StringBuilder();
                for (Map.Entry<String, StringBuilder> e : EVENTS.entrySet()) {
                    line.append(line.length() == 0 ? "" : " ").append(e.getKey()).append('[').append(e.getValue()).append(']');
                }
                Log.i("AtlTest", "flags-" + SCENARIOS[index].name + ": " + line);
                new Handler(Looper.getMainLooper()).post(new Runnable() {
                    @Override
                    public void run() {
                        next();
                    }
                });
            }
        }
    }

    public static class A extends Base {}

    public static class B extends Base {}

    public static class C extends Base {}
}
