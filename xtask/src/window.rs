// SPDX-License-Identifier: MIT
//! `xtask test window`: the window path, one stage at a time. Stage `resume` (M3 plan step 1): the window fixture (xtask/testdata/window: a software activity painting four solid colours)
//! is started the Android way by atld, as `xtask test app` starts its fixture; the services send ResumeActivityItem after LaunchActivityItem (activity.rs), so ActivityThread runs onResume.
//! Pass: the stage's marker is logged, and the app neither logs FATAL EXCEPTION, nor dies, nor waits on a service nothing serves ([`UNSERVED_WAIT`]: a hang shows only as that line) within [`WINDOW_MS`] after it. The app's log lines and the services' counter lines carry the same
//! wall clock (logcat's first field; `at=` on a counter line, atl_services::counters), so every counted call is printed as before or after the marker, with its offset.
//! Always printed: the counters before the marker, the calls after it, the services' crash reports and the app's log after it; on failure also the atld, app and services log tails.
//! `--self-test` checks the verdict on synthetic logs: a crash or a death inside the window fails, one just past it does not, a missing marker and an atld exit fail.
//! Stage `frames` (M3 slice 5) runs the same verdict over its longer window, then two sub-gates on the fixture's frames: (a) the first frame reaches the compositor and its pixels are the
//! quadrants ([`picture_check`]); (b) the Choreographer callbacks come once per display period ([`cadence_check`]).
//! Stage `input` (M3 slice 5 sub-gate c): the resume verdict over the frames window, then the touches the fixture logs are the ones the injected [`POINTER_SCRIPT`] makes ([`input_check`]).
//! Stage `resize` (freeform: the display is the monitor, a window's size is its Task's bounds): UI mode, then the probe configures the window larger and then smaller ([`drive_resize`]); [`resize_checks`] reads the services' bounds, the app's layouts, the composer's readback at each size and the display's metrics. A click and a burst of configures follow ([`burst_check`]). Stages `relaunch` and `declared` run it on the fixture variants, and [`lifecycle_check`] judges the activity's life.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use atl_layout::Layout;
use atl_services::display_model::DisplayModel;

use crate::atld_run::{
    Counter, Event, MS_PER_S, Run, Runtime, events, parse_stamp, run_display, totals,
};
use crate::composer::{fnv1a64, word};
use crate::cpu_pressure::{Sampler, Samples};
use crate::manifest;
use crate::nested_wayland::{GATE_OUTPUT, NestedCompositor, Output};
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};

mod dialog;
mod ime;
mod move_output;
mod surface_view;
mod widgets;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's slot, see `xtask slot`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default: this checkout's slot, see `xtask slot`; else <workspace>/out/java/build).
    #[arg(long)]
    java_out: Option<PathBuf>,
    /// The stage to run. `resume`: onResume is logged, and the app survives the window after it. `frames`: resume, and the first frame's pixels and the frame cadence are right. `input`: resume, and the fixture logs the injected pointer script's touches, in order. `resize`: UI mode; the window is configured larger, then smaller, and the app, the Task's bounds, the readback and the display follow each size.
    #[arg(long, value_enum, default_value = "resume")]
    stage: Stage,
    /// Instead of the stage: the verdict on synthetic logs (what the stage must fail and must pass). Needs no device.
    #[arg(long)]
    self_test: bool,
    /// liblog tag filter of the app (ANDROID_LOG_TAGS).
    #[arg(long, default_value = "*:i")]
    log_tags: String,
    /// Print the whole app log.
    #[arg(long, short)]
    verbose: bool,
    /// Holds the real window open instead of running the stage's gates (UI stages: gtk, widgets, resize). No proxy and no injected configure: the compositor alone resizes the window, so its border and size are the owner's to drag. The app stays up this many seconds after its resume; its log, `AtlWindow: size=` lines included, stays in `<out>/window-hold/root/log/`.
    #[arg(long, value_name = "SECONDS")]
    hold: Option<u64>,
    /// Deliberate break (stage surface-view): the composer orders sibling layers by creation, ignoring z, so the readback check must fail.
    #[arg(long)]
    break_z_order: bool,
    /// Deliberate break (stages widgets, gtk, ime, surface-view): the composer drops the post-draw Transactions the window service forwards, so the post-draw check must fail.
    #[arg(long)]
    break_post_draw: bool,
    /// Deliberate break (any UI stage; made for hwui): the composer latches no queued buffer, as if hwui never queued its frame, so the readback check must fail.
    #[arg(long)]
    break_queue: bool,
    /// Deliberate break (stage hwui): the expected quadrants have red and blue swapped, so the readback check must fail (it compares channel order exactly).
    #[arg(long)]
    break_swap_rb: bool,
    /// Stage ime only: the fake input method leaves its commit_string out, so the commit sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_ime_commit: bool,
    /// Stage ime only: the fake input method sends its delete as after_length (after the cursor, where nothing is), so the delete sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_ime_delete: bool,
    /// Stage ime only: the services give input method clients no death link, so the death sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_ime_death: bool,
    /// Stage ime only: the run ends by SIGKILL, not the control socket's `stop`, so the run sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_atld_stop: bool,
    /// Stage start-finish only: the services refuse a start that would reuse an instance (atld `--no-new-intent`), so the new-intent sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_new_intent: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Stage {
    /// The launch is followed by the resume (ResumeActivityItem): onResume is logged.
    Resume,
    /// Resume, then the sub-gates on the first frame and the frame cadence.
    Frames,
    /// Resume, then the touches the injected pointer script makes are the ones the fixture logs.
    Input,
    /// Resume in UI mode (`ro.atl.ui=gtk`): the app's window is a GTK window of its process. Spike stage: the verdict is the resume one, the mirror's counters are in the app log.
    Gtk,
    /// The widget fixture in UI mode: layout, the text in the readback, a tap on the button, focus, a list drag, presents and cadence (xtask/src/window/widgets.rs).
    Widgets,
    /// The widget fixture in UI mode with a fake desktop input method: the EditText's focus binds the in-app input method, and the IME's preedit, commit and delete reach the EditText (xtask/src/window/ime.rs).
    Ime,
    /// Resume in UI mode, then the probe configures the window larger, then smaller ([`drive_resize`]); the sub-gates follow each size ([`resize_checks`]).
    Resize,
    /// The resize stage on window-sized (a sw600dp resource, no configChanges): the shrink crosses 600 dp, so the activity relaunches; the grow crosses nothing and does not.
    Relaunch,
    /// The resize stage on window-declared (the same resource, the resize bits declared): no relaunch, each change reaches onConfigurationChanged.
    Declared,
    /// The declared stage on window-start: the launcher starts Main (startActivity) and finishes; Main resumes, and each resize reaches it as onConfigurationChanged.
    StartFinish,
    /// Two outputs, the second at scale 1.6: the first goes off, the window moves, and Android's density follows (xtask/src/window/move_output.rs).
    Move,
    /// UI mode at scale 1.6: software SurfaceViews below and above a software window; the readback against the picture worked out from the fixture's spec (xtask/src/window/surface_view.rs).
    SurfaceView,
    /// The gtk stage on window-hwui (hardware accelerated): onDraw gets hwui's RecordingCanvas, and the frame hwui's RenderThread draws through EGL reaches the composer's readback as the quadrants.
    Hwui,
    /// UI mode: a dimming dialog with surfaceInsets and a popup near the bottom over a software window; the readback against the picture worked out from the fixture's spec (xtask/src/window/dialog.rs).
    Dialog,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Resume => "resume",
            Stage::Frames => "frames",
            Stage::Input => "input",
            Stage::Gtk => "gtk",
            Stage::Widgets => "widgets",
            Stage::Ime => "ime",
            Stage::Resize => "resize",
            Stage::Relaunch => "relaunch",
            Stage::Declared => "declared",
            Stage::StartFinish => "start-finish",
            Stage::Move => "move",
            Stage::SurfaceView => "surface-view",
            Stage::Hwui => "hwui",
            Stage::Dialog => "dialog",
        }
    }

    /// How long the app must survive after the marker: the resume window, or the longer watch of the frames and input stages.
    fn window_ms(self) -> u64 {
        match self {
            Stage::Resume => WINDOW_MS,
            Stage::Frames | Stage::Input => FRAMES_WINDOW_MS,
            Stage::Gtk | Stage::SurfaceView | Stage::Hwui | Stage::Dialog => GTK_WINDOW_MS,
            Stage::Widgets | Stage::Ime => WIDGETS_WINDOW_MS,
            Stage::Resize
            | Stage::Relaunch
            | Stage::Declared
            | Stage::StartFinish
            | Stage::Move => RESIZE_WINDOW_MS,
        }
    }
}

/// One sub-gate's result under its name (printed `window: <name>: pass|FAIL`); the name travels with the result, so no list of names can fall out of step with the checks.
pub(crate) type Sub = (&'static str, Result<String, String>);

/// The fixture's dir.
const FIXTURE: &str = "xtask/testdata/window";
/// The same app with a sw600dp resource (their java/ links to FIXTURE's): no configChanges; the resize bits declared.
const SIZED_FIXTURE: &str = "xtask/testdata/window-sized";
const DECLARED_FIXTURE: &str = "xtask/testdata/window-declared";
/// window-declared's Main behind a launcher that starts it and finishes (the stage start-finish).
const START_FIXTURE: &str = "xtask/testdata/window-start";
/// FIXTURE hardware accelerated (the stage hwui).
const HWUI_FIXTURE: &str = "xtask/testdata/window-hwui";
/// The fixture's first onDraw on hwui's RecordingCanvas (Canvas.isHardwareAccelerated).
const HARDWARE_DRAW_LINE: &str = "onDraw reached hardware=true";
/// Scenes the compositor must present into the window before its readback (the fixture's first frame and a repaint).
const GTK_PRESENTED: u32 = 2;
/// The test app's uid: AID_APP_START + 123, any app id (as `xtask test app`).
pub(crate) const APP_UID: u32 = 10_123;
/// How long a cold start may take to reach the marker (as `xtask test app`).
pub(crate) const LIMIT: Duration = Duration::from_secs(180);
/// How long the app must survive after the marker. The resume path (WindowManagerGlobal.addView, the window of the activity) runs right after onResume and fails within
/// milliseconds when it fails, so two seconds leaves the failure time to show.
pub(crate) const WINDOW_MS: u64 = 2_000;
/// The frames and input stages' watch after the marker: the first draw, then FRAME_CALLBACKS vsyncs (0.5 s at 60 Hz), with slack for a slow start; the input stage's touches come after the first draw, inside it.
const FRAMES_WINDOW_MS: u64 = 5_000;
/// The gtk stage's watch after the marker: the first draw, the pointer script, the probe's block and the pong after it.
const GTK_WINDOW_MS: u64 = 10_000;
/// The widgets stage's watch after the marker: the first draw and the readback, then three injections, each waiting up to LINE_WAIT for its fixture line.
const WIDGETS_WINDOW_MS: u64 = 12_000;
/// The resize stage's watch after the marker: the first draw, then each configure (acked within PONG_WAIT) with RESIZE_SETTLE for the relayout, the presents and the readback.
const RESIZE_WINDOW_MS: u64 = 18_000;
const RESIZE_SETTLE: Duration = Duration::from_millis(2_000);
/// The click after the resize (surface coordinates, inside the shrunk window, under BLOCK_MIN_X): a pointer script for [`track_check`].
const CLICK_SCRIPT: &str = "down:200,300 up:200,300";
const CLICK_POINT: (f64, f64) = (200.0, 300.0);
/// Configures sent between the grow and the shrink, BURST_GAP apart. The second is under 600 dp smallest width and the third over it, so in the relaunch stage the third lands while the second's relaunch runs. Their size is the burst's last.
const BURST_STEPS: [(i32, i32, u32); 3] = [
    (1200, 800, 0xA7C0_0103),
    (900, 500, 0xA7C0_0104),
    (1100, 700, 0xA7C0_0105),
];
/// About two frames at 60 Hz: back to back, GTK applies only the last configure, and nothing behind it sees a burst.
const BURST_GAP: Duration = Duration::from_millis(30);
/// The fixture's lines: each instance's onCreate; onConfigurationChanged.
const CREATE_LINE: &str = "AtlWindow: onCreate reached";
const CHANGED_LINE: &str = "AtlWindow: configuration changed";
/// The services' counters (activity.rs task_resized, activityRelaunched).
const SENT_COUNTER: &str = "configuration change sent (ActivityConfigurationChangeItem)";
const RELAUNCH_COUNTER: &str = "configuration change relaunch (ActivityRelaunchItem)";
/// registry_service.rs set_bounds: a size change that reaches the windows only when the relaunch ends.
const RESIZE_HELD: &str = "held for the relaunch";
const RELAUNCHED_COUNTER: &str = "IActivityClientController.activityRelaunched ";
/// Relaunches the relaunch stage may see: the shrink's, then at most two for the burst (its first size, and the last read when that one ends). More means the pending relaunch does not hold back the sizes behind it.
const MAX_RELAUNCHES: u32 = 3;
/// The fixture's blocking touch: an UP past this view x sleeps the UI thread (Main.java BLOCK_MIN_X). The probe's click lands past it, under the window's decorations.
const BLOCK_MIN_X: f32 = 500.0;
const BTN_LEFT: u32 = 0x110;
/// The probe's waits: the window's first configure, a pong, silence while the UI thread sleeps (the fixture sleeps 2 s), the pong after it.
const ACK_WAIT: Duration = Duration::from_secs(60);
const PONG_WAIT: Duration = Duration::from_secs(2);
const SILENT_WAIT: Duration = Duration::from_millis(1000);
const RESUME_WAIT: Duration = Duration::from_secs(4);
/// The app's launcher binary (its command line names it); atld's apps are its descendants.
const APP_LAUNCHER: &str = "atl-app-launcher";
/// How long the apps get to exit after SIGTERM, before atld is killed.
const STOP_WAIT: Duration = Duration::from_secs(3);
/// Between the block's UP and the probe's ping, so the fixture is asleep when the ping is sent.
const BLOCK_SETTLE: Duration = Duration::from_millis(500);
/// The fixture's line once its view has drawn (Main.java Quadrants.onDraw), and the settle after it before the pointer script.
const DRAW_LINE: &str = "onDraw reached";
const INPUT_SETTLE: Duration = Duration::from_millis(500);
/// The fixture's frame callbacks (Main.java FRAME_CALLBACKS): FRAME_CALLBACKS - 1 intervals, each one display period.
const FRAME_CALLBACKS: usize = 31;
/// The layer name of the fixture's window (Main.java WINDOW_TITLE, set by `Window.setTitle`): the compositor's report names the layer by it.
const WINDOW_TITLE: &str = "AtlWindowFixture";
/// The mean interval may differ from the display period by this share: the callbacks run on the app's main thread, which the vsync timer does not wait for.
const CADENCE_TOLERANCE: f64 = 0.10;
/// No single interval may exceed this many periods: a missed vsync shows as two.
const CADENCE_MAX_PERIODS: f64 = 1.5;
/// Host CPU stall share (PSI `some`, cpu_pressure) over a cadence window above which a failed cadence is undecidable rather than FAIL. 10%: over every share a quiet host showed
/// (16 CPUs, load 7-15: 0-7% in 8 passing frames runs), under the 87-97% of a host oversubscribed twice (32 busy loops), where the app's first frames miss vsyncs.
pub(crate) const STARVED_STALL_SHARE: f64 = 0.10;
/// The input stage's pointer script (atld `--headless-pointer`): a down at (200, 300), then a move and an up at (260, 340), one pointer, identity transform.
const POINTER_SCRIPT: &str = "down:200,300 move:260,340 up:260,340";
/// The resize stage's configures, grow then shrink: xdg_toplevel size (logical px) and the serial of the injected configure, far from the compositor's sequential serials (the proxy absorbs the client's acks of these). The content is the configured size less the client-side decorations (a header bar, far under 200 px), so grow is over the opening size (DisplayModel::opening_size, 960x600 on the gates' 1280x800 display) and shrink under it in both dimensions.
const RESIZE_STEPS: [(i32, i32, u32); 2] = [(1600, 1000, 0xA7C0_0101), (800, 500, 0xA7C0_0102)];
/// The fixture's layout line prefix (Main.java Quadrants.onSizeChanged): `AtlWindow: size=WxH screenWidthDp=N window=WxH display=WxH`.
const LAYOUT_LINE: &str = "AtlWindow: size=";
/// DisplayMetrics.DENSITY_DEFAULT: density 1 (dp = px) at this dpi; density = dpi / DENSITY_DEFAULT.
const DENSITY_DEFAULT: i32 = 160;
/// MotionEvent.ACTION_DOWN, ACTION_UP and ACTION_MOVE (AOSP MotionEvent.java): the action of each touch the fixture logs.
const ACTION_DOWN: u32 = 0;
const ACTION_UP: u32 = 1;
const ACTION_MOVE: u32 = 2;
/// The pointer count of each touch: the script has one pointer.
const POINTERS: u32 = 1;
/// How often the logs are read while waiting.
pub(crate) const POLL: Duration = Duration::from_millis(50);
/// Lines shown of a log on failure.
pub(crate) const TAIL: usize = 40;

/// What the stage waits for: the fixture's Log.i line (tag `AtlWindow`, Main.java), as the launcher's logger writes it (`<tag>: <message>`).
fn marker(stage: Stage) -> &'static str {
    match stage {
        Stage::Resume
        | Stage::Frames
        | Stage::Input
        | Stage::Gtk
        | Stage::Hwui
        | Stage::Widgets
        | Stage::Ime
        | Stage::Resize
        | Stage::Relaunch
        | Stage::Declared
        | Stage::StartFinish
        | Stage::Move => "AtlWindow: onResume reached",
        Stage::SurfaceView => surface_view::MARKER,
        Stage::Dialog => dialog::MARKER,
    }
}

/// The wall clock, ms since the epoch: the clock of the logcat stamps and of the services' `at=` stamps.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `secs.mmm` for `ms` since the epoch: the form of a stamp (parse_stamp reads it back).
pub(crate) fn stamp_text(ms: u64) -> String {
    format!("{}.{:03}", ms / MS_PER_S, ms % MS_PER_S)
}

/// A logcat line's time: its first field.
fn line_stamp(line: &str) -> Option<u64> {
    parse_stamp(line.split_whitespace().next()?)
}

/// A logcat line's priority letter: the field before the tag (the first field ending in `:`). `F` = LOG_ALWAYS_FATAL / abort message / ART's abort dump, written just before the process aborts.
fn priority(line: &str) -> Option<&str> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let tag = fields.iter().position(|f| f.ends_with(':'))?;
    fields.get(tag.checked_sub(1)?).copied()
}

/// onResume's time: the first stamped app-log line that contains the marker.
fn marker_stamp(app_log: &str, marker: &str) -> Option<u64> {
    app_log
        .lines()
        .filter(|l| l.contains(marker))
        .find_map(line_stamp)
}

/// The stage's verdict: `marker_ms` is when onResume was logged (None: never); `fault` is the first thing inside the window after it that fails the stage (None: pass).
#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    pub marker_ms: Option<u64>,
    pub fault: Option<String>,
}

/// libbinder's line when a service nothing serves holds a call up (IServiceManager.cpp:727, once a second): the app is hung, not crashed, so only this line tells.
const UNSERVED_WAIT: &str = "Waited one second for";

/// The verdict on an app log. Inside [marker, marker + window_ms]: a FATAL EXCEPTION, the process killed (`Sending signal`, Process.killProcess after a crash), a wait on an unserved service ([`UNSERVED_WAIT`]),
/// and `atld_exited` (the services went away before the window ended). A line outside the window, or without a stamp, does not count.
pub fn verdict(app_log: &str, marker: &str, atld_exited: bool, window_ms: u64) -> Verdict {
    let Some(marker_ms) = marker_stamp(app_log, marker) else {
        return Verdict {
            marker_ms: None,
            fault: None,
        };
    };
    Verdict {
        marker_ms: Some(marker_ms),
        fault: fault_after(app_log, marker_ms, window_ms, marker_ms, atld_exited),
    }
}

/// The first fault of an app log in [from_ms, from_ms + window_ms], its time told relative to `resumed_ms`, onResume's stamp (run-apk reads it from the services log; the app gate watches from
/// the log's start, so a fault may precede it). `atld_exited` is a fault at the window's end.
pub(crate) fn fault_after(
    app_log: &str,
    from_ms: u64,
    window_ms: u64,
    resumed_ms: u64,
    atld_exited: bool,
) -> Option<String> {
    let end = from_ms + window_ms;
    let mut faults: Vec<(u64, String)> = app_log
        .lines()
        .filter_map(|l| Some((line_stamp(l)?, l)))
        .filter(|(t, _)| (from_ms..=end).contains(t))
        .filter_map(|(t, l)| {
            let what = if l.contains("FATAL EXCEPTION") {
                "FATAL EXCEPTION"
            } else if priority(l) == Some("F") {
                "a fatal log line (an abort)"
            } else if l.contains("Sending signal") {
                "the process was killed"
            } else if l.contains(UNSERVED_WAIT) {
                "the app waits on a service nothing serves"
            } else {
                return None;
            };
            let offset = i128::from(t) - i128::from(resumed_ms);
            Some((t, format!("{what} at {offset:+} ms from onResume")))
        })
        .collect();
    if atld_exited {
        faults.push((end, "atld exited inside the window".to_owned()));
    }
    faults.sort_by_key(|(t, _)| *t);
    faults.into_iter().next().map(|(_, f)| f)
}

/// One synthetic app log and what the verdict must say about it.
struct SelfCase {
    name: &'static str,
    log: String,
    atld_exited: bool,
    marked: bool,
    fault: bool,
}

/// A logcat line of the app at `at_ms`, with the fields logcat puts between the stamp and the tag.
fn app_line(at_ms: u64, tag: &str, text: &str) -> String {
    format!("{} 1 2 I {tag}: {text}\n", stamp_text(at_ms))
}

/// The self-test's cases. `T` is onResume's time, so the window is [T, T + WINDOW_MS].
fn self_cases() -> Vec<SelfCase> {
    const T: u64 = 1_000_000;
    let mark = app_line(T, "liblog/AtlWindow", "onResume reached");
    let crash_at = |at: u64| app_line(at, "liblog/AndroidRuntime", "FATAL EXCEPTION: main");
    let case =
        |name: &'static str, log: String, atld_exited: bool, marked: bool, fault: bool| SelfCase {
            name,
            log,
            atld_exited,
            marked,
            fault,
        };
    vec![
        case("marked, nothing after", mark.clone(), false, true, false),
        case(
            "crash inside the window",
            mark.clone() + &crash_at(T + 500),
            false,
            true,
            true,
        ),
        case(
            "native abort inside the window",
            mark.clone()
                + &format!(
                    "{} 2 14 F liblog/OpenGLRenderer: EglManager: eglCreateContext failed: EGL_BAD_ATTRIBUTE\n",
                    stamp_text(T + 300)
                ),
            false,
            true,
            true,
        ),
        case(
            "process killed inside the window",
            mark.clone() + &app_line(T + 100, "liblog/Process", "Sending signal. PID: 2 SIG: 9"),
            false,
            true,
            true,
        ),
        case(
            "unserved-service wait inside the window",
            mark.clone()
                + &app_line(
                    T + 1_000,
                    "libbinder",
                    "Waited one second for SurfaceFlingerAIDL-10123-x (is service started? are binder threads started and available?)",
                ),
            false,
            true,
            true,
        ),
        case(
            "crash just past the window",
            mark.clone() + &crash_at(T + WINDOW_MS + 1),
            false,
            true,
            false,
        ),
        case("crash with no marker", crash_at(T), false, false, false),
        case(
            "atld exits inside the window",
            mark.clone(),
            true,
            true,
            true,
        ),
    ]
}

/// Whether the verdict on a case is the one it expects, and the verdict itself.
fn case_holds(c: &SelfCase) -> (bool, Verdict) {
    let v = verdict(&c.log, marker(Stage::Resume), c.atld_exited, WINDOW_MS);
    let holds = v.marker_ms.is_some() == c.marked && v.fault.is_some() == c.fault;
    (holds, v)
}

/// `--self-test`: every case's verdict is checked; the exit status is nonzero when one differs.
fn self_test() -> ExitCode {
    let mut all = true;
    let mut failures = Vec::new();
    for c in self_cases() {
        let (holds, v) = case_holds(&c);
        all &= holds;
        let said = v.fault.unwrap_or_else(|| {
            if v.marker_ms.is_some() {
                "pass".into()
            } else {
                "no marker".into()
            }
        });
        println!(
            "window: self-test {}: {} ({said})",
            c.name,
            if holds { "pass" } else { "FAIL" }
        );
        if !holds {
            failures.push(crate::verdict::Failure::step(
                c.name,
                format!("the verdict was {said}, which the case does not expect"),
            ));
        }
    }
    if all {
        ExitCode::SUCCESS
    } else {
        let verdict = crate::verdict::Verdict {
            cmd: "window",
            total: None,
            skipped: Vec::new(),
            failures,
        };
        println!("{}", verdict.line());
        ExitCode::FAILURE
    }
}

/// The counters' totals as they stood before `at_ms`.
fn before(all: &[Event], at_ms: u64) -> Vec<Counter> {
    totals(all.iter().filter(|e| e.at_ms < at_ms))
}

fn show(c: &Counter) -> String {
    format!("{} {} x{}", c.kind, c.name, c.count)
}

/// The counters before the marker (their totals), then each counted call from the marker on, with its offset from it.
fn print_calls(all: &[Event], marked: u64) {
    for c in before(all, marked) {
        println!("window: before onResume: {}", show(&c));
    }
    for e in all.iter().filter(|e| e.at_ms >= marked) {
        println!(
            "window: +{} ms after onResume: {} {} (total {})",
            e.at_ms - marked,
            e.counter.kind,
            e.counter.name,
            e.counter.count
        );
    }
}

/// The app's waits on a service nothing serves (libbinder's IServiceManager.cpp:727, "Waited one second for <service>"), from the marker on: the calls those waits hold up.
fn print_waits(app_after: &[&str]) {
    for l in app_after
        .iter()
        .filter(|l| l.contains("Waited one second for"))
    {
        println!("window: app waits, service not served: {l}");
    }
}

/// The services' crash reports (ActivityManager's handleApplicationCrash, activity.rs): logged, not counted.
fn print_crash_reports(services: &str) {
    for l in services.lines().filter(|l| l.contains("crash report from")) {
        println!("window: {l}");
    }
}

/// The app's lines after the first one that contains `marker`.
fn after_marker<'a>(app_log: &'a str, marker: &str) -> Vec<&'a str> {
    app_log
        .lines()
        .skip_while(|l| !l.contains(marker))
        .skip(1)
        .collect()
}

pub(crate) fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The quadrants the fixture paints (Main.java `Quadrants.onDraw`): top-left red, top-right green, bottom-left blue, bottom-right white, as RGBA_8888 bytes
/// (Color.RED and the others). Each boundary is half the view's size with integer division (`getWidth() / 2`). The picture is the buffer's rows without stride padding,
/// the bytes the compositor's sink digests.
fn quadrant_pixels(width: u32, height: u32) -> Vec<u8> {
    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    let (half_width, half_height) = (width / 2, height / 2);
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let colour = match (x < half_width, y < half_height) {
                (true, true) => RED,
                (false, true) => GREEN,
                (true, false) => BLUE,
                (false, false) => WHITE,
            };
            pixels.extend_from_slice(&colour);
        }
    }
    pixels
}

/// Sub-gate (a): the first `composer: quad` line of the fixture's layer `title` (the compositor's report of the first frame it got) is the opening size, and its digest is
/// the digest of the quadrants (FNV-1a over [`quadrant_pixels`]).
fn picture_check(app: &str, display: DisplayModel, title: &str) -> Result<String, String> {
    let (width, height) = opening_size(display);
    let size = format!("{width}x{height}");
    let quad = app
        .lines()
        .filter(|l| l.starts_with("composer: quad "))
        .find(|l| word(l, "name") == Some(title))
        .ok_or_else(|| format!("no `composer: quad` line for layer {title}"))?;
    let buffer = word(quad, "buffer").unwrap_or("none");
    if buffer != size {
        return Err(format!(
            "first frame is {buffer}, the opening size is {size}"
        ));
    }
    let digest = word(quad, "digest").ok_or_else(|| format!("first frame unreadable: {quad}"))?;
    let got = u64::from_str_radix(digest, 16).map_err(|e| format!("digest {digest:?}: {e}"))?;
    let want = fnv1a64(&quadrant_pixels(width, height));
    if got != want {
        return Err(format!(
            "first frame digest {got:016x}, the quadrants {want:016x}"
        ));
    }
    Ok(format!(
        "first frame {size}, digest {want:016x} = the quadrants"
    ))
}

/// Stage `gtk` sub-gate (a): the window mapped at its first frame-clock tick, and the compositor presented at least [`GTK_PRESENTED`] scenes into it before the readback.
fn gtk_window_check(app: &str) -> Result<String, String> {
    if !app
        .lines()
        .any(|l| l.contains("atl-composer: ui: first frame mapped=true"))
    {
        return Err("the window never mapped at its first frame-clock tick".to_owned());
    }
    let line = capture_line(app)?;
    let presented = word(line, "presented")
        .and_then(|v| v.parse::<u32>().ok())
        .ok_or_else(|| format!("no presented count: {line}"))?;
    if presented < GTK_PRESENTED {
        return Err(format!(
            "{presented} scenes presented before the readback, {GTK_PRESENTED} wanted"
        ));
    }
    Ok(format!(
        "mapped, {presented} scenes presented before the readback"
    ))
}

/// Stage `gtk` sub-gate (b): the window's readback at the capture frame has the display's size and the quadrants' digest (the expectation of `picture`).
fn gtk_capture_check(app: &str, display: DisplayModel) -> Result<String, String> {
    capture_check(app, display, false)
}

/// [`gtk_capture_check`]; `swap_rb` = the hwui stage's deliberate break: the expectation's R and B bytes swapped.
fn capture_check(app: &str, display: DisplayModel, swap_rb: bool) -> Result<String, String> {
    let (width, height) = opening_size(display);
    let line = capture_line(app)?;
    let size = word(line, "size").unwrap_or("none");
    let want_size = format!("{width}x{height}");
    if size != want_size {
        return Err(format!(
            "readback is {size}, the opening size is {want_size}"
        ));
    }
    let digest = word(line, "fnv1a64").ok_or_else(|| format!("no digest: {line}"))?;
    let got = u64::from_str_radix(digest, 16).map_err(|e| format!("digest {digest:?}: {e}"))?;
    let mut pixels = quadrant_pixels(width, height);
    if swap_rb {
        pixels
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|p| p.swap(0, 2));
    }
    let want = fnv1a64(&pixels);
    if got != want {
        return Err(format!(
            "readback digest {got:016x}, the quadrants {want:016x}"
        ));
    }
    Ok(format!(
        "readback {want_size} digest {want:016x} = the quadrants"
    ))
}

/// Stops the app processes atld started, before atld is killed: an app whose atld dies under it aborts on its binder calls (DEAD_OBJECT, ~5 s after onResume). SIGTERM, then up to STOP_WAIT for them to exit.
pub(crate) fn stop_apps(atld: u32) {
    let apps = app_pids(atld);
    for pid in &apps {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    let deadline = Instant::now() + STOP_WAIT;
    while apps
        .iter()
        .any(|pid| Path::new(&format!("/proc/{pid}")).exists())
        && Instant::now() < deadline
    {
        std::thread::sleep(POLL);
    }
}

/// Ime stage: atld outlived the killed app (owner decision 2026-11-06: an app's death is an event, not the run's end) and ended on the control socket's `stop` with status 0. In atld's
/// log (main.rs): the first app's `process <pid> ended`, then the relaunch's `control: start ...: ok`, then `control: stop`; `stopped` is atld's exit after the gate stopped it.
fn run_survives_check(
    atld_log: &str,
    stopped: Option<std::process::ExitStatus>,
) -> Result<String, String> {
    let pid = atld_log
        .lines()
        .filter(|l| l.contains("started ") && l.contains("(package "))
        .find_map(|l| l.rsplit("host pid ").next()?.trim().parse::<i32>().ok());
    let at = |needle: &str| atld_log.find(needle);
    let died = pid.and_then(|pid| at(&format!("process {pid} ended")));
    let order = (died, at("control: start "), at("control: stop"));
    let in_order = matches!(order, (Some(d), Some(s), Some(e)) if d < s && s < e)
        && atld_log.contains(": ok am: start ");
    match (in_order, stopped) {
        (true, Some(s)) if s.success() => Ok(format!(
            "atld outlived app pid {} (relaunched it), ended on `stop`: {s}",
            pid.unwrap_or(0)
        )),
        _ => Err(format!(
            "want the first app's end, then an ok `control: start`, then `control: stop` and a successful exit; offsets {order:?}, exit {stopped:?}"
        )),
    }
}

/// The fixture's onNewIntent line (window Main.java) for Start's second start, the services' delivery line (activity_task.rs send) and their refusal count (counters.rs).
const NEW_INTENT_APP: &str = "AtlWindow: onNewIntent reached again=true";
const NEW_INTENT_SERVICES: &str = "services: activity new intent to com.example.atlwindow.Main ";
const START_REFUSED: &str = "refused IActivityTaskManager.startActivity";

/// Start-finish: Start starts singleTop Main twice; the second reuses the top (ActivityStarter.deliverToCurrentTopIfNeeded): one onNewIntent with the second intent, one delivery line,
/// no refused start, one onCreate (no second instance).
fn new_intent_check(app: &str, services: &str) -> Result<String, String> {
    let count = |text: &str, needle: &str| text.lines().filter(|l| l.contains(needle)).count();
    let got = (
        count(app, NEW_INTENT_APP),
        count(services, NEW_INTENT_SERVICES),
        count(services, START_REFUSED),
        count(app, "AtlWindow: onCreate reached"),
    );
    if got == (1, 1, 0, 1) {
        Ok(
            "the second start reused Main: one onNewIntent, one delivery, no refusal, one onCreate"
                .to_owned(),
        )
    } else {
        Err(format!(
            "want 1 `{NEW_INTENT_APP}`, 1 delivery, 0 refused starts, 1 onCreate; got {got:?} (onNewIntent, deliveries, refusals, onCreate)"
        ))
    }
}

/// The app processes atld started (the launcher's processes under it).
pub(crate) fn app_pids(atld: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                c.windows(APP_LAUNCHER.len())
                    .any(|w| w == APP_LAUNCHER.as_bytes())
            })
        })
        .filter(|&pid| descends_from(pid, atld))
        .collect()
}

/// Whether `pid` is a descendant of `ancestor`, by the parent links in /proc.
fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    while let Some(parent) = ppid(pid) {
        if parent == ancestor {
            return true;
        }
        if parent <= 1 {
            return false;
        }
        pid = parent;
    }
    false
}

/// The parent of `pid`: the field after the command name's closing parenthesis in /proc/<pid>/stat.
fn ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The gtk stage's proxy in front of `upstream` (the nested compositor): the app's Wayland connection goes through it, so the probe sees the pongs and injects the pointer.
fn start_proxy(upstream: &Path) -> Result<Proxy> {
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?);
    Proxy::start(
        upstream,
        &runtime.join(format!("atl-window-gate-{}.sock", std::process::id())),
    )
}

/// What the probe saw of the client: its liveness (gtk, widgets), or the acks of the injected configures (resize, where the liveness fields stay at their defaults).
#[derive(Debug, Default)]
struct Liveness {
    /// The pong to the first ping came within [`PONG_WAIT`].
    answered: bool,
    /// No pong to the second ping within [`SILENT_WAIT`], while the fixture's UI thread slept.
    silent: bool,
    /// Ms from the second ping to its pong, after the sleep (None: no pong within [`RESUME_WAIT`]).
    resumed_ms: Option<u128>,
    /// Resize stage: whether the client acked each of [`RESIZE_STEPS`], in order (empty in the other stages).
    configured: Vec<bool>,
}

/// The gtk stage's probe, from the proxy's side: the window's configure is acked; a ping is answered; the pointer script goes in; then a click whose UP sleeps the UI thread (past BLOCK_MIN_X). A ping sent meanwhile gets no pong until the sleep ends.
fn drive(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    // the fixture's first draw: its window takes input from then on (the headless stage injects after the first frame too)
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never drew")?;
    std::thread::sleep(INPUT_SETTLE);
    proxy.inject_ping(1).map_err(|e| e.to_string())?;
    let answered = await_event(&proxy.events, PONG_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 1 }
    })
    .is_some();
    let press = |pressed| PointerAction::Button {
        button: BTN_LEFT,
        pressed,
    };
    // chrome first: surface (100, 20) is in the header bar (below the shadow's 12 px, above the content's origin), so GTK routes the press to the header and Android gets no touch for it
    for action in [
        PointerAction::Enter { x: 100.0, y: 20.0 },
        press(true),
        press(false),
        PointerAction::Enter { x: 200.0, y: 300.0 },
        press(true),
        PointerAction::Motion { x: 260.0, y: 340.0 },
        press(false),
        PointerAction::Motion { x: 600.0, y: 100.0 },
        press(true),
        press(false),
    ] {
        proxy.inject_pointer(action).map_err(|e| e.to_string())?;
    }
    std::thread::sleep(BLOCK_SETTLE);
    let sent = Instant::now();
    proxy.inject_ping(2).map_err(|e| e.to_string())?;
    let silent = await_event(&proxy.events, SILENT_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 2 }
    })
    .is_none();
    let resumed_ms = await_event(&proxy.events, RESUME_WAIT, |e| {
        *e == ProxyEvent::Pong { serial: 2 }
    })
    .map(|at| at.duration_since(sent).as_millis());
    Ok(Liveness {
        answered,
        silent,
        resumed_ms,
        configured: Vec::new(),
    })
}

/// The resize stage's probe, from the proxy's side: once the window is up and has drawn, it is configured to each of [`RESIZE_STEPS`] in turn. Each configure must be acked; then the probe leaves the client [`RESIZE_SETTLE`] to relayout and present.
fn drive_resize(proxy: Proxy, app_log: &Path) -> Result<Liveness, String> {
    await_event(&proxy.events, ACK_WAIT, |e| {
        matches!(e, ProxyEvent::Acked { .. })
    })
    .ok_or("the window never acked a configure")?;
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never drew")?;
    std::thread::sleep(INPUT_SETTLE);
    let (mut configured, [grow, shrink]) = (Vec::new(), RESIZE_STEPS);
    let step = |(width, height, serial): (i32, i32, u32)| -> Result<bool, String> {
        proxy
            .inject_configure(width, height, serial)
            .map_err(|e| e.to_string())?;
        let acked = await_event(&proxy.events, PONG_WAIT, |e| {
            *e == ProxyEvent::Acked { serial }
        })
        .is_some();
        std::thread::sleep(RESIZE_SETTLE);
        Ok(acked)
    };
    configured.push(step(grow)?);
    for (width, height, serial) in BURST_STEPS {
        proxy
            .inject_configure(width, height, serial)
            .map_err(|e| e.to_string())?;
        std::thread::sleep(BURST_GAP);
    }
    std::thread::sleep(RESIZE_SETTLE);
    configured.push(step(shrink)?);
    // the window still takes input at its new size; in the relaunch stage this is the relaunched instance
    let (x, y) = CLICK_POINT;
    for action in [
        PointerAction::Enter { x, y },
        PointerAction::Button {
            button: BTN_LEFT,
            pressed: true,
        },
        PointerAction::Button {
            button: BTN_LEFT,
            pressed: false,
        },
    ] {
        proxy.inject_pointer(action).map_err(|e| e.to_string())?;
    }
    std::thread::sleep(INPUT_SETTLE);
    Ok(Liveness {
        answered: false,
        silent: false,
        resumed_ms: None,
        configured,
    })
}

/// Whether `needle` shows up in the file within `within` (polled: the app log is written by another process).
fn wait_for_line(file: &Path, needle: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if std::fs::read_to_string(file).is_ok_and(|text| text.contains(needle)) {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// The arrival time of the first event `want` accepts within `within`; the events before it are dropped.
fn await_event(
    events: &Receiver<(Instant, ProxyEvent)>,
    within: Duration,
    want: impl Fn(&ProxyEvent) -> bool,
) -> Option<Instant> {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match events.recv_timeout(left) {
            Ok((at, event)) if want(&event) => return Some(at),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
}

/// Stage `gtk` sub-gate (c): the probe's verdict. The ping is answered, no pong comes while the UI thread sleeps, and the pong follows the sleep.
fn pong_check(liveness: &Result<Liveness, String>) -> Result<String, String> {
    let l = liveness.as_ref().map_err(Clone::clone)?;
    if !l.answered {
        return Err(format!("no pong to the first ping within {PONG_WAIT:?}"));
    }
    if !l.silent {
        return Err(format!(
            "a pong came within {SILENT_WAIT:?} of a ping sent while the UI thread slept"
        ));
    }
    let resumed = l
        .resumed_ms
        .ok_or_else(|| format!("no pong within {RESUME_WAIT:?} after the sleep"))?;
    Ok(format!(
        "ping answered; no pong while the UI thread slept; pong {resumed} ms after its ping"
    ))
}

/// Stage `resize` sub-gate (a): the client acked each injected configure.
fn configure_check(liveness: &Result<Liveness, String>) -> Result<String, String> {
    let l = liveness.as_ref().map_err(Clone::clone)?;
    if l.configured.len() != RESIZE_STEPS.len() || l.configured.contains(&false) {
        return Err(format!(
            "acks {:?}, want every configure acked",
            l.configured
        ));
    }
    Ok(format!("acked the configures {RESIZE_STEPS:?}"))
}

/// The Task's bounds in order, as the services logged them (`services: task T of uid U bounds WxH`); the first is the window's size when it opened.
fn task_bounds(services: &str) -> Vec<(i32, i32)> {
    services
        .lines()
        .filter(|l| l.starts_with("services: task "))
        .filter_map(|l| l.split_once(" bounds "))
        .filter_map(|(_, size)| parse_wxh(size.split_whitespace().next()?))
        .collect()
}

/// The windows each bounds change told (`services: resized N windows of task T`), in order.
fn resized_counts(services: &str) -> Vec<u32> {
    services
        .lines()
        .filter(|l| l.starts_with("services: resized "))
        .filter_map(|l| l.split_whitespace().nth(2)?.parse().ok())
        .collect()
}

/// `WxH`, the form of every size the stage reads.
fn parse_wxh(text: &str) -> Option<(i32, i32)> {
    let (w, h) = text.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// The Task's sizes at the two changes of a resize: grow, then shrink (the display's size is the window's first).
type Steps = ((i32, i32), (i32, i32));

/// The resize's steps from the Task's logged bounds. The services log a change only: a Task opens at its opening size ([`opening_size`]), so the log is grow then shrink, and that is the implicit first size.
/// Grow is over the display in both dimensions, shrink under it in both.
fn resize_steps(bounds: &[(i32, i32)], display: (i32, i32)) -> Result<Steps, String> {
    // what follows is the burst's: burst_check reads it
    let &[grow, .., shrink] = bounds else {
        return Err(format!(
            "the Task's bounds {bounds:?}, want grow then shrink (the opening size is the first)"
        ));
    };
    if grow.0 <= display.0 || grow.1 <= display.1 {
        return Err(format!(
            "grow {grow:?} is not over the display {display:?} in both dimensions"
        ));
    }
    if shrink.0 >= display.0 || shrink.1 >= display.1 {
        return Err(format!(
            "shrink {shrink:?} is not under the display {display:?} in both dimensions"
        ));
    }
    Ok((grow, shrink))
}

/// Stage `resize` sub-gate (b): the services' Task went to grow, then to shrink, and each change told at least one window.
fn bounds_check(services: &str, steps: &Result<Steps, String>) -> Result<String, String> {
    let &(grow, shrink) = steps.as_ref().map_err(Clone::clone)?;
    let counts = resized_counts(services);
    let held = services.lines().filter(|l| l.contains(RESIZE_HELD)).count();
    if counts.len() + held != task_bounds(services).len() || counts.contains(&0) {
        return Err(format!(
            "resized {counts:?} windows, {held} held, want one of the two per bounds change, at least one window each"
        ));
    }
    Ok(format!(
        "grow {}x{}, shrink {}x{}; resized {counts:?} windows",
        grow.0, grow.1, shrink.0, shrink.1
    ))
}

/// Stage `resize` sub-gate (c): the app laid out at grow, then at shrink, and its last layout is shrink. Consecutive repeats of one size are one layout.
fn relayout_check(app: &str, steps: &Result<Steps, String>) -> Result<String, String> {
    let &(grow, shrink) = steps.as_ref().map_err(Clone::clone)?;
    let mut layouts: Vec<(i32, i32)> = Vec::new();
    for line in app.lines().filter(|l| l.contains(LAYOUT_LINE)) {
        let size = word(line, "size")
            .and_then(parse_wxh)
            .ok_or_else(|| format!("no size in {line}"))?;
        if layouts.last() != Some(&size) {
            layouts.push(size);
        }
    }
    let mut wanted = [grow, shrink].into_iter();
    let mut pending = wanted.next();
    for size in &layouts {
        if Some(*size) == pending {
            pending = wanted.next();
        }
    }
    if pending.is_some() || layouts.last() != Some(&shrink) {
        return Err(format!(
            "layouts {layouts:?}, want grow {grow:?} then shrink {shrink:?} last"
        ));
    }
    Ok(format!("laid out at {layouts:?}"))
}

/// Stage `resize` sub-gate (d): the composer's readback at grow is the quadrants at grow's size, and its last readback is the quadrants at shrink's (the digest as gtk_capture_check).
fn frame_check(app: &str, steps: &Result<Steps, String>) -> Result<String, String> {
    let &(grow, shrink) = steps.as_ref().map_err(Clone::clone)?;
    let readbacks: Vec<((i32, i32), &str)> = app
        .lines()
        .filter(|l| l.contains("atl-composer: ui: capture "))
        .filter_map(|l| Some((parse_wxh(word(l, "size")?)?, word(l, "fnv1a64")?)))
        .collect();
    let quadrants =
        |(w, h): (i32, i32)| format!("{:016x}", fnv1a64(&quadrant_pixels(w as u32, h as u32)));
    let (grow_digest, shrink_digest) = (quadrants(grow), quadrants(shrink));
    let grow_seen = readbacks
        .iter()
        .any(|&(size, digest)| size == grow && digest == grow_digest.as_str());
    let shrink_last = readbacks
        .last()
        .is_some_and(|&(size, digest)| size == shrink && digest == shrink_digest.as_str());
    if !grow_seen || !shrink_last {
        return Err(format!(
            "readbacks {readbacks:?}, want the quadrants at grow {grow:?} and the last at shrink {shrink:?}"
        ));
    }
    Ok("readbacks at grow and shrink are the quadrants at their sizes".to_owned())
}

/// The layout lines, less each that the next onCreate replaces with no layout between. The relaunch item is queued behind a traversal whose relayout already returns the Task's new size, so the old instance lays out once at that size on its old configuration. That layout is of an instance about to go, so its stale bounds are not a defect. Every other layout is judged.
fn surviving_layouts(app: &str) -> Vec<&str> {
    let lines: Vec<&str> = app
        .lines()
        .filter(|l| l.contains(LAYOUT_LINE) || l.contains(CREATE_LINE))
        .collect();
    lines
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            l.contains(LAYOUT_LINE) && !lines.get(i + 1).is_some_and(|n| n.contains(CREATE_LINE))
        })
        .map(|(_, l)| *l)
        .collect()
}

/// Stage `resize` sub-gate (e): the display is the monitor's at every layout, the window's bounds are the layout's size, screenWidthDp changes with each size and equals the layout's px over its density.
fn display_check(app: &str, display: DisplayModel) -> Result<String, String> {
    let want = format!("{}x{}", display.width, display.height);
    let mut sizes: Vec<&str> = Vec::new();
    let mut last: Option<(&str, &str)> = None;
    for line in surviving_layouts(app) {
        let size = word(line, "size").unwrap_or_default();
        let shown = word(line, "display");
        let window = word(line, "window");
        let dp = word(line, "screenWidthDp").unwrap_or_default();
        if shown != Some(want.as_str()) {
            return Err(format!(
                "the display reported {shown:?} at layout {size}, want {want}"
            ));
        }
        if window != Some(size) {
            return Err(format!("window bounds {window:?} at layout {size}"));
        }
        if let Some((prev_size, prev_dp)) = last
            && prev_size != size
            && prev_dp == dp
        {
            return Err(format!(
                "screenWidthDp stayed {dp} while the layout went {prev_size} -> {size}"
            ));
        }
        // the density math, from the spec: density = dpi / DENSITY_DEFAULT, dp = px / density (exact at the gate's 160 dpi)
        let px: i32 = size
            .split('x')
            .next()
            .and_then(|w| w.parse().ok())
            .unwrap_or(-1);
        let want_dp = px * DENSITY_DEFAULT / display.dpi;
        if dp.parse::<i32>().ok() != Some(want_dp) {
            return Err(format!(
                "screenWidthDp {dp} at layout {size}, want {want_dp} at {} dpi",
                display.dpi
            ));
        }
        if sizes.last() != Some(&size) {
            sizes.push(size);
        }
        last = Some((size, dp));
    }
    if sizes.len() < 3 {
        return Err(format!(
            "layouts {sizes:?}, want the display's size, then grow, then shrink"
        ));
    }
    Ok(format!(
        "display {want} at every layout; window bounds and screenWidthDp follow the layouts {sizes:?}"
    ))
}

/// The resize stages' sub-gates up to `display`; the stage adds click, burst, lifecycle.
fn resize_checks(
    app: &str,
    services: &str,
    display: DisplayModel,
    liveness: &Result<Liveness, String>,
) -> Vec<Sub> {
    let steps = resize_steps(&task_bounds(services), opening_size_i32(display));
    vec![
        ("configure", configure_check(liveness)),
        ("bounds", bounds_check(services, &steps)),
        ("relayout", relayout_check(app, &steps)),
        ("frame", frame_check(app, &steps)),
        ("display", display_check(app, display)),
    ]
}

/// The burst settled at its last size: the Task's bounds between grow and shrink number at most the configures and end there, and the app laid out at it. Content size = configured size less the header, read off the grow step.
fn burst_check(app: &str, services: &str, steps: &Result<Steps, String>) -> Result<String, String> {
    let &(grow, _) = steps.as_ref().map_err(Clone::clone)?;
    let (width, height, _) = BURST_STEPS[BURST_STEPS.len() - 1];
    let want = (width, height - (RESIZE_STEPS[0].1 - grow.1));
    let bounds = task_bounds(services);
    let burst = bounds.get(1..bounds.len().saturating_sub(1)).unwrap_or(&[]);
    let layout = format!("{LAYOUT_LINE}{}x{} ", want.0, want.1);
    if burst.len() > BURST_STEPS.len() || burst.last() != Some(&want) || !app.contains(&layout) {
        return Err(format!(
            "burst bounds {burst:?}, want at most {} ending at {want:?} and a layout there",
            BURST_STEPS.len()
        ));
    }
    Ok(format!("bounds {burst:?}, laid out at {want:?}"))
}

/// The last value of a services counter (`services: unimplemented <interface>.<method> N at=T`); 0 if it never counted.
fn counter(services: &str, method: &str) -> u32 {
    services
        .lines()
        .filter_map(|l| l.split_once(method))
        .filter_map(|(_, rest)| rest.split_whitespace().next()?.parse().ok())
        .max()
        .unwrap_or(0)
}

/// What a fixture's activity must do across the resizes.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Lifecycle {
    /// No thresholds: nothing crosses, the activity lives on and the configuration is sent. Whether it is told is not read: the item's alwaysReportChange tells it, the window's copy (no such flag) does not, and the two race.
    Kept,
    /// The sw600dp bucket, undeclared: the grow crosses nothing, the shrink relaunches.
    Relaunches,
    /// The sw600dp bucket, declared: no relaunch, onConfigurationChanged at each size.
    Told,
}

/// Stage `resize`, `relaunch`, `declared` last sub-gate: the instances the app created, the relaunches the services sent and the changes the app was told are `want`'s.
fn lifecycle_check(
    app: &str,
    services: &str,
    steps: &Result<Steps, String>,
    want: Lifecycle,
) -> Result<String, String> {
    let &(grow, shrink) = steps.as_ref().map_err(Clone::clone)?;
    let creates = app.matches(CREATE_LINE).count() as u32;
    let told: Vec<i32> = app
        .lines()
        .filter(|l| l.contains(CHANGED_LINE))
        .filter_map(|l| word(l, "screenWidthDp")?.parse().ok())
        .collect();
    let sent = counter(services, SENT_COUNTER);
    let relaunches = counter(services, RELAUNCH_COUNTER);
    let relaunched = counter(services, RELAUNCHED_COUNTER);
    let seen = format!(
        "{creates} onCreate, {sent} sent, {relaunches} relaunches ({relaunched} ended), told {told:?}"
    );
    let ok = match want {
        Lifecycle::Kept => creates == 1 && relaunches == 0 && sent >= 2,
        Lifecycle::Told => {
            // dp = px at the gate's density (display_check holds that)
            creates == 1
                && relaunches == 0
                && told.contains(&grow.0)
                && told.last() == Some(&shrink.0)
        }
        Lifecycle::Relaunches => {
            let grown = app
                .find(&format!("{LAYOUT_LINE}{}x{}", grow.0, grow.1))
                .unwrap_or(usize::MAX);
            let second = app.match_indices(CREATE_LINE).nth(1).map(|(at, _)| at);
            (1..=MAX_RELAUNCHES).contains(&relaunches)
                && relaunched == relaunches
                && creates == relaunches + 1
                && second.is_some_and(|at| at > grown)
        }
    };
    if ok {
        Ok(seen)
    } else {
        Err(format!("{seen}, want {want:?}"))
    }
}

/// The composer's readback line of the gtk stage (`atl-composer: ui: capture presented=N size=WxH fnv1a64=H`).
/// Stage `hwui`: the fixture drew on hwui's RecordingCanvas. Else the first onDraw line, or that none came, plus hwui's own fatal line (EglManager's `LOG_ALWAYS_FATAL` text names the failing EGL call and its error).
fn hardware_check(app: &str) -> Result<String, String> {
    if app.contains(HARDWARE_DRAW_LINE) {
        return Ok("onDraw on hwui's RecordingCanvas".to_owned());
    }
    let draw = app
        .lines()
        .find(|l| l.contains(DRAW_LINE))
        .unwrap_or("no onDraw line");
    let fatal = app
        .lines()
        .find(|l| l.contains(" F ") || l.contains("Failed to") || l.contains("EGL"))
        .unwrap_or("no fatal line");
    Err(format!("{draw}; {fatal}"))
}

fn capture_line(app: &str) -> Result<&str, String> {
    app.lines()
        .find(|l| l.contains("atl-composer: ui: capture "))
        .ok_or_else(|| "no readback line: the window never reached its capture frame".to_owned())
}

/// The frame time (ns) of an `AtlWindow: frame N time=T` line: the fixture's Choreographer callback.
fn frame_time(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once("AtlWindow: frame ")?;
    word(rest, "time")?.parse().ok()
}

/// Sub-gate (b): the first FRAME_CALLBACKS callbacks after the marker come once per display period. The mean interval is within CADENCE_TOLERANCE of the period, and no
/// interval is longer than CADENCE_MAX_PERIODS periods. Times are the vsync's (the display clock's grid point each callback was scheduled for, not a wall-clock stamp of the app's own),
/// so the mean does not depend on when the app ran its callbacks; a missed interval is a vsync the client did not ask for in time. A failed judgement while the host was starved
/// ([`STARVED_STALL_SHARE`] over the window) is `undecidable`: still a failure, so `land` needs a rerun that decides.
pub(crate) fn cadence_check(
    after: &[&str],
    refresh_hz: f32,
    pressure: &Samples,
) -> Result<String, String> {
    let times: Vec<u64> = after
        .iter()
        .filter_map(|l| frame_time(l))
        .take(FRAME_CALLBACKS)
        .collect();
    let share = match (times.first(), times.last()) {
        (Some(&a), Some(&b)) => pressure.stall_share(a as i64, b as i64),
        _ => None,
    };
    let stall = share.map_or_else(
        || "cpu stall unknown".to_owned(),
        |s| format!("cpu stall {:.0}%", s * 100.0),
    );
    match cadence_judge(&times, refresh_hz) {
        Ok(msg) => Ok(format!("{msg}; {stall}")),
        Err(e) => match share {
            Some(s) if s > STARVED_STALL_SHARE => Err(format!(
                "undecidable: {e}, but the host was starved ({stall} over the window, threshold {:.0}%); rerun on a quieter host",
                STARVED_STALL_SHARE * 100.0
            )),
            _ => Err(format!("{e}; {stall}")),
        },
    }
}

fn cadence_judge(times: &[u64], refresh_hz: f32) -> Result<String, String> {
    if times.len() < FRAME_CALLBACKS {
        return Err(format!(
            "{} frame callbacks after the marker, want {FRAME_CALLBACKS}",
            times.len()
        ));
    }
    let period = 1e9 / f64::from(refresh_hz);
    let mut longest = 0.0_f64;
    for pair in times.windows(2) {
        let gap = pair[1]
            .checked_sub(pair[0])
            .ok_or_else(|| format!("frame time goes back: {} then {}", pair[0], pair[1]))?;
        longest = longest.max(gap as f64);
    }
    let span = times[times.len() - 1] - times[0];
    let mean = span as f64 / (times.len() - 1) as f64;
    if (mean - period).abs() > CADENCE_TOLERANCE * period {
        return Err(format!(
            "mean interval {:.3} ms, display period {:.3} ms",
            mean / 1e6,
            period / 1e6
        ));
    }
    if longest > CADENCE_MAX_PERIODS * period {
        return Err(format!(
            "longest interval {:.3} ms, over {CADENCE_MAX_PERIODS} periods of {:.3} ms",
            longest / 1e6,
            period / 1e6
        ));
    }
    Ok(format!(
        "{FRAME_CALLBACKS} callbacks, mean interval {:.3} ms, longest {:.3} ms, display period {:.3} ms",
        mean / 1e6,
        longest / 1e6,
        period / 1e6
    ))
}

/// One touch as the fixture logs it (Main.java `Quadrants.onTouchEvent`): the action, the x and y of the pointer, the pointer count.
#[derive(Debug, Clone, PartialEq)]
struct Touch {
    action: u32,
    x: f32,
    y: f32,
    pointers: u32,
}

/// The touches the pointer script makes, in order: `kind:x,y` items, kind `down`, `move` or `up`, one pointer each.
fn script_touches(script: &str) -> Result<Vec<Touch>, String> {
    script
        .split_whitespace()
        .map(|item| {
            let (kind, at) = item
                .split_once(':')
                .ok_or_else(|| format!("script item {item:?}: want kind:x,y"))?;
            let action = match kind {
                "down" => ACTION_DOWN,
                "move" => ACTION_MOVE,
                "up" => ACTION_UP,
                _ => return Err(format!("script item {item:?}: kind is down, move or up")),
            };
            let (x, y) = at
                .split_once(',')
                .ok_or_else(|| format!("script item {item:?}: want kind:x,y"))?;
            let coord = |v: &str| {
                v.parse::<f32>()
                    .map_err(|e| format!("script item {item:?}: {e}"))
            };
            Ok(Touch {
                action,
                x: coord(x)?,
                y: coord(y)?,
                pointers: POINTERS,
            })
        })
        .collect()
}

/// The fixture's `AtlWindow: touch action=A x=X y=Y pointers=N` line as a [`Touch`]; any other line is no touch.
fn touch_line(line: &str) -> Option<Touch> {
    let (_, rest) = line.split_once("AtlWindow: touch ")?;
    Some(Touch {
        action: word(rest, "action")?.parse().ok()?,
        x: word(rest, "x")?.parse().ok()?,
        y: word(rest, "y")?.parse().ok()?,
        pointers: word(rest, "pointers")?.parse().ok()?,
    })
}

/// Sub-gate (c): after the marker the fixture logs one touch per script item, in order, each with the item's action, x and y and one pointer; no other touch.
/// The headless pointer has no decorations: its content origin is (0, 0).
fn input_check(after: &[&str], script: &str) -> Result<String, String> {
    track_check(after, script, (0.0, 0.0))
}

/// The services' count line of an answered WMS setInTouchMode (atl-services window.rs). Unanswered, the same call counts as plain `setInTouchMode`.
const TOUCH_MODE_LINE: &str = "IWindowManager.setInTouchMode (answered)";

/// Sub-gate: the script's first touch takes the window into touch mode (ViewRootImpl.ensureTouchMode -> IWindowManager.setInTouchMode), and the services answer it.
fn touch_mode_check(services: &str) -> Result<String, String> {
    if services.contains(TOUCH_MODE_LINE) {
        Ok("touch-mode: WMS setInTouchMode answered".to_owned())
    } else {
        Err("touch-mode: no answered IWindowManager.setInTouchMode in the services log".to_owned())
    }
}

/// What the parcel codec counts a refused post-draw Transaction under (atl-parcelables SurfaceControl_Transaction.rs).
const TRANSACTION_REFUSED: &str = "refused android.view.SurfaceControl.Transaction";

/// Sub-gate: the app's finishDrawing Transactions reached its composer and were applied (window.rs Session::finishDrawing -> IUiAgent POST_DRAW_TRANSACTION -> Composer::apply_post_draw):
/// none refused by the services' codec, at least one received (every first draw reports one: ViewRootImpl.reportDrawFinished), and every one received applied.
fn post_draw_check(app: &str, services: &str) -> Result<String, String> {
    let refused = services
        .lines()
        .filter(|l| l.contains(TRANSACTION_REFUSED))
        .count();
    let line = crate::composer::last_counters(app).ok_or("no `composer: counters` line")?;
    let n = |k: &str| crate::composer::field(line, k).unwrap_or(0);
    let (received, applied) = (n("post_draw_received"), n("post_draw_applied"));
    if refused == 0 && received > 0 && applied == received {
        Ok(format!(
            "{applied} post-draw transactions applied, 0 refused"
        ))
    } else {
        Err(format!(
            "post-draw: {refused} refused by the codec, {received} received, {applied} applied"
        ))
    }
}

/// The platform's log line of the content origin (atl-platform-ui `GtkWindow::present`), read by [`content_origin`].
const CONTENT_ORIGIN_LINE: &str = "atl-platform-ui: content origin ";

/// The content's origin in the surface, as the platform logs it once the window is mapped: an injected surface point lands in the content at `point - origin`.
fn content_origin(app: &str) -> Result<(f32, f32), String> {
    let line = app
        .lines()
        .find(|l| l.contains(CONTENT_ORIGIN_LINE))
        .ok_or_else(|| "no content origin line: the window never presented".to_owned())?;
    let coord = |key: &str| {
        word(line, key)
            .and_then(|v| v.parse::<f32>().ok())
            .ok_or_else(|| format!("no {key} in {line:?}"))
    };
    Ok((coord("x")?, coord("y")?))
}

/// The pointer script's touches against the fixture's log: the action and pointer count of each, in order, and each position at the script's point minus `origin`
/// (the content's origin in the surface, where the injected points land; (0, 0) for the headless pointer). The offset must round to (0, 0): no measured allowance. Touches past BLOCK_MIN_X are the probe's block, not the script.
fn track_check(after: &[&str], script: &str, origin: (f32, f32)) -> Result<String, String> {
    let want = script_touches(script)?;
    let got: Vec<Touch> = after
        .iter()
        .filter_map(|l| touch_line(l))
        .filter(|t| t.x <= BLOCK_MIN_X)
        .collect();
    if got.len() != want.len() {
        return Err(format!(
            "{} touch lines after the marker, the script has {}",
            got.len(),
            want.len()
        ));
    }
    let (ox, oy) = origin;
    let off = |g: &Touch, w: &Touch| {
        (
            (g.x - (w.x - ox)).round() as i64,
            (g.y - (w.y - oy)).round() as i64,
        )
    };
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        if g.action != w.action || g.pointers != w.pointers || off(g, w) != (0, 0) {
            return Err(format!(
                "touch {i}: got {g:?}, want {w:?} at content origin {origin:?}"
            ));
        }
    }
    Ok(format!(
        "{} touches as `{script}` at content origin {origin:?}",
        want.len()
    ))
}

/// The hold's result: the app logged its resume and atld stayed up for the whole hold. The app log stays where it is; its size lines are echoed.
fn hold_report(app_log: &Path, marker: &str, secs: u64, atld_exited: bool) -> ExitCode {
    let app = std::fs::read_to_string(app_log).unwrap_or_default();
    println!("window: hold log {}", app_log.display());
    for line in app.lines().filter(|l| l.contains(LAYOUT_LINE)) {
        println!("window: {line}");
    }
    if atld_exited {
        println!("window: hold {secs}s FAILED: atld exited before the hold ended");
        ExitCode::FAILURE
    } else if marker_stamp(&app, marker).is_none() {
        println!("window: hold {secs}s FAILED: `{marker}` never logged");
        ExitCode::FAILURE
    } else {
        println!("window: hold {secs}s: `{marker}` logged, atld up for the whole hold");
        ExitCode::SUCCESS
    }
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.self_test {
        return Ok(self_test());
    }
    let _ = Layout::of(root)?;
    // host CPU pressure through the run: the cadence checks tell a starved host from a client that missed frames
    let sampler = Sampler::start();
    // hwui's GraphicBufferMapper aborts the app without the mapper ("gralloc-mapper is missing"); the window path always has it, as the app and composer gates do
    let rt = Runtime::new(
        root,
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        "window-art",
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver()?;
    if args.hold.is_some() && !matches!(args.stage, Stage::Gtk | Stage::Widgets | Stage::Resize) {
        anyhow::bail!("--hold needs a UI stage (gtk, widgets or resize)");
    }
    let work_dir = if args.hold.is_some() {
        "window-hold"
    } else {
        "window"
    };
    let work = rt.out.join(work_dir);
    let mut proxy = None;
    // declared before atld, so dropped after it; a hold keeps the desktop's compositor
    let mut nested = None;
    let rt = match args.stage {
        Stage::Input => rt.with_headless_pointer(POINTER_SCRIPT),
        Stage::Gtk
        | Stage::Hwui
        | Stage::Widgets
        | Stage::Ime
        | Stage::Resize
        | Stage::Relaunch
        | Stage::Declared
        | Stage::StartFinish
        | Stage::Move
        | Stage::SurfaceView
        | Stage::Dialog => {
            // a hold has no proxy: the compositor sees the window alone, so its border and size are the owner's drags
            if args.hold.is_some() {
                rt.with_ui("gtk")
            } else {
                let outputs: &[Output] = match args.stage {
                    Stage::Widgets | Stage::Ime => &[widgets::OUTPUT],
                    Stage::SurfaceView => &[surface_view::OUTPUT],
                    Stage::Move => &move_output::OUTPUTS,
                    _ => &[GATE_OUTPUT],
                };
                let n = NestedCompositor::start_with(&work.join("nested"), outputs)?;
                if matches!(args.stage, Stage::Move) {
                    // the window opens on the first output (move_output.rs)
                    n.control().only(0)?;
                }
                let p = start_proxy(&n.socket)?;
                let rt = rt.with_ui("gtk").with_wayland(&p.listen);
                let rt = if args.break_z_order {
                    rt.with_ui_break("z-order")
                } else if args.break_post_draw {
                    rt.with_ui_break("post-draw")
                } else if args.break_queue {
                    rt.with_ui_break("queue")
                } else {
                    rt
                };
                proxy = Some(p);
                nested = Some(n);
                rt
            }
        }
        Stage::Resume | Stage::Frames => rt,
    };
    // the refresh rate atld answers with and the cadence checks judge against: the gate's own compositor's; headless stages (no window) take the same gate output's; only a hold asks the desktop
    let display = match &nested {
        Some(n) => n.display(),
        None if args.hold.is_some() => run_display(),
        None => GATE_OUTPUT.display(),
    };
    // atld gets the display this gate judges against and never asks the desktop itself: under the window gate's proxy that query reaches the proxy's client
    let rt = rt.with_display(display.to_arg());
    let rt = if args.break_ime_death {
        rt.without_ime_death_link()
    } else {
        rt
    };
    let rt = if args.break_new_intent {
        rt.without_new_intent()
    } else {
        rt
    };
    let outputs = nested.as_ref().map(NestedCompositor::control);
    // the move stage's second output, as Android's display
    let moved = nested
        .as_ref()
        .filter(|_| matches!(args.stage, Stage::Move))
        .map(|n| n.output_display(1));
    let fixture = match args.stage {
        Stage::Widgets | Stage::Ime => widgets::FIXTURE,
        Stage::Relaunch => SIZED_FIXTURE,
        Stage::Declared => DECLARED_FIXTURE,
        Stage::StartFinish => START_FIXTURE,
        Stage::SurfaceView => surface_view::FIXTURE,
        Stage::Hwui => HWUI_FIXTURE,
        Stage::Dialog => dialog::FIXTURE,
        _ => FIXTURE,
    };
    let apk = rt
        .apk
        .build(&root.join(fixture), &work.join("apk"), &[], None)?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the fixture has no launcher activity")?;
    println!(
        "window: built {} ({} versionCode {}, sdk {}..{}, launcher activity {activity})",
        apk.display(),
        m.name,
        m.version_code,
        m.min_sdk,
        m.target_sdk
    );
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![(apk, m.clone())],
        first_uid: APP_UID,
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    let start_ms = now_ms();
    let started = Instant::now();
    // the ime stage's fake desktop input method holds the seat before the app's window exists
    let mut fake_ime = match (args.stage, &nested) {
        (Stage::Ime, Some(n)) => Some(crate::nested_wayland::Ime::bind(n)?),
        _ => None,
    };
    let ime_breaks = ime::Breaks {
        commit: args.break_ime_commit,
        delete: args.break_ime_delete,
    };
    let mut atld_child = rt.start_atld(&run)?;
    let atld_pid = atld_child.id();
    // the probe drives the client through its proxy from here on
    let stage = args.stage;
    let probe = proxy.map(|p| {
        let log = run.log(&m.name);
        let services_log = run.log("services");
        let fake_ime = fake_ime.take();
        let relaunch = (run.root.clone(), format!("{}/{activity}", m.name));
        std::thread::spawn(move || match (stage, outputs, fake_ime) {
            (Stage::Ime, _, Some(ime)) => {
                ime::drive(p, &log, ime, ime_breaks, atld_pid, &services_log, relaunch)
            }
            (Stage::Widgets, _, _) => widgets::drive(p, &log),
            (Stage::SurfaceView, _, _) => surface_view::drive(p, &log),
            (Stage::Dialog, _, _) => dialog::drive(p, &log),
            (Stage::Resize | Stage::Relaunch | Stage::Declared | Stage::StartFinish, _, _) => {
                drive_resize(p, &log)
            }
            (Stage::Move, Some(outputs), _) => move_output::drive(p, &log, outputs),
            _ => drive(p, &log),
        })
    });
    let app_log = run.log(&m.name);
    let services_log = run.log("services");
    let marker = marker(args.stage);
    let window_ms = args.hold.map_or(args.stage.window_ms(), |s| s * 1_000);
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let mut ended = None;
    loop {
        // the window runs from the marker's own stamp, not from when this loop saw the marker
        if marker_stamp(&read(&app_log), marker).is_some_and(|t| now_ms() >= t + window_ms) {
            break;
        }
        if let Some(s) = atld_child.try_wait()? {
            ended = Some(s);
            break;
        }
        // the app's end is the run's end for a gate, as when atld ended with it; not in the ime stage, whose probe kills the app and relaunches it (its death sub-gate)
        if !matches!(args.stage, Stage::Ime) && run.app_ended() {
            ended = Some(run.stop(&mut atld_child)?);
            break;
        }
        if started.elapsed() > LIMIT + Duration::from_secs(args.hold.unwrap_or(0)) {
            break;
        }
        std::thread::sleep(POLL);
    }
    let mut stopped = None;
    if ended.is_none() {
        stop_apps(atld_child.id());
        stopped = Some(if args.break_atld_stop {
            let _ = atld_child.kill();
            atld_child.wait()?
        } else {
            run.stop(&mut atld_child)?
        });
    }
    if let Some(secs) = args.hold {
        return Ok(hold_report(&app_log, marker, secs, ended.is_some()));
    }
    let liveness = match probe {
        Some(handle) => handle
            .join()
            .unwrap_or_else(|_| Err("the probe thread panicked".to_owned())),
        None => Err("no probe: only the gtk, widgets and resize stages run one".to_owned()),
    };
    let pressure = sampler.stop();
    let app = read(&app_log);
    let services = read(&services_log);
    let all = events(&services);
    let v = verdict(&app, marker, ended.is_some(), window_ms);
    if args.verbose {
        println!("---- app log\n{app}");
    }
    // the sub-gates, once the marker is logged (they read the frames or the touches after it)
    let sub: Vec<Sub> = match (args.stage, v.marker_ms) {
        (Stage::Frames, Some(_)) => {
            vec![
                ("picture", picture_check(&app, display, WINDOW_TITLE)),
                (
                    "cadence",
                    cadence_check(&after_marker(&app, marker), display.refresh_hz, &pressure),
                ),
            ]
        }
        (Stage::Input, Some(_)) => vec![
            (
                "touch",
                input_check(&after_marker(&app, marker), POINTER_SCRIPT),
            ),
            ("touch-mode", touch_mode_check(&services)),
        ],
        (Stage::SurfaceView, Some(_)) => surface_view::checks(&app, &liveness),
        (Stage::Dialog, Some(_)) => dialog::checks(&app, &liveness),
        (Stage::Ime, Some(_)) => {
            let mut sub = ime::checks(&app, &services, &liveness);
            sub.push(("run", run_survives_check(&read(&run.atld_log()), stopped)));
            sub
        }
        (Stage::Widgets, Some(_)) => widgets::checks(
            &app,
            &after_marker(&app, marker),
            &liveness,
            display,
            &pressure,
        ),
        (Stage::Move, Some(_)) => match moved {
            Some(moved) => move_output::checks(&app, &services, &liveness, display, moved),
            None => vec![("outputs", Err("no nested compositor".to_owned()))],
        },
        (Stage::Resize | Stage::Relaunch | Stage::Declared | Stage::StartFinish, Some(_)) => {
            let want = match args.stage {
                Stage::Relaunch => Lifecycle::Relaunches,
                Stage::Declared | Stage::StartFinish => Lifecycle::Told,
                _ => Lifecycle::Kept,
            };
            let steps = resize_steps(&task_bounds(&services), opening_size_i32(display));
            let mut sub = resize_checks(&app, &services, display, &liveness);
            sub.push((
                "click",
                content_origin(&app).and_then(|origin| {
                    track_check(&after_marker(&app, marker), CLICK_SCRIPT, origin)
                }),
            ));
            sub.push(("burst", burst_check(&app, &services, &steps)));
            sub.push(("lifecycle", lifecycle_check(&app, &services, &steps, want)));
            if matches!(args.stage, Stage::StartFinish) {
                sub.push(("new-intent", new_intent_check(&app, &services)));
            }
            sub
        }
        (Stage::Hwui, Some(_)) => vec![
            ("mapped", gtk_window_check(&app)),
            ("hardware", hardware_check(&app)),
            ("readback", capture_check(&app, display, args.break_swap_rb)),
        ],
        (Stage::Gtk, Some(_)) => vec![
            ("mapped", gtk_window_check(&app)),
            ("readback", gtk_capture_check(&app, display)),
            (
                "touch",
                content_origin(&app).and_then(|origin| {
                    track_check(&after_marker(&app, marker), POINTER_SCRIPT, origin)
                }),
            ),
            ("pong", pong_check(&liveness)),
        ],
        _ => Vec::new(),
    };
    let mut sub = sub;
    if !sub.is_empty()
        && matches!(
            args.stage,
            Stage::Widgets
                | Stage::Gtk
                | Stage::Hwui
                | Stage::Ime
                | Stage::SurfaceView
                | Stage::Dialog
        )
    {
        sub.push(("post-draw", post_draw_check(&app, &services)));
    }
    let sub_fault = sub.iter().find_map(|(_, r)| r.as_ref().err().cloned());
    for (name, result) in &sub {
        match result {
            Ok(s) => println!("window: {name}: pass {s}"),
            Err(s) => println!("window: {name}: FAIL {s}"),
        }
    }
    if let (Some(marked), None, None) = (v.marker_ms, &v.fault, &sub_fault) {
        println!(
            "window: pass {} (`{marker}` logged {} ms after the start; no FATAL EXCEPTION, death or unserved-service wait in the {window_ms} ms after it)",
            args.stage.name(),
            marked.saturating_sub(start_ms)
        );
        print_calls(&all, marked);
        print_crash_reports(&services);
        let rest = after_marker(&app, marker);
        print_waits(&rest);
        println!(
            "---- app log after the marker (last {TAIL} of {} lines)\n{}",
            rest.len(),
            rest[rest.len().saturating_sub(TAIL)..].join("\n")
        );
        return Ok(ExitCode::SUCCESS);
    }
    let why = match (&v.fault, ended, &sub_fault) {
        (Some(fault), _, _) => fault.clone(),
        (None, Some(s), _) => format!("atld exited {s} before onResume"),
        (None, None, Some(fault)) => fault.clone(),
        (None, None, None) => format!("`{marker}` never logged within {LIMIT:?}"),
    };
    println!("window: FAIL {}: {why}", args.stage.name());
    if let Some(marked) = v.marker_ms {
        print_calls(&all, marked);
        print_waits(&after_marker(&app, marker));
    }
    print_crash_reports(&services);
    println!("---- atld log\n{}", tail(&read(&run.atld_log()), TAIL));
    println!("---- app log (last {TAIL} lines)\n{}", tail(&app, TAIL));
    println!(
        "---- services log (last {TAIL} lines)\n{}",
        tail(&services, TAIL)
    );
    let verdict = crate::verdict::Verdict {
        cmd: "window",
        total: None,
        skipped: Vec::new(),
        failures: vec![crate::verdict::Failure::step(args.stage.name(), why)],
    };
    println!("{}", verdict.line());
    Ok(ExitCode::FAILURE)
}

/// A new Task's window size: 3/4 of each display side, truncated -- AOSP desktop mode's default bounds (Shell DesktopModeUtils.kt:43-56, `persist.wm.debug.desktop_mode_initial_bounds_scale` default 75), stated here from AOSP, not read from the services.
fn opening_size(display: DisplayModel) -> (u32, u32) {
    let (w, h) = opening_size_i32(display);
    (w as u32, h as u32)
}

fn opening_size_i32(display: DisplayModel) -> (i32, i32) {
    (display.width * 3 / 4, display.height * 3 / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every self-test case holds: a crash or a death inside the window fails, one just past it passes, a missing marker and an atld exit are as stated.
    #[test]
    fn self_cases_hold() {
        for c in self_cases() {
            let (holds, v) = case_holds(&c);
            assert!(holds, "{}: {v:?}", c.name);
        }
    }

    /// The marker's time is the first stamped line that contains it; an unstamped line is no stamp.
    #[test]
    fn marker_stamp_reads_the_first_stamped_line() {
        let log = "boot line\n1000.001 x marker\n1000.500 x marker\n";
        assert_eq!(marker_stamp(log, "marker"), Some(1_000_001));
        assert_eq!(marker_stamp("no marker here\n", "marker"), None);
    }

    /// The counters before the marker are totals; the calls from it on keep their offsets.
    #[test]
    fn calls_split_at_the_marker() {
        let ev = |at_ms, count| Event {
            at_ms,
            counter: Counter {
                kind: "unimplemented".into(),
                name: "IA.m".into(),
                count,
            },
        };
        let all = [ev(900, 1), ev(1_000, 2), ev(1_004, 3)];
        assert_eq!(
            before(&all, 1_000),
            [Counter {
                kind: "unimplemented".into(),
                name: "IA.m".into(),
                count: 1,
            }]
        );
        let offsets: Vec<u64> = all
            .iter()
            .filter(|e| e.at_ms >= 1_000)
            .map(|e| e.at_ms - 1_000)
            .collect();
        assert_eq!(offsets, [0, 4]);
    }

    /// The fixture's quadrants on a 4x2 buffer, written out pixel by pixel (red and green above the line y = 1, blue and white below; the vertical boundary at x = 2).
    #[test]
    fn quadrants_are_the_fixture_colours() {
        let (r, g, b, w): ([u8; 4], [u8; 4], [u8; 4], [u8; 4]) = (
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        );
        let want: Vec<u8> = [r, r, g, g, b, b, w, w].concat();
        assert_eq!(quadrant_pixels(4, 2), want);
        // an odd size: the boundary is still width / 2 = 2 (getWidth() / 2), so x = 1 is red and x = 2 green
        let odd = quadrant_pixels(5, 3);
        assert_eq!(odd.len(), 5 * 3 * 4);
        assert_eq!(&odd[4..8], &r, "x = 1, y = 0");
        assert_eq!(&odd[8..12], &g, "x = 2, y = 0");
    }

    /// screenWidthDp is the layout's px over its density (dpi / 160): 320 dpi is density 2, so 1280, 1600 and 960 px are 640, 800 and 480 dp; one dp off fails.
    #[test]
    fn screen_width_is_px_over_density() {
        let shown = |dp: [i32; 3]| {
            ["1280x800", "1600x961", "960x561"]
                .iter()
                .zip(dp)
                .map(|(size, dp)| {
                    format!(
                        "AtlWindow: size={size} screenWidthDp={dp} window={size} display=1280x800"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let at_320 = DisplayModel {
            dpi: 320,
            ..DisplayModel::default()
        };
        assert!(display_check(&shown([640, 800, 480]), at_320).is_ok());
        let err = display_check(&shown([640, 801, 480]), at_320).expect_err("801 dp");
        assert!(err.contains("want 800"), "{err}");
    }

    /// The first frame of the fixture's layer must be the quadrants at the display's size. The expected digest is FNV-1a over the bytes written out here, not over quadrant_pixels.
    /// The fixture's layout line (Main.java onSizeChanged) at `w`x`h`, with screenWidthDp `dp` and the 4x2 test display.
    fn layout(w: i32, h: i32, dp: i32) -> String {
        format!("AtlWindow: size={w}x{h} screenWidthDp={dp} window={w}x{h} display=6x3")
    }

    /// The composer's readback line at `w`x`h`. Its digest is the gate's quadrant pattern, which the gtk stage checks against the live readback.
    fn readback(w: u32, h: u32) -> String {
        let digest = fnv1a64(&quadrant_pixels(w, h));
        format!("atl-composer: ui: capture presented=3 size={w}x{h} fnv1a64={digest:016x}")
    }

    /// The services' log of a Task opened at 4x2 resized to 6x4 (grow) and 3x1 (shrink). A Task opens at its opening size, so only the changes are logged.
    const RESIZE_SERVICES: &str = "services: task 1 of uid 10123 bounds 6x4\nservices: resized 1 windows of task 1\nservices: task 1 of uid 10123 bounds 3x1\nservices: resized 1 windows of task 1";

    /// The named sub-gate is present and failed.
    fn failed(sub: &[Sub], name: &str) -> bool {
        sub.iter().any(|(n, r)| *n == name && r.is_err())
    }

    fn resize_run(app: &[String], services: &str, configured: Vec<bool>) -> Vec<Sub> {
        // opens at 4x2: 3/4 of 6x3, truncated (opening_size)
        let display = DisplayModel {
            width: 6,
            height: 3,
            ..DisplayModel::default()
        };
        let liveness = Ok(Liveness {
            answered: false,
            silent: false,
            resumed_ms: None,
            configured,
        });
        resize_checks(&app.join("\n"), services, display, &liveness)
    }

    /// The consistent run passes every sub-gate; each defect fails the sub-gate that names it. Sizes and pattern come from the spec, not from the code under test.
    #[test]
    fn resize_checks_pass_a_consistent_run_and_fail_each_defect() {
        let good = vec![
            layout(4, 2, 4),
            layout(6, 4, 6),
            layout(3, 1, 3),
            readback(6, 4),
            readback(3, 1),
        ];
        let ok = resize_run(&good, RESIZE_SERVICES, vec![true, true]);
        assert!(ok.iter().all(|(_, r)| r.is_ok()), "{ok:?}");

        let acks = resize_run(&good, RESIZE_SERVICES, vec![true, false]);
        assert!(
            failed(&acks, "configure"),
            "a configure not acked: {acks:?}"
        );

        let services = RESIZE_SERVICES.replace("bounds 6x4", "bounds 4x4");
        let grow = resize_run(&good, &services, vec![true, true]);
        assert!(
            failed(&grow, "bounds"),
            "grow not over the display: {grow:?}"
        );

        let no_grow = [
            layout(4, 2, 4),
            layout(3, 1, 3),
            readback(6, 4),
            readback(3, 1),
        ];
        let relayout = resize_run(&no_grow, RESIZE_SERVICES, vec![true, true]);
        assert!(
            failed(&relayout, "relayout"),
            "no layout at grow: {relayout:?}"
        );

        let stale = [
            layout(4, 2, 4),
            layout(6, 4, 6),
            layout(3, 1, 3),
            readback(6, 4),
            "atl-composer: ui: capture presented=3 size=3x1 fnv1a64=0000000000000000".to_owned(),
        ];
        let frame = resize_run(&stale, RESIZE_SERVICES, vec![true, true]);
        assert!(
            failed(&frame, "frame"),
            "shrink readback has other pixels: {frame:?}"
        );

        let moved = [
            layout(4, 2, 4),
            layout(6, 4, 6),
            layout(3, 1, 3).replace("display=6x3", "display=8x4"),
            readback(6, 4),
            readback(3, 1),
        ];
        let display = resize_run(&moved, RESIZE_SERVICES, vec![true, true]);
        assert!(
            failed(&display, "display"),
            "the display moved with the window: {display:?}"
        );

        let same_dp = [
            layout(4, 2, 4),
            layout(6, 4, 4),
            layout(3, 1, 3),
            readback(6, 4),
            readback(3, 1),
        ];
        let dp = resize_run(&same_dp, RESIZE_SERVICES, vec![true, true]);
        assert!(
            failed(&dp, "display"),
            "screenWidthDp did not change with the grow: {dp:?}"
        );
    }

    /// Each Lifecycle against a log of what it wants, and the one defect each must catch. Sizes are arbitrary: grow (6, 4), shrink (3, 1).
    #[test]
    fn lifecycle_check_tells_the_three_lives_apart() {
        let steps: Result<Steps, String> = Ok(((6, 4), (3, 1)));
        let count = |name: &str, n: u32| format!("services: unimplemented I.{name} {n} at=1\n");
        let create = format!("{CREATE_LINE}\n");
        let grown = format!("{LAYOUT_LINE}6x4 x\n");
        let told = |dp: i32| format!("{CHANGED_LINE} screenWidthDp={dp}\n");
        let check = |app: &str, services: &str, want| lifecycle_check(app, services, &steps, want);
        let sent = count(SENT_COUNTER, 2);
        assert!(check(&create, &sent, Lifecycle::Kept).is_ok());
        assert!(check(&create, "", Lifecycle::Kept).is_err(), "never sent");
        let twice = format!("{create}{grown}{create}");
        assert!(check(&twice, &sent, Lifecycle::Kept).is_err());

        let relaunched = format!(
            "{}services: unimplemented {RELAUNCHED_COUNTER}1 at=1\n",
            count(RELAUNCH_COUNTER, 1)
        );
        assert!(check(&twice, &relaunched, Lifecycle::Relaunches).is_ok());
        let early = format!("{create}{create}{grown}");
        assert!(
            check(&early, &relaunched, Lifecycle::Relaunches).is_err(),
            "relaunched before the grow"
        );
        assert!(check(&create, &relaunched, Lifecycle::Relaunches).is_err());
        let many = count(RELAUNCH_COUNTER, MAX_RELAUNCHES + 1);
        assert!(check(&twice, &many, Lifecycle::Relaunches).is_err());

        let both = format!("{create}{}{}", told(6), told(3));
        assert!(check(&both, "", Lifecycle::Told).is_ok());
        let wrong_last = format!("{create}{}{}", told(3), told(6));
        assert!(check(&wrong_last, "", Lifecycle::Told).is_err());
        assert!(check(&create, "", Lifecycle::Told).is_err());
    }

    /// Only a layout straight before an onCreate drops out; the layout after it and one followed by another layout stay.
    #[test]
    fn a_layout_a_relaunch_replaces_is_not_judged() {
        let app = format!(
            "{CREATE_LINE}\n{LAYOUT_LINE}1\n{LAYOUT_LINE}2\n{CREATE_LINE}\n{LAYOUT_LINE}3\n"
        );
        let kept = surviving_layouts(&app);
        assert_eq!(kept, [format!("{LAYOUT_LINE}1"), format!("{LAYOUT_LINE}3")]);
    }

    /// A 39 px header (grow configured 1000, content 961): the burst's last, 1100x700, is content 1100x661. It must be the last of the middle bounds, with a layout, and the middle no longer than the burst.
    #[test]
    fn burst_check_reads_the_last_size() {
        let steps: Result<Steps, String> = Ok(((1600, 961), (960, 561)));
        let bounds = |sizes: &[&str]| {
            sizes
                .iter()
                .map(|s| format!("services: task 1 of uid 2 bounds {s}\n"))
                .collect::<String>()
        };
        let layout = |s: &str| format!("x {LAYOUT_LINE}{s} y\n");
        let (grow, last, shrink) = ("1600x961", "1100x661", "960x561");
        let services = bounds(&[grow, "900x461", last, shrink]);
        assert!(burst_check(&layout(last), &services, &steps).is_ok());
        assert!(burst_check(&layout("900x461"), &services, &steps).is_err());
        let stuck = bounds(&[grow, last, "900x461", shrink]);
        assert!(burst_check(&layout(last), &stuck, &steps).is_err());
        let dropped = bounds(&[grow, shrink]);
        assert!(burst_check(&layout(last), &dropped, &steps).is_err());
        let uncoalesced = bounds(&[grow, "1x1", "2x2", "3x3", last, shrink]);
        assert!(burst_check(&layout(last), &uncoalesced, &steps).is_err());
    }

    /// An unanswered setInTouchMode counts under its bare name (counters::unimplemented): only the answered line passes.
    #[test]
    fn touch_mode_check_needs_the_answered_line() {
        let line = |m: &str| format!("services: unimplemented IWindowManager.{m} 1 at=1.000");
        assert!(touch_mode_check(&line("setInTouchMode (answered)")).is_ok());
        assert!(touch_mode_check(&line("setInTouchMode")).is_err());
        assert!(touch_mode_check("").is_err());
    }

    #[test]
    fn resize_steps_are_grow_then_shrink_around_the_display() {
        let display = (4, 2);
        assert_eq!(
            resize_steps(&[(6, 4), (3, 1)], display),
            Ok(((6, 4), (3, 1)))
        );
        assert!(resize_steps(&[(4, 4), (3, 1)], display).is_err());
        assert!(resize_steps(&[(6, 4), (4, 1)], display).is_err());
        assert!(resize_steps(&[(6, 4)], display).is_err());
        assert!(resize_steps(&[(4, 2), (6, 4), (3, 1)], display).is_err());
    }

    #[test]
    fn picture_check_reads_the_first_frame_of_the_layer() {
        // opens at 4x2: 3/4 of 6x3, truncated (opening_size)
        let display = DisplayModel {
            width: 6,
            height: 3,
            ..DisplayModel::default()
        };
        let (r, g, b, w): ([u8; 4], [u8; 4], [u8; 4], [u8; 4]) = (
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        );
        let picture: Vec<u8> = [r, r, g, g, b, b, w, w].concat();
        let quad = |name: &str, buffer: &str, digest: String| {
            format!(
                "composer: quad frame=1 layer=2 name={name} buffer={buffer} frame_number=1 dst=0,0,4,2 digest={digest}\n"
            )
        };
        let good = format!("{:016x}", fnv1a64(&picture));
        let t = WINDOW_TITLE;
        assert!(picture_check(&quad(t, "4x2", good.clone()), display, t).is_ok());
        assert!(
            picture_check(&quad("other", "4x2", good.clone()), display, t).is_err(),
            "another layer's frame is not the fixture's"
        );
        assert!(
            picture_check(&quad(t, "4x3", good.clone()), display, t).is_err(),
            "a buffer of the wrong size"
        );
        let red = format!("{:016x}", fnv1a64(&r.repeat(8)));
        assert!(
            picture_check(&quad(t, "4x2", red), display, t).is_err(),
            "all red is not the quadrants"
        );
        let unreadable = format!(
            "composer: quad frame=1 layer=2 name={WINDOW_TITLE} buffer=4x2 unreadable=no dma-buf\n"
        );
        assert!(picture_check(&unreadable, display, t).is_err());
    }

    /// Callbacks one display period apart pass. A mean off by a fifth, too few callbacks, and one missed vsync (one interval of two periods, the mean still within tolerance) fail.
    #[test]
    fn cadence_check_reads_the_callbacks() {
        const STEP: u64 = 16_666_667;
        let lines = |times: &[u64]| -> Vec<String> {
            times
                .iter()
                .enumerate()
                .map(|(i, t)| format!("1000.000 1 2 I AtlWindow: frame {} time={t}", i + 1))
                .collect()
        };
        let check = |times: &[u64], hz: f32| {
            let owned = lines(times);
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            cadence_check(&refs, hz, &Samples::default())
        };
        let steady: Vec<u64> = (0..31).map(|i| 5_000_000_000 + i * STEP).collect();
        let ok = check(&steady, 60.0);
        assert!(ok.is_ok(), "{ok:?}");
        let short: Vec<u64> = (0..30).map(|i| 5_000_000_000 + i * STEP).collect();
        assert!(check(&short, 60.0).is_err());
        let fast: Vec<u64> = (0..31).map(|i| 5_000_000_000 + i * 20_000_000).collect();
        assert!(
            check(&fast, 60.0).is_err(),
            "20 ms intervals on a 60 Hz display"
        );
        let mut missed = Vec::new();
        let mut t = 5_000_000_000u64;
        for i in 0..31 {
            missed.push(t);
            t += if i == 15 { 2 * STEP } else { STEP };
        }
        assert!(check(&missed, 60.0).is_err(), "one interval of two periods");
        assert!(frame_time("1000.000 1 2 I AtlWindow: onDraw reached").is_none());
    }

    /// The starvation rule: a client that skips every other vsync (the stage's deliberate-skip case) FAILs on a quiet host and is undecidable on a starved one; steady callbacks
    /// PASS on both (a pass under load is a pass). Pressure samples bracket the window at the threshold's half and double.
    #[test]
    fn cadence_undecidable_only_when_starved() {
        use crate::cpu_pressure::Sample;
        const STEP: u64 = 16_666_667;
        const T0: u64 = 5_000_000_000;
        let pressure = |share: f64| {
            let wall_ns = 2_000_000_000_i64;
            Samples(vec![
                Sample {
                    at_ns: T0 as i64 - 1,
                    stall_us: 0,
                },
                Sample {
                    at_ns: T0 as i64 - 1 + wall_ns,
                    stall_us: (share * wall_ns as f64 / 1e3) as u64,
                },
            ])
        };
        let run = |times: &[u64], p: &Samples| {
            let owned: Vec<String> = times
                .iter()
                .map(|t| format!("1000.000 1 2 I AtlWindow: frame 1 time={t}"))
                .collect();
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            cadence_check(&refs, 60.0, p)
        };
        let quiet = pressure(STARVED_STALL_SHARE / 2.0);
        let starved = pressure(STARVED_STALL_SHARE * 2.0);
        let steady: Vec<u64> = (0..31).map(|i| T0 + i * STEP).collect();
        let skipping: Vec<u64> = (0..31).map(|i| T0 + i * 2 * STEP).collect();
        assert!(run(&steady, &quiet).is_ok());
        assert!(run(&steady, &starved).is_ok());
        let fail = run(&skipping, &quiet).expect_err("skips on a quiet host");
        assert!(!fail.starts_with("undecidable"), "{fail}");
        let undecided = run(&skipping, &starved).expect_err("skips on a starved host");
        assert!(undecided.starts_with("undecidable"), "{undecided}");
        let unknown = run(&skipping, &Samples::default()).expect_err("no samples");
        assert!(
            !unknown.starts_with("undecidable"),
            "no evidence decides FAIL: {unknown}"
        );
    }

    /// The fixture's touch line reads as its action, x, y and pointer count; any other line is no touch.
    #[test]
    fn touch_line_reads_the_fixture_log() {
        assert_eq!(
            touch_line("1000.000 1 2 I AtlWindow: touch action=2 x=260.0 y=340.0 pointers=1"),
            Some(Touch {
                action: 2,
                x: 260.0,
                y: 340.0,
                pointers: 1,
            })
        );
        assert!(touch_line("1000.000 1 2 I AtlWindow: onDraw reached").is_none());
    }

    /// The script's items written out by hand: a down at (200, 300), then a move and an up at (260, 340), one pointer each. The actions are MotionEvent's ACTION_DOWN 0, ACTION_MOVE 2 and ACTION_UP 1.
    #[test]
    fn script_touches_follow_the_script_text() {
        let want = [
            Touch {
                action: 0,
                x: 200.0,
                y: 300.0,
                pointers: 1,
            },
            Touch {
                action: 2,
                x: 260.0,
                y: 340.0,
                pointers: 1,
            },
            Touch {
                action: 1,
                x: 260.0,
                y: 340.0,
                pointers: 1,
            },
        ];
        assert_eq!(script_touches(POINTER_SCRIPT), Ok(want.to_vec()));
        assert!(script_touches("tap:1,2").is_err(), "no such kind");
        assert!(script_touches("down:1").is_err(), "no y");
        assert!(script_touches("down:a,2").is_err(), "x is not a number");
    }

    /// The touches after the marker must be the script's, in order: one line each, with the script's action, x and y and one pointer. The expected lines are written out here, not made by the code under test.
    #[test]
    fn input_check_matches_the_touch_lines() {
        fn refs(v: &[String]) -> Vec<&str> {
            v.iter().map(String::as_str).collect()
        }
        let line = |action: u32, x: &str, y: &str, pointers: u32| {
            format!(
                "1000.000 1 2 I AtlWindow: touch action={action} x={x} y={y} pointers={pointers}"
            )
        };
        let ok = [
            line(0, "200.0", "300.0", 1),
            line(2, "260.0", "340.0", 1),
            line(1, "260.0", "340.0", 1),
        ];
        assert!(input_check(&refs(&ok), POINTER_SCRIPT).is_ok());
        let wrong_x = [line(0, "201.0", "300.0", 1), ok[1].clone(), ok[2].clone()];
        assert!(
            input_check(&refs(&wrong_x), POINTER_SCRIPT).is_err(),
            "x off by one"
        );
        assert!(
            input_check(&refs(&ok[..2]), POINTER_SCRIPT).is_err(),
            "the up is missing"
        );
        let extra = [ok[0].clone(), ok[0].clone(), ok[1].clone(), ok[2].clone()];
        assert!(
            input_check(&refs(&extra), POINTER_SCRIPT).is_err(),
            "one touch too many"
        );
        let reordered = [ok[0].clone(), ok[2].clone(), ok[1].clone()];
        assert!(
            input_check(&refs(&reordered), POINTER_SCRIPT).is_err(),
            "up before move"
        );
        let two = [line(0, "200.0", "300.0", 2), ok[1].clone(), ok[2].clone()];
        assert!(
            input_check(&refs(&two), POINTER_SCRIPT).is_err(),
            "two pointers"
        );
        let up_first = [line(1, "200.0", "300.0", 1), ok[1].clone(), ok[2].clone()];
        assert!(
            input_check(&refs(&up_first), POINTER_SCRIPT).is_err(),
            "an up where the script has a down"
        );
        assert!(
            input_check(&[], POINTER_SCRIPT).is_err(),
            "no touches at all"
        );
    }

    /// A stamp is printed in the form parse_stamp reads.
    #[test]
    fn stamp_text_round_trips() {
        assert_eq!(stamp_text(1_793_747_156_733), "1793747156.733");
        assert_eq!(
            parse_stamp(&stamp_text(1_793_747_156_733)),
            Some(1_793_747_156_733)
        );
    }
}
