// SPDX-License-Identifier: MIT
//! `xtask test window`: the window path, one stage at a time. Stage `resume` (M3 plan step 1): the window fixture (xtask-harness/testdata/window: a software activity painting four solid colours)
//! is started the Android way by atld, as `xtask test app` starts its fixture; the services send ResumeActivityItem after LaunchActivityItem (activity.rs), so ActivityThread runs onResume.
//! Pass: the stage's marker is logged, and the app neither logs FATAL EXCEPTION, nor dies, nor waits on a service nothing serves ([`UNSERVED_WAIT`]: a hang shows only as that line) within [`WINDOW_MS`] after it. The app's log lines and the services' counter lines carry the same
//! wall clock (logcat's first field; `at=` on a counter line, atl-diag), so every counted call is printed as before or after the marker, with its offset.
//! Always printed: the counters before the marker, the calls after it, the services' crash reports and the app's log after it; on failure also the atld, app and services log tails.
//! `--self-test` checks the verdict on synthetic logs: a crash or a death inside the window fails, one just past it does not, a missing marker and an atld exit fail.
//! Stage `frames` (M3 slice 5) runs the same verdict over its longer window, then two sub-gates on the fixture's frames: (a) the first frame reaches the compositor and its pixels are the
//! quadrants ([`picture_check`]); (b) the Choreographer callbacks come once per display period ([`cadence_check`]).
//! Stage `input` (M3 slice 5 sub-gate c): the resume verdict over the frames window, then the touches the fixture logs are the ones the injected [`POINTER_SCRIPT`] makes ([`input_check`]).
//! Stage `resize` (freeform: the display is the monitor, a window's size is its Task's bounds): UI mode, then the probe configures the window larger and then smaller ([`drive_resize`]); [`resize_checks`] reads the services' bounds, the app's layouts, the window's picture at each size ([`capture::Probe`]) and the display's metrics. A click and a burst of configures follow ([`burst_check`]). Stages `relaunch` and `declared` run it on the fixture variants, and [`lifecycle_check`] judges the activity's life.
//! Stage `gles2` (GS2.5): the GLES2 fixture with its `.so` a guest library through the native bridge; the app's readback and the harness's picture at its probes are the expectation ([`gles2::checks`]).
//! Stage `suspended` (window lifecycle S0): the probe sends the maximized control and the suspended state through the proxy; the platform layer must log each as the toplevel's state ([`suspended::checks`]).
//! Stage `portrait` (window bounds, slice 3): the portrait fixture in UI mode; its readback and every layout are [`portrait_expected`], AOSP's size for a portrait app on the gate's landscape output.
//! Stage `tiled`: UI mode with the proxy's tile ([`crate::wayland_proxy::Tile`]): every compositor configure of the window, the first included, carries [`TILE`], under the requested size, as a tiling compositor's layout; the Task's bounds, every layout, screenWidthDp and the picture must be the tile's content ([`tiled_checks`]).
//! Stage `app-id`: UI mode; the last `xdg_toplevel.set_app_id` the window sends before its first commit (the initial one, no buffer) is `atl.<package>` ([`app_id_check`]), read by the proxy ([`crate::wayland_proxy::Event::AppId`], [`crate::wayland_proxy::Event::FirstCommit`]).
//! Stage `title`: UI mode; the window's `xdg_toplevel.set_title`s ([`crate::wayland_proxy::Event::Title`]) hold the fixture's label (recents' fallback), then end at the TaskDescription label the fixture sets after its first draw ([`title_check`]).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use atl_layout::Layout;
use atl_services_types::display_model::DisplayModel;

use crate::atld_run::{
    Counter, Event, MS_PER_S, Package, Run, Runtime, events, parse_stamp, run_display, show, totals,
};
use crate::composer::{fnv1a64, word};
use crate::gles2_fixture;
use crate::manifest;
use crate::nested_wayland::{GATE_OUTPUT, NestedCompositor, Output};
use crate::wayland_proxy::{Event as ProxyEvent, PointerAction, Proxy};
use atl_host_load::{Sampler, Samples, judge_window};

mod aux_process;
mod capture;
mod dialog;
mod gles1;
mod gles2;
mod ime;
mod move_output;
mod slow_display;
mod splash;
mod starvation;
mod surface_view;
mod suspended;
mod ui_block;
mod webview;
mod webview_overlay;
mod widgets;

#[derive(clap::Args)]
pub struct Args {
    /// The host arch (an arches.toml row with `[host]`): the default --out.
    #[arg(long)]
    host: String,
    /// Native build directory holding ART, servicemanager and the services (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/native/<host>).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Java pipeline build directory: boot jars, aapt2, d8 (default: this checkout's build dir, see `cargo xtask out show`; else <workspace>/out/java/build).
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
    /// Holds the real window open instead of running the stage's gates (UI stages: gtk, widgets, resize). No proxy and no injected configure: the compositor alone resizes the window, so its border and size are the maintainer's to drag. The app stays up this many seconds after its resume; its log, `AtlWindow: size=` lines included, stays in `<out>/window-<stage>-hold/root/log/`.
    #[arg(long, value_name = "SECONDS")]
    hold: Option<u64>,
    /// The headless compositor's output, WxH (default: GATE_OUTPUT, 1920x1200). Stages gtk, portrait and app-id only: their expectations follow the display.
    #[arg(long, value_name = "WxH", value_parser = parse_output_size)]
    output: Option<(i32, i32)>,
    /// Deliberate break (stages surface-view and gles2): the composer orders sibling layers by creation, ignoring z, so the readback check must fail.
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
    /// Deliberate break (stage dialog): the composer ignores window frames, so the dialog's touches arrive in Task coordinates and the touch check must fail.
    #[arg(long)]
    break_input_frames: bool,
    /// Deliberate break (stages widgets, dialog and the other nested-compositor UI stages): the composer's input loop is never woken by a send (atl-composer feature `break-input-no-wake`), so its messages wait and the input checks must fail.
    #[arg(long)]
    break_input_no_wake: bool,
    /// Deliberate break (any UI stage that replaces buffers): the composer ignores the window's hold set and releases a shown buffer at the latch that replaces it, before the window's paint, so the release check must fail.
    #[arg(long)]
    break_ignore_hold: bool,
    /// Deliberate break (stage slow-display): the composer latches every tick whether the window took the last scene or not (atl-composer feature `break-no-backpressure`), so the held and replaced checks must fail.
    #[arg(long)]
    break_no_backpressure: bool,
    /// Deliberate break (stages widgets, ui-block): the composer reads every presentation sample as not vsync (atl-composer feature `break-ignore-vsync-flag`), so on a hardware-vsync host the
    /// vsync-tier check, judged by the proxy's count, must fail. A host that flags no vsync (headless labwc) stays green: the red control needs a hardware-vsync desktop.
    #[arg(long)]
    break_ignore_vsync_flag: bool,
    /// Stage ime only: the fake input method leaves its commit_string out, so the commit sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_ime_commit: bool,
    /// Stage ime only: the composer's outbox runs each post inline on the poster's thread (atl-composer feature `break-outbox`), so the Task reports and the input method's sends are made on the
    /// display thread and the outbox sub-gate must fail (an input method send there reaches the in-process editor with no JNIEnv: the app aborts, which fails the stage too).
    #[arg(long)]
    break_outbox: bool,
    /// Stage aux-process only: a queued transaction does not keep the layers it names (atl-composer feature `break-handle-hold`), so the fixture's transaction on a layer it released at once meets
    /// an unknown layer and the released-layer sub-gate must fail.
    #[arg(long)]
    break_handle_hold: bool,
    /// Stage ime only: the fake input method sends its delete as after_length (after the cursor, where nothing is), so the delete sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_ime_delete: bool,
    /// Stage ime only: the services give input method clients no death link, so the death sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_ime_death: bool,
    /// Stage ime only: the input method's copy of the editor's text keeps its text through each edit (atl-ime feature `break-copy`), so the editor's answers diverge from it and the copy sub-gate must fail.
    #[arg(long)]
    break_ime_copy: bool,
    /// Stage ime only: the run ends by SIGKILL, not the control socket's `stop`, so the run sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_atld_stop: bool,
    /// Stage start-finish only: the services refuse a start that would reuse an instance (atld `--no-new-intent`), so the new-intent sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_new_intent: bool,
    /// Stage widgets only: the back press is BTN_LEFT, so no back key comes and the mouse-buttons sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_mouse_back: bool,
    /// Stage mouse-touch only: the desktop adaptation is off (no `with_mouse_touch()`, so atld `--mouse-touch false`), the drag stays the mouse's (source 8194) and the mouse-touch sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_mouse_touch: bool,
    /// Stage webview-overlay: what the page tap is (mouse: SOURCE_MOUSE; mouse-finger: a mouse click with atld `--mouse-touch true`; touch: a real `wl_touch` contact). A mouse press focuses the
    /// WebView, a touch press does not (tap-focus, esc-keys). Run the stage each way.
    #[arg(long, value_enum, default_value = "mouse")]
    tap_origin: webview_overlay::TapOrigin,
    /// Stage webview-overlay: the checks expect the other origin's focus rule, so tap-focus and esc-keys must fail (the gate's deliberate break).
    #[arg(long)]
    break_tap_origin: bool,
    /// Stages widgets and webview: atld runs with the Esc -> Back adaptation on (`with_esc_back()`, atld `--esc-back true`), and the esc sub-gates expect the keyboard map's plain Esc fallback BACK (widgets: esc-fallback, esc-back; webview: esc-back); without it, AOSP's none. Run the stages both ways.
    #[arg(long)]
    esc_back: bool,
    /// Stages widgets and webview, with `--esc-back` only: atld runs with the adaptation off (atld `--esc-back false`) while the checks expect it on, so no BACK follows the unhandled Esc and the esc-back sub-gate must fail (the gate's deliberate break).
    #[arg(long, requires = "esc_back")]
    break_esc_back: bool,
    /// Stage capture: atld runs with `--exit-with-last-app true` and the atld-end sub-gate expects the run to end with status 0 after the close's kill, every process atld ran gone; without it,
    /// atld `false` and the sub-gate expects atld still running after the kill. Run the stage both ways. The timeout sub-gate (d) needs the run to go on: it runs only without this flag.
    #[arg(long)]
    exit_with_last_app: bool,
    /// Stage capture, with `--exit-with-last-app` only: atld runs with the adaptation off while the check expects the end, so the atld-end sub-gate must fail (the gate's deliberate break).
    #[arg(long, requires = "exit_with_last_app")]
    break_exit_with_last_app: bool,
    /// Stage dialog only: the services send no requestFinish on an unhandled Back (atld `--no-back-finish`), so the back sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_back_finish: bool,
    /// Stage dialog only: an unhandled root Back closes the root instead of minimizing (atld `--root-back close`), so the minimize sub-gate must fail (the gate's deliberate break).
    #[arg(long)]
    break_root_minimize: bool,
    /// Stage orientation only: orientation requests follow AOSP's rule (atld `--orientation-reshape unresizeable`), so the resizeable fixture's window stays landscape and the reshaped and told
    /// sub-gates must fail (the gate's deliberate break).
    #[arg(long)]
    break_orientation_reshape: bool,
    /// Stage suspended only: the probe never sends the suspended configure, so the states check must fail (the gate's deliberate break).
    #[arg(long)]
    break_suspended: bool,
    /// Stage capture only: the refusal sub-gate expects the fixture served, so it must fail on the registry's SECURITY (the gate's deliberate break, harness-side).
    #[arg(long)]
    break_capture_caller: bool,
    /// Stage gles2 only: the fixture's worker thread parks forever instead of exiting, so the bridge never detaches it and the balance sub-gate must fail (the gate's deliberate break, fixture-side).
    #[arg(long)]
    break_no_detach: bool,
    /// Stage hwui only: the app processes preload the platform module (`LD_PRELOAD`), putting GTK's closure in global scope as an `RTLD_GLOBAL` load would, so the bindings sub-gate must fail on undeclared app/AOSP -> host bindings (the gate's deliberate break).
    #[arg(long)]
    break_global_module: bool,
    /// Stage hwui only: the app processes preload the runtime cdylib (`LD_PRELOAD` of xtask launcher_closure `RUNTIME`), putting the AOSP group in global scope as loading it `RTLD_GLOBAL` would, so host objects (GTK, the Khronos loader) bind AOSP libraries again
    /// and the bindings sub-gate must fail on new reverse sonames (the scope split's deliberate break).
    #[arg(long)]
    break_global_runtime: bool,
    /// Stage hwui only: this `[[reverse]]` row of launcher-closure.toml is dropped before judging, so its soname is a new reverse binding and the bindings sub-gate must fail naming it (the gate's deliberate break).
    #[arg(long, value_name = "SONAME")]
    break_forget_reverse: Option<String>,
    /// Stage webview only: the run's library dir lacks the web engine module (a host without WebKit), the fixture's WebView cannot be made, so the readback sub-gate must fail (the gate's deliberate break, environment-side).
    #[arg(long)]
    break_webview_engine: bool,
    /// Stage webview only: atld finds no pulse server (`PULSE_SERVER` names a missing socket, Runtime::without_pulse_server), so `share-sound` grants the web process nothing and the sound
    /// sub-gate must fail (the monitor stays silent, atld counts `WebProcessSpawn.sound-no-server`; the gate's deliberate break, environment-side).
    #[arg(long)]
    break_webview_sound: bool,
    /// Stage splash only: `ro.atl.splash` is left unset, so the launcher shows no starting window and the order sub-gate must fail (the gate's deliberate break, environment-side).
    #[arg(long)]
    break_splash: bool,
    /// A variable in every app process's environment (repeat), atld `--app-env`: test-environment setup, e.g. `GSK_RENDERER=ngl` so GTK renders with GL and Mesa's driver (with libLLVM) sits in the global scope before ART loads. The product reads none of these.
    #[arg(long = "app-env", value_name = "KEY=VALUE")]
    app_env: Vec<String>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Stage {
    /// The launch is followed by the resume (ResumeActivityItem): onResume is logged.
    Resume,
    /// Resume, then the sub-gates on the first frame and the frame cadence.
    Frames,
    /// Resume, then the touches the injected pointer script makes are the ones the fixture logs.
    Input,
    /// Resume in UI mode (`ro.atl.ui=gtk`): the app's window is a GTK window of its process, on the platform's display thread. Sub-gates: mapped, readback, touch, pong, ping-hold, startup-order (the display thread attached before ART's first record).
    Gtk,
    /// The widget fixture in UI mode: layout, the text in the readback, a tap on the button, focus, a list drag, presents and cadence (xtask-harness/src/window/widgets.rs).
    Widgets,
    /// The widget fixture with the desktop's mouse-as-one-finger adaptation on (`with_mouse_touch()`, atld `--mouse-touch true`): one mouse drag on the list reaches the app as a touchscreen drag (source 4098), not the mouse's 8194 (xtask-harness/src/window/widgets.rs `drive_mouse_touch`).
    MouseTouch,
    /// The widget fixture in UI mode with a fake desktop input method: the EditText's focus binds the in-app input method, and the IME's preedit, commit and delete reach the EditText (xtask-harness/src/window/ime.rs).
    Ime,
    /// Resume in UI mode, then the probe configures the window larger, then smaller ([`drive_resize`]); the sub-gates follow each size ([`resize_checks`]).
    Resize,
    /// The resize stage on window-sized (a sw600dp resource, no configChanges): the shrink crosses 600 dp, so the activity relaunches; the grow crosses nothing and does not.
    Relaunch,
    /// The resize stage on window-declared (the same resource, the resize bits declared): no relaunch, each change reaches onConfigurationChanged.
    Declared,
    /// The declared stage on window-start: the launcher starts Main (startActivity) and finishes; Main resumes, and each resize reaches it as onConfigurationChanged.
    StartFinish,
    /// Two outputs, the second at scale 1.6: the first goes off, the window moves, and Android's density follows (xtask-harness/src/window/move_output.rs).
    Move,
    /// UI mode at scale 1.6: software SurfaceViews below and above a software window; the readback against the picture worked out from the fixture's spec (xtask-harness/src/window/surface_view.rs).
    SurfaceView,
    /// UI mode: a WebView (the provider placed, FEATURE_WEBVIEW's test file) with a native View over it; the readback against the picture worked out from the fixture's spec (xtask-harness/src/window/webview.rs; webview plan W3a).
    #[value(name = "webview")]
    WebView,
    /// UI mode: an in-game survey's arrangement, a full-window WebView added at runtime over an animating focused GLSurfaceView; resize, click and three Escs on the real GTK path (xtask-harness/src/window/webview_overlay.rs).
    #[value(name = "webview-overlay")]
    WebViewOverlay,
    /// UI mode: a SurfaceView's thread keeps posting while the UI thread sleeps 2 s; the window keeps committing at its own idle rate and the producer tracks it (xtask-harness/src/window/ui_block.rs; display-thread S5d).
    UiBlock,
    /// UI mode, the ui-block fixture: the proxy answers the window's frame callbacks at 1/4 of the output rate; the SurfaceView producer must be held to that slow display, none of its frames replaced
    /// unseen (xtask-harness/src/window/slow_display.rs; display-thread S4).
    SlowDisplay,
    /// The gtk stage on window-hwui (hardware accelerated): onDraw gets hwui's RecordingCanvas, and the frame hwui's RenderThread draws through EGL reaches the window's picture as the quadrants.
    Hwui,
    /// UI mode: a dimming dialog with surfaceInsets and a popup near the bottom over a software window; the readback against the picture worked out from the fixture's spec (xtask-harness/src/window/dialog.rs).
    Dialog,
    /// The GLES2 fixture in UI mode, its `.so` a guest library through the native bridge: the app's readback and the window's picture at its probes are the expectation (xtask-harness/src/window/gles2.rs; GS2.5).
    Gles2,
    /// UI mode, routing keys empty: a Java GLSurfaceView that never sets a client version (ES1 default) draws with GL10 on the driver's embedded ANGLE; the app's readback, its GL_VERSION and the window's picture at its probes (xtask-harness/src/window/gles1.rs; gl-honesty H2b).
    Gles1,
    /// UI mode: the probe sends the maximized control, then the suspended state through the proxy; the platform layer must log each as the toplevel's state (xtask-harness/src/window/suspended.rs; window lifecycle S0).
    Suspended,
    /// UI mode with the portrait fixture (xtask-harness/testdata/window-portrait): the readback and every layout are [`portrait_expected`] on the gate output.
    Portrait,
    /// UI mode, the window tiled: the proxy gives every compositor configure the size [`TILE`], the first included, as a tiling compositor sizes a window without asking it; the Task's bounds, every layout, the display metrics and the picture are the tile's content ([`tiled_checks`]).
    Tiled,
    /// UI mode with the orientation fixture (xtask-harness/testdata/window-orientation): opened landscape, the activity's setRequestedOrientation(PORTRAIT) reshapes the window to AOSP's portrait size
    /// and the activity is told the portrait configuration ([`orientation_checks`]).
    Orientation,
    /// UI mode, two processes of one uid (window-aux): a `:aux` process registered before the window's first layout, killed, restarted, then a dialog; the main window's and the dialog's layers stay in the main process's compositor (xtask-harness/src/window/aux_process.rs).
    AuxProcess,
    /// UI mode: the last `xdg_toplevel.set_app_id` the window sends before its first commit is `atl.<package>` ([`app_id_check`]).
    AppId,
    /// UI mode: the window's `xdg_toplevel.set_title`s are the task's label as recents shows it, then the TaskDescription label the fixture sets ([`title_check`]).
    Title,
    /// UI mode, the gtk fixture: the display capture through the harness route (atld `capture`, atl-screencap) is the quadrants at the opening size, and a caller that is not the shell or system is refused with SECURITY (xtask-harness/src/window/capture.rs).
    Capture,
    /// UI mode with `ro.atl.splash` set: the window shows the starting window before ART boots, and the app's first present replaces it; time-to-splash and time-to-first-app-frame (xtask-harness/src/window/splash.rs).
    Splash,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Resume => "resume",
            Stage::Frames => "frames",
            Stage::Input => "input",
            Stage::Gtk => "gtk",
            Stage::Widgets => "widgets",
            Stage::MouseTouch => "mouse-touch",
            Stage::Ime => "ime",
            Stage::Resize => "resize",
            Stage::Relaunch => "relaunch",
            Stage::Declared => "declared",
            Stage::StartFinish => "start-finish",
            Stage::Move => "move",
            Stage::SurfaceView => "surface-view",
            Stage::WebView => "webview",
            Stage::WebViewOverlay => "webview-overlay",
            Stage::UiBlock => "ui-block",
            Stage::SlowDisplay => "slow-display",
            Stage::Hwui => "hwui",
            Stage::Dialog => "dialog",
            Stage::Gles2 => "gles2",
            Stage::Gles1 => "gles1",
            Stage::Suspended => "suspended",
            Stage::AuxProcess => "aux-process",
            Stage::Portrait => "portrait",
            Stage::Tiled => "tiled",
            Stage::Orientation => "orientation",
            Stage::AppId => "app-id",
            Stage::Title => "title",
            Stage::Capture => "capture",
            Stage::Splash => "splash",
        }
    }

    /// How long the app must survive after the marker: the resume window, or the longer watch of the frames and input stages.
    fn window_ms(self) -> u64 {
        match self {
            Stage::Resume => WINDOW_MS,
            Stage::Frames | Stage::Input => FRAMES_WINDOW_MS,
            Stage::Gtk
            | Stage::SurfaceView
            | Stage::UiBlock
            | Stage::Hwui
            | Stage::Gles2
            | Stage::Gles1
            | Stage::Portrait
            | Stage::Tiled
            | Stage::Orientation
            | Stage::AppId
            | Stage::Title
            | Stage::Splash => GTK_WINDOW_MS,
            Stage::Dialog => DIALOG_WINDOW_MS,
            Stage::SlowDisplay => slow_display::WINDOW_MS,
            Stage::Capture => capture::CAPTURE_WINDOW_MS,
            // the webview stage's picture, then four inputs each waiting for the page's answer
            Stage::Widgets | Stage::Ime | Stage::MouseTouch | Stage::WebView => WIDGETS_WINDOW_MS,
            Stage::WebViewOverlay => WEBVIEW_OVERLAY_WINDOW_MS,
            Stage::Resize
            | Stage::Relaunch
            | Stage::Declared
            | Stage::StartFinish
            | Stage::Move
            | Stage::Suspended => RESIZE_WINDOW_MS,
            Stage::AuxProcess => aux_process::WINDOW_MS,
        }
    }
}

/// The stages whose run ends when their probe is done, not at the watch's end: the ime probe kills the app and relaunches it, a cold start (8-20 s under load, 2026-11-21) the 12 s watch
/// cut short, so the stop landed anywhere in the relaunch (its bind missing, the death counted from the exit summary, the relaunched process's half-started counters judged). LIMIT still bounds it.
/// webview-overlay: its probe waits for every answer it judges (the last Esc's back line), so the rest of its 30 s watch was idle time (rows 49-55 s, 2026-11-21).
/// aux-process: its probe waits for the dialog after a cold start of the new `:aux` process, as long as the ime relaunch.
fn probe_ends_run(stage: Stage) -> bool {
    matches!(
        stage,
        Stage::Ime | Stage::WebViewOverlay | Stage::AuxProcess
    )
}

/// The stages whose watch ends [`WINDOW_MS`] after their probe is done (never before marker + WINDOW_MS), not at the stage's own watch end: their probe waits for every line or event its
/// sub-gates judge, so after it the watch only looks for a fault its last input caused, and the resume path's derivation of WINDOW_MS (a failure shows within ms) holds for that input too.
/// The composer's counters line lags work by at most 1 s (atl-composer host.rs COUNTERS_PERIOD_NS), inside the grace. Per stage (check rows):
/// surface-view: its last wait is the offscreen commit line, then the second settled picture; tiled: the tile rewrite and pictures come at the first draw, the probe's last wait the block's pong;
/// ime: the relaunched window's rebind line; webview-overlay: the last Esc's back line. Not here: slow-display (warm-up / steady / block / tail phases are the assertion), ui-block (its timed
/// commit-flow check spans the watch), and stages nobody audited for this. `--hold` keeps the fixed watch.
fn probe_bounds_watch(stage: Stage) -> bool {
    matches!(
        stage,
        Stage::SurfaceView | Stage::Tiled | Stage::Ime | Stage::WebViewOverlay
    )
}

/// What ended a window stage's watch; printed on its `window: phases` line, counted by check's gates step (check_gates.rs `WatchTally`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchEnd {
    /// [`WINDOW_MS`] after the probe was done ([`probe_bounds_watch`]), or the probe's own end in a [`probe_ends_run`] stage past the stage's watch.
    Probe,
    /// The stage's watch end, marker + its window (the timeout).
    Window,
    /// The app or atld ended first.
    Ended,
    /// [`LIMIT`] passed without the watch ending.
    Limit,
}

impl WatchEnd {
    pub(crate) const ALL: [WatchEnd; 4] = [Self::Probe, Self::Window, Self::Ended, Self::Limit];

    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Window => "window",
            Self::Ended => "ended",
            Self::Limit => "limit",
        }
    }
}

/// The phases line's head (`window: phases start->marker=<ms> watch=<ms> teardown=<ms> end=<word>`), parsed by check_gates.rs.
pub(crate) const PHASES_LINE: &str = "window: phases ";

/// When the watch ends (ms on the log clock) and why, once known; None while a [`probe_ends_run`] probe still runs. `marker_ms`: the marker's stamp; `window_ms`: the stage's watch;
/// `probe_done_ms`: when the loop saw the probe finished; `bounded`: [`probe_bounds_watch`] (false under `--hold`).
fn watch_end(
    marker_ms: u64,
    window_ms: u64,
    probe_done_ms: Option<u64>,
    ends_run: bool,
    bounded: bool,
) -> Option<(u64, WatchEnd)> {
    let window_end = marker_ms + window_ms;
    match (probe_done_ms, ends_run, bounded) {
        (None, true, _) => None,
        (Some(done), _, true) => {
            let early = (marker_ms + WINDOW_MS).max(done + WINDOW_MS);
            if ends_run || early < window_end {
                Some((early, WatchEnd::Probe))
            } else {
                Some((window_end, WatchEnd::Window))
            }
        }
        (Some(done), true, false) if done > window_end => Some((done, WatchEnd::Probe)),
        _ => Some((window_end, WatchEnd::Window)),
    }
}

/// The stages whose probe takes pictures of the window through [`capture::Probe`] (the harness route, not the composer's log).
fn takes_pictures(stage: Stage) -> bool {
    matches!(
        stage,
        Stage::Capture
            | Stage::Gtk
            | Stage::Hwui
            | Stage::Portrait
            | Stage::Tiled
            | Stage::Gles2
            | Stage::Gles1
            | Stage::Widgets
            | Stage::SurfaceView
            | Stage::WebView
            | Stage::Dialog
            | Stage::Resize
            | Stage::Relaunch
            | Stage::Declared
            | Stage::StartFinish
    )
}

/// One sub-gate's result under its name (printed `window: <name>: pass|FAIL`); the name travels with the result, so no list of names can fall out of step with the checks.
pub(crate) type Sub = (&'static str, Result<String, String>);

/// A stage's sub-gates: functional ones, always judged, and timed ones, whose bound assumes a render time (starvation.rs decides whether this run's host left them unjudged).
pub struct Checks {
    pub subs: Vec<Sub>,
    pub timed: Vec<starvation::Timed>,
}

/// The fixture's dir.
pub(crate) const FIXTURE: &str = "xtask-harness/testdata/window";
/// The widget fixture's dir (`xtask test drive` taps its Button).
pub(crate) const WIDGETS_FIXTURE: &str = widgets::FIXTURE;
/// The same app with a sw600dp resource (their java/ links to FIXTURE's): no configChanges; the resize bits declared.
const SIZED_FIXTURE: &str = "xtask-harness/testdata/window-sized";
const DECLARED_FIXTURE: &str = "xtask-harness/testdata/window-declared";
/// window-declared's Main behind a launcher that starts it and finishes (the stage start-finish).
const START_FIXTURE: &str = "xtask-harness/testdata/window-start";
/// FIXTURE hardware accelerated (the stage hwui).
const HWUI_FIXTURE: &str = "xtask-harness/testdata/window-hwui";
/// FIXTURE fixed to portrait and resizeable (stage portrait): its java/ links to FIXTURE's.
const PORTRAIT_FIXTURE: &str = "xtask-harness/testdata/window-portrait";
/// FIXTURE with no manifest orientation, requesting portrait at its first create (stage orientation): its java/ links to FIXTURE's.
const ORIENTATION_FIXTURE: &str = "xtask-harness/testdata/window-orientation";
/// The fixture's first onDraw on hwui's RecordingCanvas (Canvas.isHardwareAccelerated).
const HARDWARE_DRAW_LINE: &str = "onDraw reached hardware=true";
/// Scenes the compositor must present into the window before its readback (the fixture's first frame and a repaint).
const GTK_PRESENTED: u64 = 2;
/// How long a cold start may take to reach the marker (as `xtask test app`).
pub(crate) const LIMIT: Duration = Duration::from_secs(180);
/// How long the app must survive after the marker. The resume path (WindowManagerGlobal.addView, the window of the activity) runs right after onResume and fails within
/// milliseconds when it fails, so two seconds leaves the failure time to show.
pub(crate) const WINDOW_MS: u64 = 2_000;
/// The frames and input stages' watch after the marker: the first draw, then FRAME_CALLBACKS vsyncs (0.5 s at 60 Hz), with slack for a slow start; the input stage's touches come after the first draw, inside it.
const FRAMES_WINDOW_MS: u64 = 5_000;
/// The gtk stage's watch after the marker: the first draw, the pointer script, the probe's block and the pong after it.
const GTK_WINDOW_MS: u64 = 10_000;
/// The dialog stage's watch after the marker: the probe's drags, the resize, and the back case (three Backs, each with its settle and waits), so longer than the gtk watch. The app stays up after the last Back (minimized), so the watch always runs to its end.
const DIALOG_WINDOW_MS: u64 = 30_000;
/// The widgets stage's watch after the marker: the first draw and the readback, then three injections, each waiting up to LINE_WAIT for its fixture line.
const WIDGETS_WINDOW_MS: u64 = 12_000;
/// The webview-overlay stage's watch after the marker: the WebView's late add and load (~5 s measured on a quiet host), paint settle 2 s, resize settle 2 s, then a click and three Escs, each up to its 5 s
/// INPUT_WAIT (webview_overlay.rs): ~29 s at worst.
const WEBVIEW_OVERLAY_WINDOW_MS: u64 = 30_000;
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
/// The title stage's labels: the fixture manifest's `<application android:label>` (its activity has none, so ActivityInfo.loadLabel falls back to it), and Main.java TASK_LABEL with the line logged
/// once it is set.
const FIXTURE_LABEL: &str = "ATL window test";
const TASK_LABEL: &str = "ATL task description";
const TASK_LINE: &str = "AtlWindow: task description set";
/// The mean interval may differ from the display period by this share: the callbacks run on the app's main thread, which the vsync timer does not wait for.
const CADENCE_TOLERANCE: f64 = 0.10;
/// No single interval may exceed this many periods: a missed vsync shows as two.
const CADENCE_MAX_PERIODS: f64 = 1.5;
/// No interval may be a period less this or shorter: SurfaceFlinger's `kSnapToSameVsyncWithin` (VsyncSchedule.cpp), the closest VSyncDispatch lets two app vsyncs be (`nextVsyncTooClose`).
const CADENCE_SNAP_NS: f64 = 3e6;
/// The input stage's pointer script (atld `--headless-pointer`): a down at (200, 300), then a move and an up at (260, 340), one pointer, identity transform.
const POINTER_SCRIPT: &str = "down:200,300 move:260,340 up:260,340";
/// The resize stage's configures, grow then shrink: xdg_toplevel size (logical px) and the serial of the injected configure, far from the compositor's sequential serials (the proxy absorbs the client's acks of these). The content is the configured size less the client-side decorations (a header bar, far under 200 px), so grow is over the opening size (opening_size below, 960x600 on the gates' 1280x800 display) and shrink under it in both dimensions.
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
        | Stage::Capture
        | Stage::Portrait
        | Stage::Tiled
        | Stage::Orientation
        | Stage::AppId
        | Stage::Title
        | Stage::Splash
        | Stage::Hwui
        | Stage::Widgets
        | Stage::Ime
        | Stage::Resize
        | Stage::Relaunch
        | Stage::Declared
        | Stage::StartFinish
        | Stage::Move
        | Stage::UiBlock
        | Stage::SlowDisplay
        | Stage::Suspended
        | Stage::AuxProcess
        | Stage::MouseTouch => "AtlWindow: onResume reached",
        Stage::SurfaceView => surface_view::MARKER,
        Stage::WebView => webview::MARKER,
        Stage::WebViewOverlay => webview_overlay::MARKER,
        Stage::Dialog => dialog::MARKER,
        Stage::Gles2 => gles2::MARKER,
        Stage::Gles1 => gles1::MARKER,
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

/// The counters before the marker (their totals), then each counted line from the marker on, with its offset from it.
fn print_calls(all: &[Event], marked: u64) {
    for c in before(all, marked) {
        println!("window: before onResume: {}", show(&c));
    }
    for e in all.iter().filter(|e| e.at_ms >= marked) {
        println!(
            "window: +{} ms after onResume: {} {} (total {})",
            e.at_ms - marked,
            e.counter.outcome,
            e.counter.api,
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
pub(crate) fn quadrant_pixels(width: u32, height: u32) -> Vec<u8> {
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

/// Stage `gtk` sub-gate (a): the window mapped at its first frame-clock tick, and the compositor presented at least [`GTK_PRESENTED`] scenes into it (run_apk::presented_total).
fn gtk_window_check(app: &str) -> Result<String, String> {
    if !app
        .lines()
        .any(|l| l.contains("atl-composer: ui: first frame mapped=true"))
    {
        return Err("the window never mapped at its first frame-clock tick".to_owned());
    }
    let presented = crate::run_apk::presented_total(app);
    if presented < GTK_PRESENTED {
        return Err(format!(
            "{presented} scenes presented, {GTK_PRESENTED} wanted"
        ));
    }
    Ok(format!("mapped, {presented} scenes presented"))
}

/// The AOSP size of a portrait, resizeable app on a landscape display (`(width, height)`), written out from AOSP, not read from atl-services.
/// The app's orientation mismatches the display's, so calculateInitialBounds takes its landscape-display branch (DesktopModeBoundsCalculator.java:165-185): the height is the ideal (side x 0.75f, truncated; :50-51, :314-318), the width (int)(tdaHeight / aspect + 0.5f).
/// The aspect is the display's own, width / height: maintainer decision 2026-11-08 (option A), DisplayContent.java:6647-6654 ignores orientation requests, so getDesiredAspectRatio takes its branch (c).
fn portrait_expected((width, height): (i32, i32)) -> (i32, i32) {
    let aspect = width as f32 / height as f32;
    (
        (height as f32 / aspect + 0.5) as i32,
        (height as f32 * 0.75) as i32,
    )
}

/// Configuration.ORIENTATION_LANDSCAPE / ORIENTATION_PORTRAIT (Configuration.java:713-714) and ActivityInfo.SCREEN_ORIENTATION_PORTRAIT (1), the fixture's request (window-orientation's meta-data).
const CONFIG_LANDSCAPE: i32 = 2;
const CONFIG_PORTRAIT: i32 = 1;
const REQUESTED_PORTRAIT: i32 = 1;
/// The fixture's line after its setRequestedOrientation (Main.java requestOrientation).
const REQUESTED_LINE: &str = "AtlWindow: requested orientation";
/// The fixture's process configuration (Application resources + Resources.getSystem()) at onCreate and at each onConfigurationChanged (Main.java logProcessConfiguration).
const PROCESS_CREATED_LINE: &str = "AtlWindow: process configuration created";
const PROCESS_CHANGED_LINE: &str = "AtlWindow: process configuration changed";
/// The fixture's IWindowManager.getInitialDisplayDensity(0), called by reflection at onCreate (Main.java logInitialDensity).
const INITIAL_DENSITY_LINE: &str = "AtlWindow: initial display density";

/// Sub-gate `initial-density`: getInitialDisplayDensity of the default display is the gate display's dpi (WindowManagerService.getInitialDisplayDensity:6158-6162 = DisplayContent's initial
/// density; no density is forced on the gate's display). An unanswered transaction reads 0 in the AIDL proxy: WeChat then mis-sizes its phone Welcome screen.
fn initial_density_check(app: &str, display: DisplayModel) -> Result<String, String> {
    let want = display.dpi.to_string();
    match app.lines().find(|l| l.contains(INITIAL_DENSITY_LINE)) {
        Some(line) if word(line, "density") == Some(want.as_str()) => {
            Ok(format!("getInitialDisplayDensity(0) = {want}"))
        }
        Some(line) => Err(format!("{}: want density={want}", line.trim())),
        None => Err("the fixture never asked getInitialDisplayDensity".to_owned()),
    }
}

/// Sub-gates `process-opened` / `process-reshaped`: the process configuration `line` (Application's and Resources.getSystem()'s) is a window of `size` px on `display`: dp = px / (dpi / 160),
/// rounded (DisplayContent.computeScreenAppConfiguration), portrait when width <= height. AOSP: the process follows its activity launched last (WindowProcessController.updateActivityConfigurationListener),
/// so not the display's size.
fn process_config_check(
    line: Option<&str>,
    size: (i32, i32),
    display: DisplayModel,
) -> Result<String, String> {
    let density = display.dpi as f32 / DENSITY_DEFAULT as f32;
    let dp = |px: i32| (px as f32 / density + 0.5) as i32;
    let want_dp = (dp(size.0), dp(size.1));
    let orientation = if want_dp.0 <= want_dp.1 {
        CONFIG_PORTRAIT
    } else {
        CONFIG_LANDSCAPE
    };
    let Some(line) = line else {
        return Err("the fixture never logged its process configuration".to_owned());
    };
    let seen = |key: &str, okey: &str| {
        (
            word(line, key).and_then(parse_wxh),
            word(line, okey).and_then(|o| o.parse::<i32>().ok()),
        )
    };
    let want = (Some(want_dp), Some(orientation));
    let (app, system) = (
        seen("app", "appOrientation"),
        seen("system", "systemOrientation"),
    );
    if app == want && system == want {
        Ok(format!(
            "application + system {}x{} dp orientation {orientation}",
            want_dp.0, want_dp.1
        ))
    } else {
        Err(format!(
            "application {app:?}, system {system:?}; want {want_dp:?} dp orientation {orientation} (a {}x{} px window)",
            size.0, size.1
        ))
    }
}

/// Stage `orientation` sub-gates on the app's log, against values worked out from AOSP and the gate's display, not read from the services:
/// `requested`: getRequestedOrientation returns the request. `opened`: the first layout is the landscape opening size (AOSP's ideal size, [`opening_size_i32`]), so the change is the request's.
/// `reshaped`: the last layout is AOSP's portrait size for the display ([`portrait_expected`]) as content and window frame, in a portrait configuration. `told`: the activity (size and orientation
/// declared) got the change as onConfigurationChanged with a portrait orientation and the portrait width in dp (px / (dpi / 160), rounded), and was created once (no relaunch).
/// `process-opened` / `process-reshaped`: the process configuration at onCreate is the opening window's, after the reshape the portrait window's ([`process_config_check`]).
fn orientation_checks(app: &str, display: DisplayModel) -> Vec<Sub> {
    let opening = opening_size_i32(display);
    let want = portrait_expected((display.width, display.height));
    let want_dp = (want.0 as f32 / (display.dpi as f32 / 160.0) + 0.5) as i32;
    let layouts: Vec<&str> = app.lines().filter(|l| l.contains(LAYOUT_LINE)).collect();
    let layout_of = |line: &str| {
        (
            word(line, "size").and_then(parse_wxh),
            word(line, "window").and_then(parse_wxh),
            word(line, "orientation").and_then(|o| o.parse::<i32>().ok()),
        )
    };
    let requested = match app.lines().find(|l| l.contains(REQUESTED_LINE)) {
        Some(line) if word(line, "get") == Some(&REQUESTED_PORTRAIT.to_string()) => {
            Ok(line.trim().to_owned())
        }
        Some(line) => Err(format!("{line}: want get={REQUESTED_PORTRAIT}")),
        None => Err("the fixture never requested an orientation".to_owned()),
    };
    let opened = match layouts.first().map(|l| layout_of(l)) {
        Some((Some(size), Some(window), Some(CONFIG_LANDSCAPE)))
            if size == opening && window == opening =>
        {
            Ok(format!(
                "first layout {}x{} landscape",
                opening.0, opening.1
            ))
        }
        Some(seen) => Err(format!(
            "first layout {seen:?}, want {opening:?} landscape ({CONFIG_LANDSCAPE})"
        )),
        None => Err("the fixture never laid out".to_owned()),
    };
    let reshaped = match layouts.last().map(|l| layout_of(l)) {
        Some((Some(size), Some(window), Some(CONFIG_PORTRAIT)))
            if size == want && window == want =>
        {
            Ok(format!(
                "last of {} layouts {}x{} portrait",
                layouts.len(),
                want.0,
                want.1
            ))
        }
        Some(seen) => Err(format!(
            "last layout {seen:?}, want {want:?} portrait ({CONFIG_PORTRAIT})"
        )),
        None => Err("the fixture never laid out".to_owned()),
    };
    let creates = app.matches(CREATE_LINE).count();
    let told_portrait = app.lines().filter(|l| l.contains(CHANGED_LINE)).any(|l| {
        word(l, "orientation") == Some(&CONFIG_PORTRAIT.to_string())
            && word(l, "screenWidthDp") == Some(&want_dp.to_string())
    });
    let told = if creates == 1 && told_portrait {
        Ok(format!(
            "onConfigurationChanged screenWidthDp={want_dp} orientation={CONFIG_PORTRAIT}, 1 onCreate"
        ))
    } else {
        Err(format!(
            "{creates} onCreate, portrait change (screenWidthDp={want_dp}) told: {told_portrait}"
        ))
    };
    let line_of = |key: &str, last: bool| {
        let mut lines = app.lines().filter(move |l| l.contains(key));
        if last {
            lines.next_back()
        } else {
            lines.next()
        }
    };
    vec![
        ("requested", requested),
        ("opened", opened),
        ("reshaped", reshaped),
        ("told", told),
        (
            "process-opened",
            process_config_check(line_of(PROCESS_CREATED_LINE, false), opening, display),
        ),
        (
            "process-reshaped",
            process_config_check(line_of(PROCESS_CHANGED_LINE, true), want, display),
        ),
        ("initial-density", initial_density_check(app, display)),
    ]
}

/// Stage `portrait` sub-gate (b): every layout's content view (`size=`) and window frame (`window=`, the relayout frame) are `want`, on the gate's display (`display=`).
/// A layout at another size is the window laid out before its bounds applied, or after they changed.
fn layout_check(app: &str, want: (i32, i32), display: (i32, i32)) -> Result<String, String> {
    let layouts: Vec<&str> = app.lines().filter(|l| l.contains(LAYOUT_LINE)).collect();
    if layouts.is_empty() {
        return Err("the fixture never laid out".to_owned());
    }
    for line in &layouts {
        let size = word(line, "size").and_then(parse_wxh);
        let window = word(line, "window").and_then(parse_wxh);
        let shown = word(line, "display").and_then(parse_wxh);
        if size != Some(want) || window != Some(want) || shown != Some(display) {
            return Err(format!(
                "layout {line}, want size and window {want:?} on display {display:?}"
            ));
        }
    }
    Ok(format!(
        "{} layouts at {}x{} (content and window), display {}x{}",
        layouts.len(),
        want.0,
        want.1,
        display.0,
        display.1
    ))
}

/// Stage `tiled`'s tile, xdg_toplevel size in logical px (the gate output is scale 1): what a tiling compositor gives the window in every configure, the first included. Under the opening size (opening_size, 1440x900 on the 1920x1200 gate display) in both dimensions, as Hyprland gave fossify-math 1080x675 for its 1350x844 (2026-11-22), so content drawn at the opening size is cut.
const TILE: (i32, i32) = (840, 540);
/// The client-side decorations are a header bar only (no side borders inside the xdg geometry), far under this height: the content is the tile's width by its height less the header.
const HEADER_MAX: i32 = 200;

/// Stage `tiled` sub-gates. The content size is derived from the tile: its width exactly, its height less a header in `0..HEADER_MAX`, read off the first layout and then required everywhere. AOSP sizes the window to its bounds before the first draw (relayout), so every layout is that size, not only the last.
fn tiled_checks(
    app: &str,
    services: &str,
    display: DisplayModel,
    rewritten: u64,
    seen: &Mutex<capture::Seen>,
) -> Vec<Sub> {
    let tile = if rewritten > 0 {
        Ok(format!(
            "{rewritten} compositor configure(s) tiled at {}x{}",
            TILE.0, TILE.1
        ))
    } else {
        Err("the proxy tiled no configure: the window never got the tile".to_owned())
    };
    let content = tiled_content(app);
    let display_size = (display.width, display.height);
    let relayout = content
        .clone()
        .and_then(|want| layout_check(app, want, display_size));
    let bounds = content.clone().and_then(|want| {
        let bounds = task_bounds(services);
        // the services log a change only: no line means the Task kept its opening size
        let last = bounds.last().copied().unwrap_or(opening_size_i32(display));
        if last == want {
            Ok(format!("the Task's bounds {bounds:?} end at {want:?}"))
        } else {
            Err(format!(
                "the Task's bounds {bounds:?} end at {last:?}, want the tile's content {want:?}"
            ))
        }
    });
    let picture = content
        .clone()
        .and_then(|(w, h)| capture::judged(seen, 0, (w as u32, h as u32), false));
    let dp = content.and_then(|(w, _)| {
        // density = dpi / DENSITY_DEFAULT, dp = px / density (exact at the gate's 160 dpi)
        let want_dp = w * DENSITY_DEFAULT / display.dpi;
        let dps: Vec<&str> = app
            .lines()
            .filter(|l| l.contains(LAYOUT_LINE))
            .filter_map(|l| word(l, "screenWidthDp"))
            .collect();
        if !dps.is_empty() && dps.iter().all(|d| d.parse::<i32>().ok() == Some(want_dp)) {
            Ok(format!("screenWidthDp {want_dp} at every layout"))
        } else {
            Err(format!(
                "screenWidthDp {dps:?}, want {want_dp} at every layout"
            ))
        }
    });
    vec![
        ("tile", tile),
        ("mapped", gtk_window_check(app)),
        ("relayout", relayout),
        ("bounds", bounds),
        ("content", picture),
        ("display", dp),
    ]
}

/// The tile's content size off the fixture's first layout: width = the tile's, height = the tile's less a header under [`HEADER_MAX`]. A first layout of another width is the window laid out before the tile reached the app.
fn tiled_content(app: &str) -> Result<(i32, i32), String> {
    let first = app
        .lines()
        .find(|l| l.contains(LAYOUT_LINE))
        .ok_or("the fixture never laid out")?;
    let (width, height) = word(first, "size")
        .and_then(parse_wxh)
        .ok_or_else(|| format!("no size in {first}"))?;
    if width != TILE.0 || height > TILE.1 || height <= TILE.1 - HEADER_MAX {
        return Err(format!(
            "first layout {width}x{height}, want the tile's content: width {}, height {} less a header under {HEADER_MAX}",
            TILE.0, TILE.1
        ));
    }
    Ok((width, height))
}

/// Stage `app-id` sub-gate: the last `xdg_toplevel.set_app_id` sent before the window's first commit (the initial one, no buffer) is `want`. The desktop matches the window's rules on that commit; GTK's default id before it does not count, and an id sent after it is too late.
fn app_id_check(
    seen: &AppIdsSeen,
    want: &str,
    liveness: &Result<Liveness, String>,
) -> Result<String, String> {
    liveness.as_ref().map_err(Clone::clone)?;
    if !seen.first_commit {
        return Err("the window never committed".to_owned());
    }
    match seen.before_first_commit.last() {
        Some(last) if last == want => Ok(format!(
            "set_app_id {:?} before the first commit, last {want}",
            seen.before_first_commit
        )),
        Some(last) => Err(format!(
            "last set_app_id before the first commit {last}, want {want} (before {:?}, after {:?})",
            seen.before_first_commit, seen.after_first_commit
        )),
        None => Err(format!(
            "the window sent no set_app_id before its first commit (after {:?})",
            seen.after_first_commit
        )),
    }
}

/// Stage `title` sub-gate: the window's titles, in order, hold `label` (recents' fallback: the root activity's label as PackageManager loads it), then `task_label` after it, and end at `task_label`
/// (the fixture's setTaskDescription retitled it). A title from the window's LayoutParams (Main.java WINDOW_TITLE) never matches either.
fn title_check(
    titles: &[String],
    label: &str,
    task_label: &str,
    liveness: &Result<Liveness, String>,
) -> Result<String, String> {
    liveness.as_ref().map_err(Clone::clone)?;
    let Some(first) = titles.iter().position(|t| t == label) else {
        return Err(format!("no title {label:?} (titles {titles:?})"));
    };
    if !titles[first..].iter().any(|t| t == task_label)
        || titles.last().map(String::as_str) != Some(task_label)
    {
        return Err(format!(
            "titles {titles:?}: want {task_label:?} after {label:?} and last"
        ));
    }
    Ok(format!("titles {titles:?}"))
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

/// Ime stage: atld outlived the killed app (maintainer decision 2026-11-06: an app's death is an event, not the run's end) and ended on the control socket's `stop` with status 0. In atld's
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

/// The fixture's onNewIntent line (window Main.java) for Start's second start, the services' delivery line (activity_task.rs send) and their refusal count (atl-diag).
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

/// The app processes atld started (the launcher's processes under it, and the sandbox running each).
pub(crate) fn app_pids(atld: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| runs_launcher(&c))
        })
        .filter(|&pid| descends_from(pid, atld))
        .collect()
}

/// Whether a NUL-separated cmdline runs the launcher: an argument whose file name is it (the program, or bwrap's command). Not a substring: a sandbox of atld's services bind-mounts the runtime library dirs, `<out>/crates/atl-app-launcher/lib` among them.
fn runs_launcher(cmdline: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    cmdline.split(|&b| b == 0).any(|arg| {
        Path::new(std::ffi::OsStr::from_bytes(arg))
            .file_name()
            .is_some_and(|f| f == APP_LAUNCHER)
    })
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
pub(crate) fn start_proxy(upstream: &Path) -> Result<Proxy> {
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?);
    Proxy::start(
        upstream,
        &runtime.join(format!("atl-window-gate-{}.sock", std::process::id())),
    )
}

/// What the probe saw of the client: its liveness (gtk, widgets), or the acks of the injected configures (resize, where the liveness fields stay at their defaults).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Liveness {
    /// The pong to the first ping came within [`PONG_WAIT`].
    answered: bool,
    /// No pong to the second ping within [`SILENT_WAIT`], while the fixture's UI thread slept.
    silent: bool,
    /// Ms from the second ping to its pong, after the sleep (None: no pong within [`RESUME_WAIT`]).
    resumed_ms: Option<u128>,
    /// Resize stage: whether the client acked each of [`RESIZE_STEPS`], in order (empty in the other stages).
    configured: Vec<bool>,
    /// Widgets stage: the idle window's log offsets (None in the other stages).
    idle: Option<widgets::Idle>,
    /// Dialog stage: the window sent `xdg_toplevel.set_minimized` after the unhandled root Back (false in the other stages).
    minimized: bool,
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
        idle: None,
        minimized: false,
    })
}

/// The app ids the window sent, split at its first commit (the initial one), in order; `first_commit` once that commit came.
#[derive(Clone, Debug, Default)]
struct AppIdsSeen {
    before_first_commit: Vec<String>,
    after_first_commit: Vec<String>,
    first_commit: bool,
}

/// The app-id stage's probe, from the proxy's side: once the window has drawn, the requests it made before its first commit are in the proxy's channel. Its app ids go to `seen`, split at the first commit; the settle leaves room for one sent just after the draw.
fn drive_app_id(
    proxy: Proxy,
    app_log: &Path,
    seen: &Mutex<AppIdsSeen>,
) -> Result<Liveness, String> {
    wait_for_line(app_log, DRAW_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never drew")?;
    std::thread::sleep(INPUT_SETTLE);
    let events: Vec<ProxyEvent> = proxy.events.try_iter().map(|(_, event)| event).collect();
    if !events.iter().any(|e| matches!(e, ProxyEvent::Acked { .. })) {
        return Err("the window never acked a configure".to_owned());
    }
    let mut split = AppIdsSeen::default();
    for event in events {
        match event {
            ProxyEvent::AppId { app_id } if split.first_commit => {
                split.after_first_commit.push(app_id)
            }
            ProxyEvent::AppId { app_id } => split.before_first_commit.push(app_id),
            ProxyEvent::FirstCommit => split.first_commit = true,
            _ => {}
        }
    }
    *seen.lock().map_err(|e| e.to_string())? = split;
    Ok(Liveness::default())
}

/// The title stage's probe, from the proxy's side: once the fixture logged its setTaskDescription ([`TASK_LINE`]), and a settle for the retitle to reach the window, the titles it sent go to `seen`.
fn drive_title(
    proxy: Proxy,
    app_log: &Path,
    seen: &Mutex<Vec<String>>,
) -> Result<Liveness, String> {
    wait_for_line(app_log, TASK_LINE, ACK_WAIT)
        .then_some(())
        .ok_or("the fixture never set its task description")?;
    std::thread::sleep(RESIZE_SETTLE);
    let titles = proxy
        .events
        .try_iter()
        .filter_map(|(_, event)| match event {
            ProxyEvent::Title { title } => Some(title),
            _ => None,
        })
        .collect();
    *seen.lock().map_err(|e| e.to_string())? = titles;
    Ok(Liveness::default())
}

/// The resize stage's probe, from the proxy's side: once the window is up and has drawn, it is configured to each of [`RESIZE_STEPS`] in turn. Each configure must be acked; then the probe leaves the client [`RESIZE_SETTLE`] to relayout and present.
/// The window's picture is recorded after each of the two steps' settle, so the pictures are grow's, then shrink's ([`frame_check`]).
fn drive_resize(proxy: Proxy, app_log: &Path, probe: &capture::Probe) -> Result<Liveness, String> {
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
            .inject_configure(width, height, serial, &[])
            .map_err(|e| e.to_string())?;
        let acked = await_event(&proxy.events, PONG_WAIT, |e| {
            *e == ProxyEvent::Acked { serial }
        })
        .is_some();
        std::thread::sleep(RESIZE_SETTLE);
        Ok(acked)
    };
    configured.push(step(grow)?);
    probe.record_settled();
    for (width, height, serial) in BURST_STEPS {
        proxy
            .inject_configure(width, height, serial, &[])
            .map_err(|e| e.to_string())?;
        std::thread::sleep(BURST_GAP);
    }
    std::thread::sleep(RESIZE_SETTLE);
    configured.push(step(shrink)?);
    probe.record_settled();
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
        idle: None,
        minimized: false,
    })
}

/// Whether `needle` shows up in the file within `within` (polled: the app log is written by another process).
fn wait_for_line(file: &Path, needle: &str, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if atl_diag::logfiles::read(file).contains(needle) {
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

/// Stage `gtk`: the real launcher's module armed its ping hold (atl-platform-ui-linux ping.rs `report`; display-thread plan D3): libgtk's `wl_proxy_add_listener` bound to the module and GDK's xdg_wm_base listener was kept. Its line is logged once, at attach; not armed names its reason.
fn ping_hold_check(app: &str) -> Result<String, String> {
    match app.lines().find(|l| l.contains("display: ping hold")) {
        Some(l) if l.contains("display: ping hold armed") => Ok("ping hold armed".to_owned()),
        Some(l) => Err(l.trim().to_owned()),
        None => Err("no ping hold line in the app log".to_owned()),
    }
}

/// The module's attach record names the display thread it started (atl-platform-ui-linux module.rs `ui_attach`, ui.rs `DISPLAY_THREAD`).
const ATTACHED_ON_DISPLAY: &str = "module attached: GTK";
const DISPLAY_THREAD: &str = "on thread atl-display";

/// An ART record: libbase's logger (ART's `LOG`, routed through the launcher's logger) carries its source position, `<file>.cc:<line>`; no record logged before the VM exists has one.
fn is_art_line(line: &str) -> bool {
    line.split_whitespace().any(|t| {
        t.split_once(".cc:").is_some_and(|(file, n)| {
            !file.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        })
    })
}

/// Stage `gtk` sub-gate `startup-order` (display-thread plan S5d, proof 4): the display thread exists and owns the toolkit before ART boots, so no Java can reach the module first: the module's attach record, naming thread `atl-display`, precedes the first ART record in the app log.
fn startup_order_check(app: &str) -> Result<String, String> {
    let lines: Vec<&str> = app.lines().collect();
    let attached = lines
        .iter()
        .position(|l| l.contains(ATTACHED_ON_DISPLAY))
        .ok_or("no module attach line in the app log")?;
    if !lines[attached].contains(DISPLAY_THREAD) {
        return Err(format!(
            "the attach line names no display thread: {}",
            lines[attached].trim()
        ));
    }
    let art = lines
        .iter()
        .position(|l| is_art_line(l))
        .ok_or("no ART line in the app log")?;
    if attached < art {
        Ok(format!(
            "attach on atl-display at line {}, first ART line {}",
            attached + 1,
            art + 1
        ))
    } else {
        Err(format!(
            "ART logged (line {}) before the module attached (line {})",
            art + 1,
            attached + 1
        ))
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

/// `--output`'s value: a `WxH` of positive sides.
fn parse_output_size(text: &str) -> Result<(i32, i32), String> {
    parse_wxh(text)
        .filter(|(width, height)| *width > 0 && *height > 0)
        .ok_or_else(|| format!("{text}: want WxH"))
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

/// Stage `resize` sub-gate (d): the picture recorded after grow is the quadrants at grow's Task size, the one after shrink the quadrants at shrink's (the pictures are the window's content).
fn frame_check(
    seen: &Mutex<capture::Seen>,
    steps: &Result<Steps, String>,
) -> Result<String, String> {
    let &(grow, shrink) = steps.as_ref().map_err(Clone::clone)?;
    let size = |(w, h): (i32, i32)| (w as u32, h as u32);
    let grow_line = capture::judged(seen, 0, size(grow), false)?;
    let shrink_line = capture::judged(seen, 1, size(shrink), false)?;
    Ok(format!("grow: {grow_line}; shrink: {shrink_line}"))
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

/// Session.relayoutAsync (oneway, IWindowSession.aidl:110) is served: an unserved call leaves the client's frame at its old size, so no `unimplemented IWindowSession.relayoutAsync` counter may appear. The frames themselves are `relayout_check`'s and `frame_check`'s.
fn relayout_async_check(services: &str) -> Result<String, String> {
    let unserved = services
        .lines()
        .filter(|line| line.contains("unimplemented IWindowSession.relayoutAsync"))
        .count();
    if unserved > 0 {
        return Err(format!(
            "{unserved} relayoutAsync call(s) unimplemented: the client keeps its old frame"
        ));
    }
    let served = services
        .lines()
        .filter(|line| line.contains("answered IWindowSession.relayoutAsync"))
        .count();
    Ok(format!(
        "relayoutAsync served: {served} call(s), none unimplemented"
    ))
}

/// The resize stages' sub-gates up to `display`; the stage adds click, burst, lifecycle.
fn resize_checks(
    app: &str,
    services: &str,
    display: DisplayModel,
    liveness: &Result<Liveness, String>,
    seen: &Mutex<capture::Seen>,
) -> Vec<Sub> {
    let steps = resize_steps(&task_bounds(services), opening_size_i32(display));
    vec![
        ("configure", configure_check(liveness)),
        ("bounds", bounds_check(services, &steps)),
        ("relayout", relayout_check(app, &steps)),
        ("relayoutAsync", relayout_async_check(services)),
        ("frame", frame_check(seen, &steps)),
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

/// The last value of a services counter (`services: <kind> <interface>.<method> N at=T`); 0 if it never counted.
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

/// The frame time (ns) of an `AtlWindow: frame N time=T` line: the fixture's Choreographer callback.
fn frame_time(line: &str) -> Option<u64> {
    let (_, rest) = line.split_once("AtlWindow: frame ")?;
    word(rest, "time")?.parse().ok()
}

/// Sub-gate (b): the first FRAME_CALLBACKS callbacks after the marker come once per display period. The mean interval is within CADENCE_TOLERANCE of the period, and no
/// interval is longer than CADENCE_MAX_PERIODS periods. Times are the vsync's (the display clock's grid point each callback was scheduled for, not a wall-clock stamp of the app's own),
/// so the mean does not depend on when the app ran its callbacks; a missed interval is a vsync the client did not ask for in time. A failed judgement while the host was starved
/// (`atl_host_load::judge_window`, share over the window) is `undecidable`: still a failure, so `land` needs a rerun that decides.
pub(crate) fn cadence_check(
    after: &[&str],
    refresh_hz: f32,
    rate: Rate,
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
    judge_window(cadence_judge(&times, refresh_hz, rate), share, "the window")
}

/// What the run's display rate promises the cadence sub-gates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rate {
    /// A fixed-rate output (the gate's headless labwc, the 60 Hz free-running grid): mean and longest interval are asserted.
    Fixed,
    /// A real session display (remote gates): VRR stretches intervals and Wayland has no portable VRR query (`wp_presentation.refresh` may stay the max rate), so mean and longest are printed
    /// `host-stated`, never failed; count, monotonic times and the shortest bound (VRR never runs faster than the max rate) still are.
    Session,
}

fn cadence_judge(times: &[u64], refresh_hz: f32, rate: Rate) -> Result<String, String> {
    if times.len() < FRAME_CALLBACKS {
        return Err(format!(
            "{} frame callbacks after the marker, want {FRAME_CALLBACKS}",
            times.len()
        ));
    }
    let period = 1e9 / f64::from(refresh_hz);
    let mut longest = 0.0_f64;
    let mut shortest = f64::MAX;
    for pair in times.windows(2) {
        let gap = pair[1]
            .checked_sub(pair[0])
            .ok_or_else(|| format!("frame time goes back: {} then {}", pair[0], pair[1]))?;
        longest = longest.max(gap as f64);
        shortest = shortest.min(gap as f64);
    }
    if shortest + CADENCE_SNAP_NS <= period {
        return Err(format!(
            "shortest interval {:.3} ms, under a period of {:.3} ms less SurfaceFlinger's 3 ms snap",
            shortest / 1e6,
            period / 1e6
        ));
    }
    let span = times[times.len() - 1] - times[0];
    let mean = span as f64 / (times.len() - 1) as f64;
    if rate == Rate::Session {
        return Ok(format!(
            "{FRAME_CALLBACKS} callbacks, shortest {:.3} ms >= P - 3 ms; host-stated mean={:.3} ms longest={:.3} ms P={:.3} ms",
            shortest / 1e6,
            mean / 1e6,
            longest / 1e6,
            period / 1e6
        ));
    }
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

/// The fixture's line at its first draw (Main.java Quadrants.onDraw): the view's touch mode then.
const TOUCH_MODE_LINE: &str = "AtlWindow: touch-mode first-draw=";

/// Sub-gate: the window was added in touch mode (WMS addWindow ADD_FLAG_IN_TOUCH_MODE while the display is, config_defaultInTouchMode true -> ViewRootImpl's first traversal), so the
/// first frame draws no focus highlight. Out of touch mode a touch would call IWindowManager.setInTouchMode; in it, an all-touch script calls nothing.
fn touch_mode_check(app: &str) -> Result<String, String> {
    match app
        .lines()
        .find_map(|l| l.split_once(TOUCH_MODE_LINE).map(|(_, v)| v.trim()))
    {
        Some("true") => Ok("touch-mode: first draw in touch mode".to_owned()),
        Some(v) => Err(format!(
            "touch-mode: first draw touch mode {v}, AOSP starts in touch mode"
        )),
        None => Err("touch-mode: no first-draw touch-mode line".to_owned()),
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

/// GTK's own debug output in the app log (GDK_DEBUG=dmabuf, set by this gate's runs): every scene buffer imported as a dmabuf texture, none downloaded through mmap (`gdk_dmabuf`), except on a tier whose own lines allow it (`Verdict::tier`); the tier is logged.
fn dmabuf_check(app: &str) -> Result<String, String> {
    let v = crate::gdk_dmabuf::Verdict::of(app);
    if let Some(tier) = v.tier() {
        Ok(format!("tier {tier}: {v}"))
    } else {
        Err(format!("dmabuf present: {v}"))
    }
}

/// Vsync-flagged presentation samples the composer needs before it trusts a hardware vsync source: AOSP's minimum before its model predicts (VsyncSchedule.cpp `createTracker`:
/// `kMinSamplesForPrediction` 6). Spec value, not read from the composer.
const VSYNC_TIER_SAMPLES: u64 = 6;
/// The composer's tier line, `atl-composer: vsync source <tier> (<reason>) ...` (first at start, then once per change).
const VSYNC_TIER_LINE: &str = "atl-composer: vsync source ";

/// The vsync source tier the run chose matches what the host's presentation feedback stated, never the host's name. The host's side is `host`: the proxy's count of the compositor's `presented`
/// events by `kind.vsync` (wayland_proxy.rs [`Presented`](crate::wayland_proxy::Presented)), independent of the composer's reading of the flags; None = no proxy, so no feedback the run could
/// see (non-UI stages) = (0, 0). Vsync-flagged >= [`VSYNC_TIER_SAMPLES`] and no others, on CLOCK_MONOTONIC (the platform's clock line) -> `hardware-vsync`, reached by exactly one change (it
/// starts free-running) and kept; fewer -> `free-running`, never changed. Mixed kinds are red: the tier would hinge on their order. The composer's counters and last tier line name the tier;
/// `--break-ignore-vsync-flag` (the composer reads every sample as not vsync) must turn this red on a hardware-vsync host.
fn vsync_tier_check(app: &str, host: Option<(u64, u64)>) -> Result<String, String> {
    let line = crate::composer::last_counters(app).ok_or("no `composer: counters` line")?;
    let n = |k: &str| crate::composer::field(line, k).unwrap_or(0);
    let tier = word(line, "vsync_source").ok_or("no vsync_source in the counters")?;
    let (vsync, other, changes) = (
        n("vsync_samples"),
        n("vsync_samples_other"),
        n("vsync_source_changes"),
    );
    let said = app
        .lines()
        .rev()
        .find_map(|l| l.split_once(VSYNC_TIER_LINE).map(|(_, rest)| rest))
        .ok_or("no `vsync source` tier line")?;
    let reason = said
        .split_once(" (")
        .and_then(|(_, r)| r.split_once(')'))
        .map_or("", |(r, _)| r);
    if !said.starts_with(&format!("{tier} ")) {
        return Err(format!(
            "counters say {tier}, the last tier line says {said:?}"
        ));
    }
    let clock = app
        .lines()
        .find_map(|l| {
            l.split_once("wp_presentation clock_id=")
                .map(|(_, c)| c.trim())
        })
        .map_or("none".to_owned(), |c| format!("clock_id={c}"));
    let (host_vsync, host_other) = host.unwrap_or((0, 0));
    let seen = host.map_or("no proxy".to_owned(), |(v, o)| {
        format!("proxy presented vsync={v} other={o}")
    });
    let evidence = format!(
        "{reason}; {seen}; composer vsync_samples={vsync} other={other} changes={changes} {clock}"
    );
    let want = if host_vsync >= VSYNC_TIER_SAMPLES {
        if host_other > 0 {
            return Err(format!(
                "host feedback mixes vsync and other samples: {evidence}"
            ));
        }
        if clock != "clock_id=1" {
            return Err(format!(
                "vsync samples counted on a host clock other than CLOCK_MONOTONIC: {evidence}"
            ));
        }
        ("hardware-vsync", 1)
    } else {
        ("free-running", 0)
    };
    if (tier, changes) != want {
        return Err(format!(
            "tier {tier} after {changes} changes, the host's feedback asks {} after {}: {evidence}",
            want.0, want.1
        ));
    }
    Ok(format!("{tier} ({evidence})"))
}

/// The release of a replaced buffer waited for the window's paint: every read fence a paint exported rode a release (none unmatched), and when a shown buffer was retired for the paint
/// (`releases_held`), at least one release carried its fence. A release sent before the paint (`--break-ignore-hold`) leaves the fence of that paint with no release to ride.
/// Keyed on shown buffers, not on `buffers_released`: a buffer replaced in a scene the window never took (a same-thread or unpainted latch over an untaken scene) was never read, and goes back at
/// once with its acquire fence (AOSP's dropped-buffer path), so a run whose only release is such a buffer has `1 released, 0 with a read fence` and nothing wrong.
/// `Ok(None)` = not judged: no shown buffer was retired, so there was nothing to judge; the caller names it apart, never as passed. A counters line without the release fields is an error (a renamed or
/// dropped field must not read as 0).
fn release_check(app: &str) -> Result<Option<String>, String> {
    let line = crate::composer::last_counters(app).ok_or("no `composer: counters` line")?;
    let n = |k: &str| {
        crate::composer::field(line, k)
            .ok_or_else(|| format!("no `{k}` in the counters line: {}", line.trim_end()))
    };
    let (released, shown, fenced, unmatched) = (
        n("buffers_released")?,
        n("releases_held")?,
        n("releases_fenced")?,
        n("releases_fenced_unmatched")?,
    );
    let summary = format!(
        "{released} released, {shown} shown retired for the paint, {fenced} with a read fence, {unmatched} fences unmatched"
    );
    if unmatched != 0 || (shown > 0 && fenced == 0) {
        Err(format!("release before paint: {summary}"))
    } else if shown == 0 {
        Ok(None)
    } else {
        Ok(Some(summary))
    }
}

/// Stages whose run retires a shown buffer for a paint, so the release check must judge: a not-judged release fails them. Measured from the gate logs (2026-11-20, every widgets / ime /
/// surface-view run with the `releases_held` field had it > 0; surface-view also proves it in `replaced_check`). gtk, portrait, hwui and dialog draw a static first frame: no log shows them retiring one.
fn release_must_judge(stage: Stage) -> bool {
    matches!(stage, Stage::Widgets | Stage::Ime | Stage::SurfaceView)
}

/// [`release_check`] as a sub-check: `None` when it judged nothing on a stage that may legitimately not ([`release_must_judge`]).
fn release_sub(app: &str, stage: Stage) -> Option<Result<String, String>> {
    match release_check(app) {
        Ok(Some(s)) => Some(Ok(s)),
        Ok(None) if release_must_judge(stage) => Some(Err(
            "no shown buffer retired for a paint (releases_held=0): this stage replaces one, the release went unjudged".to_owned(),
        )),
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    }
}

/// The platform's log line of the content origin (atl-platform-ui `GtkWindow::present`), read by [`content_origin`].
const CONTENT_ORIGIN_LINE: &str = "atl-platform-ui: content origin ";

/// The content's origin in the surface, as the platform logs it once the window is mapped: an injected surface point lands in the content at `point - origin`.
pub(crate) fn content_origin(app: &str) -> Result<(f32, f32), String> {
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

/// The surface point of content pixel `(x, y)`: the content origin (logical surface units) plus the pixel over the window's scale, both from the platform's origin line. At scale 1 the pixel itself.
pub(crate) fn content_to_surface(app: &str, (x, y): (f64, f64)) -> Result<(f64, f64), String> {
    let (ox, oy) = content_origin(app)?;
    let line = app
        .lines()
        .find(|l| l.contains(CONTENT_ORIGIN_LINE))
        .ok_or_else(|| "no content origin line: the window never presented".to_owned())?;
    let scale: f64 = word(line, "scale")
        .and_then(|v| v.parse().ok())
        .filter(|s: &f64| *s > 0.0)
        .ok_or_else(|| format!("no positive scale in {line:?}"))?;
    Ok((f64::from(ox) + x / scale, f64::from(oy) + y / scale))
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
    let app = atl_diag::logfiles::read(app_log);
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

/// The window run's key: its work dir `<out>/<key>` and the runtime's `<key>-art` / `<key>-fonts` (atld_run.rs `Runtime::new`). Named as its check row (`window-surface-view`, `window-webview-overlay-touch`; check_gates.rs `Row::name`), so side-by-side rows never share a dir. `--esc-back` is not in the name, as in `Row::name`; a hold adds `-hold`.
pub fn work_key(args: &Args) -> String {
    let stage =
        clap::ValueEnum::to_possible_value(&args.stage).expect("every stage has a CLI name");
    let mut key = format!("window-{}", stage.get_name());
    if args.tap_origin != webview_overlay::TapOrigin::Mouse {
        let tap = clap::ValueEnum::to_possible_value(&args.tap_origin)
            .expect("every tap origin has a CLI name");
        key = format!("{key}-{}", tap.get_name());
    }
    if args.hold.is_some() {
        key.push_str("-hold");
    }
    key
}

pub fn run(root: &Path, args: &Args) -> Result<ExitCode> {
    if args.self_test {
        return Ok(self_test());
    }
    let _ = Layout::of(root)?;
    // host CPU pressure through the run: the cadence checks tell a starved host from a client that missed frames
    let sampler = Sampler::start();
    let key = work_key(args);
    // hwui's GraphicBufferMapper aborts the app without the mapper ("gralloc-mapper is missing"); the window path always has it, as the app and composer gates do
    let rt = Runtime::new(
        root,
        "window",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        &key,
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver(root)?
    // the native-bridge guest: every window stage carries it, as `app` does (the GLES2 fixture's library is a guest library)
    .with_native_bridge_guest(&args.host)?;
    if args.hold.is_some() && !matches!(args.stage, Stage::Gtk | Stage::Widgets | Stage::Resize) {
        anyhow::bail!("--hold needs a UI stage (gtk, widgets or resize)");
    }
    if args.output.is_some() && !matches!(args.stage, Stage::Gtk | Stage::Portrait | Stage::AppId) {
        anyhow::bail!("--output needs stage gtk, portrait or app-id");
    }
    let work = rt.out.join(&key);
    let mut proxy = None;
    // declared before atld, so dropped after it; a hold keeps the desktop's compositor
    let mut nested = None;
    let rt = match args.stage {
        Stage::Input => rt.with_headless_pointer(POINTER_SCRIPT),
        Stage::Gtk
        | Stage::Capture
        | Stage::Portrait
        | Stage::Tiled
        | Stage::Orientation
        | Stage::AppId
        | Stage::Title
        | Stage::Splash
        | Stage::Hwui
        | Stage::Widgets
        | Stage::Ime
        | Stage::Resize
        | Stage::Relaunch
        | Stage::Declared
        | Stage::StartFinish
        | Stage::Move
        | Stage::SurfaceView
        | Stage::WebView
        | Stage::WebViewOverlay
        | Stage::UiBlock
        | Stage::SlowDisplay
        | Stage::Dialog
        | Stage::Gles2
        | Stage::Gles1
        | Stage::Suspended
        | Stage::AuxProcess
        | Stage::MouseTouch => {
            // a hold has no proxy: the compositor sees the window alone, so its border and size are the maintainer's drags
            if args.hold.is_some() {
                rt.with_ui("gtk")
            } else {
                let gate = args.output.map_or(GATE_OUTPUT, |(width, height)| Output {
                    width,
                    height,
                    ..GATE_OUTPUT
                });
                let outputs: &[Output] = match args.stage {
                    Stage::Widgets | Stage::Ime | Stage::MouseTouch => &[widgets::OUTPUT],
                    Stage::SurfaceView => &[surface_view::OUTPUT],
                    Stage::Move => &move_output::OUTPUTS,
                    _ => std::slice::from_ref(&gate),
                };
                let n = NestedCompositor::start_with(&work.join("nested"), outputs)?;
                if matches!(args.stage, Stage::Move) {
                    // the window opens on the first output (move_output.rs)
                    n.control().only(0)?;
                }
                let p = start_proxy(&n.socket)?;
                if matches!(args.stage, Stage::Move) {
                    // before atld starts: its app processes' contexts see Hyprland's policy (move_output.rs)
                    p.withhold_in_contexts(&move_output::WITHHELD);
                }
                if matches!(args.stage, Stage::Tiled) {
                    // before atld starts: the window's first configure is the tile's
                    p.tile(TILE.0, TILE.1);
                }
                let rt = rt.with_ui("gtk").with_wayland(&p.listen);
                let feature = [
                    (args.break_z_order, "atl-composer/break-z-order"),
                    (args.break_post_draw, "atl-composer/break-post-draw"),
                    (args.break_queue, "atl-composer/break-queue"),
                    (args.break_ignore_hold, "atl-composer/break-ignore-hold"),
                    (
                        args.break_no_backpressure,
                        "atl-composer/break-no-backpressure",
                    ),
                    (
                        args.break_ignore_vsync_flag,
                        "atl-composer/break-ignore-vsync-flag",
                    ),
                    (args.break_input_frames, "atl-composer/break-input-frames"),
                    (args.break_input_no_wake, "atl-composer/break-input-no-wake"),
                    (args.break_outbox, "atl-composer/break-outbox"),
                    (args.break_handle_hold, "atl-composer/break-handle-hold"),
                    (args.break_ime_copy, "atl-ime/break-copy"),
                ]
                .into_iter()
                .find_map(|(on, feature)| on.then_some(feature));
                let rt = match feature {
                    Some(feature) => rt.with_ui_break(root, feature)?,
                    None => rt,
                };
                proxy = Some(p);
                nested = Some(n);
                rt
            }
        }
        Stage::Resume | Stage::Frames => rt,
    };
    let rt = if matches!(args.stage, Stage::Splash) {
        splash::setup(rt, args.break_splash)?
    } else {
        rt
    };
    // the bindings sub-gate reads glibc's records from the app log (test-environment setup: the product reads neither variable)
    let mut env = args.app_env.clone();
    // GTK's dmabuf lines: the import verdict and its tier (dmabuf_check, gdk_dmabuf.rs); only this gate reads them, and on a download tier GDK writes one per frame, so other runs go without
    env.push(format!("GDK_DEBUG={}", crate::gdk_dmabuf::GDK_DEBUG));
    if matches!(args.stage, Stage::Hwui) {
        env.push("LD_DEBUG=bindings".to_owned());
        if args.break_global_module {
            env.push(format!("LD_PRELOAD={}", crate::ui_module::MODULE_FILE));
        }
        if args.break_global_runtime {
            env.push(format!("LD_PRELOAD={}", xtask::launcher_closure::RUNTIME));
        }
    }
    let rt = rt.with_app_env(&env);
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
    let rt = if args.break_back_finish {
        rt.without_back_finish()
    } else {
        rt
    };
    let rt = if args.break_root_minimize {
        rt.with_root_back_close()
    } else {
        rt
    };
    let rt = if args.break_orientation_reshape {
        rt.with_orientation_unresizeable()
    } else {
        rt
    };
    // the webview stage's break takes the engine module away (a host without WebKit: no FEATURE_WEBVIEW)
    let rt = if matches!(args.stage, Stage::WebView) && args.break_webview_engine {
        rt.without_webview_engine()?
    } else {
        rt
    };
    let rt = if matches!(args.stage, Stage::WebView) && args.break_webview_sound {
        rt.without_pulse_server()
    } else {
        rt
    };
    // the desktop's mouse-as-one-finger adaptation (atld `--mouse-touch`): this stage's drag is a touchscreen's; its break is the adaptation off, the AOSP mouse
    let overlay_finger = matches!(args.stage, Stage::WebViewOverlay)
        && args.tap_origin == webview_overlay::TapOrigin::MouseFinger;
    let rt =
        if (matches!(args.stage, Stage::MouseTouch) && !args.break_mouse_touch) || overlay_finger {
            rt.with_mouse_touch()
        } else {
            rt
        };
    // the break runs atld with the adaptation off while the checks still expect it on
    let rt = if args.esc_back && !args.break_esc_back {
        rt.with_esc_back()
    } else {
        rt
    };
    let rt = if args.exit_with_last_app && !args.break_exit_with_last_app {
        rt.with_exit_with_last_app()
    } else {
        rt
    };
    // the move stage's second output, as Android's display
    let moved = nested
        .as_ref()
        .filter(|_| matches!(args.stage, Stage::Move))
        .map(|n| n.output_display(1));
    let fixture = fixture(args.stage);
    // the GLES2 fixture's library is built for the host arch (the identity guest), as its static gate builds it
    let native = match args.stage {
        Stage::Gles2 => Some(gles2_fixture::apk_libs(
            root,
            &crate::ndk::default_ndk(root)?,
            &args.host,
            &work.join("gles2-lib"),
            args.break_no_detach,
        )?),
        _ => None,
    };
    let apk = rt.apk.build(
        &root.join(fixture),
        &work.join("apk"),
        &[],
        native.as_ref(),
        None,
    )?;
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
    // the test app's uid: the run's only package's, as the allocator hands it out
    let app_uid = rt.first_app_uid();
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![Package::base(apk)],
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    let start_ms = now_ms();
    let started = Instant::now();
    // the ime stage's fake desktop input method holds the seat before the app's window exists
    let fake_ime = match (args.stage, &nested) {
        (Stage::Ime, Some(n)) => Some(crate::nested_wayland::Ime::bind(n)?),
        _ => None,
    };
    let ime_breaks = ime::Breaks {
        commit: args.break_ime_commit,
        delete: args.break_ime_delete,
    };
    // the webview stage's page plays a tone (webview.rs sound): into the run's own null sink, silent on the desktop, its monitor recorded (audio_pcm.rs)
    let sound = match args.stage {
        Stage::WebView => Some(webview::Sound::start(&run.scratch)?),
        _ => None,
    };
    // atld's streams and the run's pulse server target that sink (the break keeps the pulse server missing)
    let rt = match &sound {
        Some(s) if args.break_webview_sound => {
            rt.with_audio_routing(s.routing()).without_pulse_server()
        }
        Some(s) => rt.with_audio_routing(s.routing()),
        None => rt,
    };
    let mut atld = Atld::Child(rt.start_atld(&run)?);
    let out = live(
        LiveIn {
            args,
            run: &run,
            package: &m.name,
            activity: &activity,
            work: &work,
            proxy,
            nested: nested.as_ref(),
            fake_ime,
            ime_breaks,
            started,
        },
        &mut atld,
        sampler,
    )?;
    if let Some(secs) = args.hold {
        return Ok(hold_report(
            &run.log(&m.name),
            marker(args.stage),
            secs,
            out.ended.is_some(),
        ));
    }
    let heard = sound.map(webview::Sound::stop);
    judge(
        &Judged {
            root,
            args,
            run: &run,
            package: &m.name,
            display,
            rate: Rate::Fixed,
            moved,
            app_uid,
            start_ms,
        },
        out,
        heard,
    )
}

/// atld as the live half sees it: its own child (a local gate), or a process the target's run script started (the agent, [`agent`]), known by pid only.
enum Atld {
    Child(std::process::Child),
    Pid(u32),
}

impl Atld {
    fn id(&self) -> u32 {
        match self {
            Atld::Child(c) => c.id(),
            Atld::Pid(p) => *p,
        }
    }

    /// Its end, if it ended. A `Pid` has no status of its own (not this process's child): a placeholder 0, which the agent records only as "ended"; the build host reads the run script's `atld.rc`.
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        match self {
            Atld::Child(c) => Ok(c.try_wait()?),
            Atld::Pid(p) => Ok((!pid_alive(*p)).then(|| std::process::ExitStatus::from_raw(0))),
        }
    }

    /// [`Run::stop`] for either kind: `stop` on the control socket, SIGKILL after [`STOP_WAIT`].
    fn stop(&mut self, run: &Run) -> Result<std::process::ExitStatus> {
        match self {
            Atld::Child(c) => run.stop(c),
            Atld::Pid(_) => {
                if let Some(s) = self.try_wait()? {
                    return Ok(s);
                }
                let asked = run.control("stop");
                let deadline = Instant::now() + STOP_WAIT;
                while Instant::now() < deadline {
                    if let Some(s) = self.try_wait()? {
                        return Ok(s);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                println!("atld: no exit {STOP_WAIT:?} after stop ({asked:?}): killed");
                self.kill()
            }
        }
    }

    /// SIGKILL, then its end.
    fn kill(&mut self) -> Result<std::process::ExitStatus> {
        use std::os::unix::process::ExitStatusExt;
        match self {
            Atld::Child(c) => {
                let _ = c.kill();
                Ok(c.wait()?)
            }
            Atld::Pid(p) => {
                let _ = rustix::process::kill_process(
                    rustix::process::Pid::from_raw(*p as i32).context("pid 0")?,
                    rustix::process::Signal::KILL,
                );
                let deadline = Instant::now() + STOP_WAIT;
                while pid_alive(*p) && Instant::now() < deadline {
                    std::thread::sleep(POLL);
                }
                Ok(std::process::ExitStatus::from_raw(libc_sigkill_status()))
            }
        }
    }
}

/// The wait status of a SIGKILL death (signal number in the low bits).
fn libc_sigkill_status() -> i32 {
    rustix::process::Signal::KILL.as_raw()
}

/// Whether `pid` runs: its /proc entry exists and is not a zombie (the run script reaps it).
fn pid_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    })
}

/// What the live half needs from the prepared run.
struct LiveIn<'a> {
    args: &'a Args,
    run: &'a Run,
    package: &'a str,
    activity: &'a str,
    /// The stage's work dir (`run.root`'s parent): pictures go under it.
    work: &'a Path,
    proxy: Option<Proxy>,
    nested: Option<&'a NestedCompositor>,
    fake_ime: Option<crate::nested_wayland::Ime>,
    ime_breaks: ime::Breaks,
    /// atld's start: the run's [`LIMIT`] counts from here.
    started: Instant,
}

/// What the live half saw: the judge's inputs besides the logs.
struct LiveOut {
    liveness: Result<Liveness, String>,
    captured: Arc<Mutex<capture::Seen>>,
    app_ids: Arc<Mutex<AppIdsSeen>>,
    titles: Arc<Mutex<Vec<String>>>,
    splash_shot: Option<Result<splash::Shot, String>>,
    /// atld ended before the window closed (by itself, or stopped with the app's end).
    ended: Option<std::process::ExitStatus>,
    /// atld ended because the gate stopped it at the window's end.
    stopped: Option<std::process::ExitStatus>,
    pressure: Samples,
    /// The proxy's count of the compositor's `presented` feedback (vsync-flagged, other); None: the stage ran without a proxy (no Wayland connection the harness sees).
    presented: Option<(u64, u64)>,
    /// The proxy's frame-callback throttle (held, released); (0, 0) unless the stage throttled (slow-display).
    throttle: (u64, u64),
    /// The window surface's commit times on CLOCK_MONOTONIC ns (wayland_proxy `Commits`); None: no proxy, or a remote run (its target's clock, not carried in `live.json`).
    commits: Option<Vec<u64>>,
    /// The compositor configures the proxy tiled (wayland_proxy `Tile`); 0 unless the stage tiles (tiled).
    tiled: u64,
}

/// The live half of a window stage, next to atld and the compositor: the probe thread drives the client through the proxy, the poll loop watches the marker's window, the app's end and [`LIMIT`], then
/// atld is stopped. One implementation for `test window` (this host) and the target agent ([`agent`]).
fn live(l: LiveIn, atld: &mut Atld, sampler: Sampler) -> Result<LiveOut> {
    let LiveIn {
        args,
        run,
        package,
        activity,
        work,
        proxy,
        nested,
        fake_ime,
        ime_breaks,
        started,
    } = l;
    let atld_pid = atld.id();
    let outputs = nested.map(NestedCompositor::control);
    // the probe drives the client through its proxy from here on
    let stage = args.stage;
    let break_mouse_back = args.break_mouse_back;
    let esc_back = args.esc_back;
    let tap_origin = args.tap_origin;
    let break_suspended = args.break_suspended;
    // the app-id stage's probe leaves the window's app ids here; the sub-gates read them once the probe is joined
    let app_ids: Arc<Mutex<AppIdsSeen>> = Arc::default();
    // the title stage's probe leaves the window's titles here, in order
    let titles: Arc<Mutex<Vec<String>>> = Arc::default();
    // the probe records its pictures (and the capture stage's refused caller) in `captured`; the sub-gates read them once the probe is joined
    let captured: Arc<Mutex<capture::Seen>> = Arc::default();
    let capture_probe = takes_pictures(args.stage).then(|| capture::Probe {
        root: run.root.clone(),
        package: package.to_owned(),
        out_dir: work.join("capture"),
        break_caller: args.break_capture_caller,
        seen: Arc::clone(&captured),
        component: format!("{package}/{activity}"),
        timeout_close: !args.exit_with_last_app,
    });
    // the host's own vsync marking, counted by the proxy: the vsync-tier check's independent side
    let presented = proxy.as_ref().map(Proxy::presented);
    let tile = proxy.as_ref().map(Proxy::tiled);
    let throttle = proxy.as_ref().map(Proxy::throttle);
    let commits = proxy.as_ref().map(Proxy::commits);
    let probe = proxy.map(|p| {
        let log = run.log(package);
        let services_log = run.log("services");
        let seen = Arc::clone(&app_ids);
        let titles_seen = Arc::clone(&titles);
        let relaunch = (run.root.clone(), format!("{package}/{activity}"));
        std::thread::spawn(move || match (stage, outputs, fake_ime) {
            (Stage::Gtk | Stage::Hwui | Stage::Portrait | Stage::Tiled, _, _) => {
                match &capture_probe {
                    Some(capture_probe) => {
                        capture_probe.record_after_draw(&log);
                        drive(p, &log)
                    }
                    None => Err("no capture probe".to_owned()),
                }
            }
            (Stage::Capture, _, _) => match &capture_probe {
                Some(capture_probe) => {
                    capture::drive(p, &log, &services_log, capture_probe, atld_pid)
                }
                None => Err("no capture probe".to_owned()),
            },
            (Stage::Ime, _, Some(ime)) => {
                ime::drive(p, &log, ime, ime_breaks, atld_pid, &services_log, relaunch)
            }
            (Stage::Widgets, _, _) => match &capture_probe {
                Some(capture_probe) => {
                    widgets::drive(p, &log, break_mouse_back, esc_back, capture_probe)
                }
                None => Err("no capture probe".to_owned()),
            },
            (Stage::SurfaceView, _, _) => match &capture_probe {
                Some(capture_probe) => surface_view::drive(p, &log, capture_probe),
                None => Err("no capture probe".to_owned()),
            },
            (Stage::WebView, _, _) => match &capture_probe {
                Some(capture_probe) => webview::drive(p, &log, capture_probe),
                None => Err("no capture probe".to_owned()),
            },
            (Stage::WebViewOverlay, _, _) => webview_overlay::drive(p, &log, tap_origin),
            (Stage::UiBlock, _, _) => ui_block::drive(p, &log),
            // local only: the nested compositor's output is the gate output, whose rate the checks judge against too
            (Stage::SlowDisplay, _, _) => {
                slow_display::drive(p, &log, GATE_OUTPUT.refresh_mhz as f32 / 1000.0)
            }
            (Stage::MouseTouch, _, _) => widgets::drive_mouse_touch(p, &log),
            (Stage::Dialog, _, _) => match &capture_probe {
                Some(capture_probe) => dialog::drive(p, &log, capture_probe),
                None => Err("no capture probe".to_owned()),
            },
            (Stage::Gles2, _, _) => match &capture_probe {
                Some(capture_probe) => gles2::drive(p, &log, capture_probe),
                None => Err("no capture probe".to_owned()),
            },
            (Stage::Gles1, _, _) => match &capture_probe {
                Some(capture_probe) => gles1::drive(p, &log, capture_probe),
                None => Err("no capture probe".to_owned()),
            },
            (Stage::Resize | Stage::Relaunch | Stage::Declared | Stage::StartFinish, _, _) => {
                match &capture_probe {
                    Some(capture_probe) => drive_resize(p, &log, capture_probe),
                    None => Err("no capture probe".to_owned()),
                }
            }
            (Stage::AppId, _, _) => drive_app_id(p, &log, &seen),
            (Stage::Title, _, _) => drive_title(p, &log, &titles_seen),
            (Stage::AuxProcess, _, _) => aux_process::drive(p, &log),
            (Stage::Splash, _, _) => splash::drive(p, &log),
            (Stage::Suspended, _, _) => suspended::drive(p, &log, break_suspended),
            (Stage::Move, Some(outputs), _) => move_output::drive(p, &log, outputs),
            _ => drive(p, &log),
        })
    });
    let app_log = run.log(package);
    let marker = marker(args.stage);
    let window_ms = args.hold.map_or(args.stage.window_ms(), |s| s * 1_000);
    let read = atl_diag::logfiles::read;
    let mut ended = None;
    // the splash stage's picture, taken by this loop while the starting window shows
    let mut splash_shot = None;
    let splash_png = work.join("splash.png");
    let start_ms = now_ms().saturating_sub(started.elapsed().as_millis() as u64);
    let bounded = args.hold.is_none() && probe_bounds_watch(args.stage);
    let ends_run = probe_ends_run(args.stage);
    // when this loop first saw the probe finished (a stage without a probe: never)
    let mut probe_done_ms = None;
    let mut marker_ms = None;
    let end = loop {
        let app = read(&app_log);
        if matches!(args.stage, Stage::Splash) {
            splash::take(&mut splash_shot, &app, &app_log, nested, &splash_png);
        }
        if probe_done_ms.is_none() && probe.as_ref().is_some_and(|h| h.is_finished()) {
            probe_done_ms = Some(now_ms());
        }
        // the window runs from the marker's own stamp, not from when this loop saw the marker
        marker_ms = marker_ms.or_else(|| marker_stamp(&app, marker));
        if let Some((at, why)) = marker_ms
            .and_then(|t| watch_end(t, window_ms, probe_done_ms, ends_run, bounded))
            .filter(|(at, _)| now_ms() >= *at)
        {
            break (at, why);
        }
        if let Some(s) = atld.try_wait()? {
            ended = Some(s);
            break (now_ms(), WatchEnd::Ended);
        }
        // the app's end is the run's end for a gate, as when atld ended with it; not in the ime stage, whose probe kills the app and relaunches it (its death sub-gate), nor in the capture
        // stage, whose close ends the app (its `ended` sub-gate)
        if !matches!(args.stage, Stage::Ime | Stage::Capture) && run.app_ended() {
            let at = now_ms();
            ended = Some(atld.stop(run)?);
            break (at, WatchEnd::Ended);
        }
        if started.elapsed() > LIMIT + Duration::from_secs(args.hold.unwrap_or(0)) {
            break (now_ms(), WatchEnd::Limit);
        }
        std::thread::sleep(POLL);
    };
    let teardown = Instant::now();
    let mut stopped = None;
    if ended.is_none() {
        stop_apps(atld_pid);
        stopped = Some(if args.break_atld_stop {
            atld.kill()?
        } else {
            atld.stop(run)?
        });
    }
    let liveness = match probe {
        Some(handle) => handle
            .join()
            .unwrap_or_else(|_| Err("the probe thread panicked".to_owned())),
        None => Err("no probe: only the gtk, widgets and resize stages run one".to_owned()),
    };
    // start: the launch (after the fixture's build and the stack's start); watch: marker -> the loop's end; teardown: stopping the apps and atld, joining the probe
    let (end_ms, why) = end;
    let phase = |ms: Option<u64>| ms.map_or("-".to_owned(), |ms| ms.to_string());
    println!(
        "{PHASES_LINE}start->marker={} watch={} teardown={} end={} (watch cap {window_ms} ms, probe {})",
        phase(marker_ms.map(|t| t.saturating_sub(start_ms))),
        phase(marker_ms.map(|t| end_ms.saturating_sub(t))),
        teardown.elapsed().as_millis(),
        why.word(),
        match (bounded, ends_run) {
            (true, _) => "bounds the watch",
            (false, true) => "extends the watch",
            (false, false) => "does not end it",
        }
    );
    Ok(LiveOut {
        liveness,
        captured,
        app_ids,
        titles,
        splash_shot,
        ended,
        stopped,
        pressure: sampler.stop(),
        presented: presented.map(|p| p.counts()),
        throttle: throttle.map_or((0, 0), |t| t.counts()),
        commits: commits.map(|c| c.times_ns()),
        tiled: tile.map_or(0, |t| t.rewritten()),
    })
}

/// What the judge needs from the prepared run besides [`LiveOut`].
struct Judged<'a> {
    root: &'a Path,
    args: &'a Args,
    run: &'a Run,
    package: &'a str,
    /// The display atld answered with and the cadence is judged against.
    display: DisplayModel,
    rate: Rate,
    /// The move stage's second output.
    moved: Option<DisplayModel>,
    app_uid: u32,
    /// The run's start on the build host's clock, ms: the marker's offset is printed from it.
    start_ms: u64,
}

/// The judge of a window stage: the verdict and the stage's sub-gates over the run's logs and what the live half saw; prints them and the verdict line.
fn judge(j: &Judged, out: LiveOut, heard: Option<webview::Heard>) -> Result<ExitCode> {
    let Judged {
        root,
        args,
        run,
        package,
        display,
        rate,
        moved,
        app_uid,
        start_ms,
    } = *j;
    let LiveOut {
        liveness,
        captured,
        app_ids,
        titles,
        mut splash_shot,
        ended,
        stopped,
        pressure,
        presented,
        throttle,
        commits,
        tiled,
    } = out;
    let read = atl_diag::logfiles::read;
    let app_log = run.log(package);
    let services_log = run.log("services");
    let marker = marker(args.stage);
    let window_ms = args.stage.window_ms();
    let app = read(&app_log);
    let services = read(&services_log);
    let all = events(&services);
    let atld_log = read(&run.atld_log());
    // atld's end with the last app (--exit-with-last-app) is the run's normal end, not a fault
    let v = verdict(
        &app,
        marker,
        ended.is_some() && !capture::ended_with_last_app(&atld_log),
        window_ms,
    );
    if args.verbose {
        println!("---- app log\n{app}");
    }
    // the sub-gates, once the marker is logged (they read the frames or the touches after it); timed ones go through the one starvation rule below
    let mut timed: Vec<starvation::Timed> = Vec::new();
    let sub: Vec<Sub> = match (args.stage, v.marker_ms) {
        (Stage::Frames, Some(_)) => {
            vec![
                ("picture", picture_check(&app, display, WINDOW_TITLE)),
                (
                    "cadence",
                    cadence_check(
                        &after_marker(&app, marker),
                        display.refresh_hz,
                        rate,
                        &pressure,
                    ),
                ),
            ]
        }
        (Stage::Input, Some(_)) => vec![
            (
                "touch",
                input_check(&after_marker(&app, marker), POINTER_SCRIPT),
            ),
            ("touch-mode", touch_mode_check(&app)),
        ],
        (Stage::SurfaceView, Some(_)) => surface_view::checks(&app, &liveness, &captured),
        (Stage::WebView, Some(_)) => webview::checks(
            &app,
            &liveness,
            &captured,
            args.esc_back,
            heard.unwrap_or_else(|| webview::Heard {
                peak: Err("no recording".to_owned()),
                streams: Err("no recording".to_owned()),
            }),
            // the desktop file ID rule `atl.<package>`, as the app_id sub-gate expects of the window
            &format!("atl.{package}"),
            &atld_log,
        ),
        (Stage::WebViewOverlay, Some(_)) => webview_overlay::checks(
            &app,
            args.esc_back,
            args.tap_origin.focuses() != args.break_tap_origin,
        ),
        (Stage::UiBlock, Some(_)) => {
            let c = ui_block::checks(&app, display.refresh_hz, commits.as_deref());
            timed = c.timed;
            c.subs
        }
        (Stage::SlowDisplay, Some(_)) => {
            let c = slow_display::checks(&app, display.refresh_hz, throttle);
            timed = c.timed;
            c.subs
        }
        (Stage::MouseTouch, Some(_)) => widgets::mouse_touch_checks(&app, &liveness),
        (Stage::Dialog, Some(_)) => {
            dialog::checks(&app, &services, &liveness, &captured, ended.is_some())
        }
        (Stage::Gles2, Some(_)) => gles2::checks(&app, display, &liveness, &captured),
        (Stage::AuxProcess, Some(_)) => aux_process::checks(&app, &services, &liveness),
        (Stage::Gles1, Some(_)) => gles1::checks(root, &app, display, &liveness, &captured),
        (Stage::Suspended, Some(_)) => suspended::checks(&after_marker(&app, marker), &liveness),
        (Stage::Ime, Some(_)) => {
            let mut sub = ime::checks(&app, &services, &liveness, app_uid);
            sub.push(("run", run_survives_check(&read(&run.atld_log()), stopped)));
            sub
        }
        (Stage::Widgets, Some(_)) => widgets::checks(
            &app,
            &services,
            &after_marker(&app, marker),
            &liveness,
            display,
            rate,
            &pressure,
            &captured,
            args.esc_back,
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
            let mut sub = resize_checks(&app, &services, display, &liveness, &captured);
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
        (Stage::Portrait, Some(_)) => {
            let (width, height) = (display.width, display.height);
            let want = portrait_expected((width, height));
            vec![
                ("mapped", gtk_window_check(&app)),
                (
                    "content",
                    capture::judged(&captured, 0, (want.0 as u32, want.1 as u32), false),
                ),
                ("relayout", layout_check(&app, want, (width, height))),
                (
                    "touch",
                    content_origin(&app).and_then(|origin| {
                        track_check(&after_marker(&app, marker), POINTER_SCRIPT, origin)
                    }),
                ),
                ("pong", pong_check(&liveness)),
            ]
        }
        (Stage::Tiled, Some(_)) => {
            let mut sub = tiled_checks(&app, &services, display, tiled, &captured);
            sub.push(("pong", pong_check(&liveness)));
            sub
        }
        (Stage::Orientation, Some(_)) => {
            let mut sub = vec![("mapped", gtk_window_check(&app))];
            sub.extend(orientation_checks(&app, display));
            sub.push(("pong", pong_check(&liveness)));
            sub
        }
        (Stage::AppId, Some(_)) => {
            let seen = app_ids.lock().map(|seen| seen.clone()).unwrap_or_default();
            vec![(
                "app-id",
                app_id_check(&seen, &format!("atl.{package}"), &liveness),
            )]
        }
        (Stage::Title, Some(_)) => {
            let seen = titles.lock().map(|seen| seen.clone()).unwrap_or_default();
            vec![(
                "title",
                title_check(&seen, FIXTURE_LABEL, TASK_LABEL, &liveness),
            )]
        }
        (Stage::Capture, Some(_)) => {
            let mut sub = capture::checks(
                &captured,
                opening_size(display),
                &liveness,
                !args.exit_with_last_app,
            );
            sub.push((
                "atld-end",
                capture::atld_end_judged(&captured, &atld_log, ended, args.exit_with_last_app),
            ));
            sub
        }
        (Stage::Splash, Some(_)) => {
            let mut sub = splash::checks(&app, splash_shot.take(), opening_size(display));
            sub.push((
                "probe",
                liveness
                    .as_ref()
                    .map(|_| "the fixture drew".to_owned())
                    .map_err(Clone::clone),
            ));
            sub
        }
        (Stage::Hwui, Some(_)) => vec![
            ("mapped", gtk_window_check(&app)),
            ("hardware", hardware_check(&app)),
            (
                "bindings",
                bindings_check(root, &app, args.break_forget_reverse.as_deref()),
            ),
            (
                "readback",
                capture::judged(&captured, 0, opening_size(display), args.break_swap_rb),
            ),
        ],
        (Stage::Gtk, Some(_)) => vec![
            ("mapped", gtk_window_check(&app)),
            (
                "readback",
                capture::judged(&captured, 0, opening_size(display), false),
            ),
            (
                "touch",
                content_origin(&app).and_then(|origin| {
                    track_check(&after_marker(&app, marker), POINTER_SCRIPT, origin)
                }),
            ),
            ("pong", pong_check(&liveness)),
            ("ping-hold", ping_hold_check(&app)),
            ("startup-order", startup_order_check(&app)),
        ],
        _ => Vec::new(),
    };
    let mut sub = sub;
    // sub-checks that judged nothing: named on the pass line, never counted as passed
    let mut not_judged: Vec<&str> = Vec::new();
    if !sub.is_empty()
        && matches!(
            args.stage,
            Stage::Widgets
                | Stage::Gtk
                | Stage::Portrait
                | Stage::Hwui
                | Stage::Ime
                | Stage::SurfaceView
                | Stage::Dialog
        )
    {
        // the ime stage's log holds two processes: judge the one the stage drove (ime::first_process)
        let judged = if matches!(args.stage, Stage::Ime) {
            ime::first_process(&app)
        } else {
            &app
        };
        sub.push(("post-draw", post_draw_check(judged, &services)));
        sub.push(("dmabuf", dmabuf_check(judged)));
        match release_sub(judged, args.stage) {
            Some(r) => sub.push(("release", r)),
            None => {
                println!(
                    "window: release: not judged (releases_held=0: no shown buffer retired for a paint)"
                );
                not_judged.push("release");
            }
        }
    }
    // each vsync source tier's gate: local stages run free-running (headless labwc, clock_id=1 without the vsync flag), session displays that mark vsyncs run hardware-vsync
    if !sub.is_empty() && matches!(args.stage, Stage::Frames | Stage::Widgets | Stage::UiBlock) {
        sub.push(("vsync-tier", vsync_tier_check(&app, presented)));
    }
    // the one host-GPU-starvation rule, every stage: the window's own render after the marker, printed whether or not a timed sub-gate exists, so land logs show the host's state
    let mut undecided = Vec::new();
    if let Some(marked) = v.marker_ms {
        match starvation::host_render_us(&app, marked) {
            Some((median, max, n)) => println!(
                "window: host gpu: window render median {} ms, max {} ms over {n} paint lines after the marker",
                median / 1000,
                max / 1000
            ),
            None => println!("window: host gpu: no paint line after the marker"),
        }
        let (judged, unjudged) = starvation::apply(&app, args.stage.name(), marked, timed);
        sub.extend(judged);
        undecided = unjudged;
    }
    let sub_fault = sub.iter().find_map(|(_, r)| r.as_ref().err().cloned());
    for (name, result) in &sub {
        match result {
            Ok(s) => println!("window: {name}: pass {s}"),
            Err(s) => println!("window: {name}: FAIL {s}"),
        }
    }
    for (name, why, result) in &undecided {
        let (Ok(s) | Err(s)) = result;
        println!("window: {name}: undecided {why}: {s}");
    }
    // a real failure elsewhere still fails the run; undecided replaces only a pass
    if let (Some(_), None, None, Some((_, why, _))) =
        (v.marker_ms, &v.fault, &sub_fault, undecided.first())
    {
        // check_gates.rs `undecided_line` reads this line; the exit code alone carries the outcome
        println!(
            "window: {}{}: {why}",
            crate::verdict::UNDECIDED_WORD,
            args.stage.name()
        );
        return Ok(ExitCode::from(crate::verdict::UNDECIDED_EXIT));
    }
    if let (Some(marked), None, None) = (v.marker_ms, &v.fault, &sub_fault) {
        let skip_note = if not_judged.is_empty() {
            String::new()
        } else {
            format!("; not judged: {}", not_judged.join(", "))
        };
        println!(
            "window: pass {} (`{marker}` logged {} ms after the start; no FATAL EXCEPTION, death or unserved-service wait in the {window_ms} ms after it{skip_note})",
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
    // the ime stage's red: its probe journal and every input-method line, not only the tail above (ime.rs `red_dump`)
    if matches!(args.stage, Stage::Ime) {
        ime::red_dump(&app);
    }
    let verdict = crate::verdict::Verdict {
        cmd: "window",
        total: None,
        skipped: Vec::new(),
        failures: vec![crate::verdict::Failure::step(args.stage.name(), why)],
    };
    println!("{}", verdict.line());
    Ok(ExitCode::FAILURE)
}

/// The stage's fixture dir.
fn fixture(stage: Stage) -> &'static str {
    match stage {
        Stage::Widgets | Stage::Ime | Stage::MouseTouch => widgets::FIXTURE,
        Stage::Relaunch => SIZED_FIXTURE,
        Stage::Declared => DECLARED_FIXTURE,
        Stage::StartFinish => START_FIXTURE,
        Stage::SurfaceView => surface_view::FIXTURE,
        Stage::WebView => webview::FIXTURE,
        Stage::WebViewOverlay => webview_overlay::FIXTURE,
        Stage::UiBlock => ui_block::FIXTURE,
        Stage::SlowDisplay => slow_display::FIXTURE,
        Stage::Hwui => HWUI_FIXTURE,
        Stage::Dialog => dialog::FIXTURE,
        Stage::Gles2 => gles2_fixture::FIXTURE,
        Stage::Gles1 => gles1::FIXTURE,
        Stage::Portrait => PORTRAIT_FIXTURE,
        Stage::Orientation => ORIENTATION_FIXTURE,
        Stage::AuxProcess => aux_process::FIXTURE,
        _ => FIXTURE,
    }
}

/// `window` args from words, parsed as `test window` parses its own: the target agent and `remote test window` get every default of the local gate.
fn args_of(words: &[String]) -> Result<Args> {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct Words {
        #[command(flatten)]
        args: Args,
    }
    let argv = std::iter::once("window".to_owned()).chain(words.iter().cloned());
    Ok(Words::try_parse_from(argv)?.args)
}

/// The stages `remote test window` runs: the agent serves them (no nested compositor, fake input method, pulse sink or full picture bytes on the target).
const REMOTE_STAGES: [Stage; 3] = [Stage::Frames, Stage::Widgets, Stage::UiBlock];
/// atld's limit on the target: the live half's [`LIMIT`] and a margin for the stop.
const REMOTE_LIMIT: Duration = Duration::from_secs(LIMIT.as_secs() + 60);
/// How long the agent waits for the run script to start atld (its pid file).
const PID_WAIT: Duration = Duration::from_secs(30);

/// What the agent hands the judge on the build host (`live.json`): the [`LiveOut`] fields the remote stages read. `ended`/`stopped` are flags: the agent is not atld's parent, so the status is the run
/// script's `atld.rc`.
#[derive(serde::Serialize, serde::Deserialize)]
struct LiveRecord {
    liveness: Result<Liveness, String>,
    captured: capture::Seen,
    ended: bool,
    stopped: bool,
    pressure: Samples,
    presented: Option<(u64, u64)>,
}

/// Hidden `window-live`: the window gate's live half on a remote target, started by the run script (remote.rs `Plan::script`) before atld.
#[derive(clap::Args)]
pub struct AgentArgs {
    /// The stage's work dir (`<bundle>/out/window`): atld's root and scratch below it.
    #[arg(long)]
    work: PathBuf,
    /// The fixture's package and launcher activity.
    #[arg(long)]
    package: String,
    #[arg(long)]
    activity: String,
    /// The compositor's Wayland socket the proxy connects to (UI stages); absent: no proxy.
    #[arg(long, requires = "listen")]
    upstream: Option<PathBuf>,
    /// The proxy's socket, atld's `WAYLAND_DISPLAY`.
    #[arg(long, requires = "upstream")]
    listen: Option<PathBuf>,
    /// After `--`: the run's `test window` args (stage and flags).
    #[arg(last = true)]
    window: Vec<String>,
}

/// The agent: binds the proxy (the run script waits for its socket, then starts atld with it), reads atld's pid from the script, runs [`live`] as `test window` does, writes `live.json`.
pub fn agent(a: &AgentArgs) -> Result<ExitCode> {
    let args = args_of(&a.window)?;
    let run = Run {
        root: a.work.join("root"),
        scratch: a.work.join("scratch"),
        launch: None,
        packages: vec![],
        libraries: vec![],
        shell: None,
        log_tags: String::new(),
    };
    let proxy = match (&a.upstream, &a.listen) {
        (Some(upstream), Some(listen)) => Some(Proxy::start(upstream, listen)?),
        _ => None,
    };
    let pid_file = run.scratch.join(crate::remote::ATLD_PID);
    let deadline = Instant::now() + PID_WAIT;
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|t| t.trim().parse::<u32>().ok())
        {
            break pid;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no atld pid in {} within {PID_WAIT:?}",
            pid_file.display()
        );
        std::thread::sleep(POLL);
    };
    println!("window-live: atld pid {pid}, stage {}", args.stage.name());
    let sampler = Sampler::start();
    let mut atld = Atld::Pid(pid);
    let out = live(
        LiveIn {
            args: &args,
            run: &run,
            package: &a.package,
            activity: &a.activity,
            work: &a.work,
            proxy,
            nested: None,
            fake_ime: None,
            ime_breaks: ime::Breaks::default(),
            started: Instant::now(),
        },
        &mut atld,
        sampler,
    )?;
    let record = LiveRecord {
        liveness: out.liveness,
        captured: std::mem::take(
            &mut *out
                .captured
                .lock()
                .map_err(|_| anyhow::anyhow!("pictures poisoned"))?,
        ),
        ended: out.ended.is_some(),
        stopped: out.stopped.is_some(),
        pressure: out.pressure,
        presented: out.presented,
    };
    let path = run.scratch.join(crate::remote::LIVE_JSON);
    std::fs::write(&path, serde_json::to_vec(&record)?)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("window-live: wrote {}", path.display());
    Ok(ExitCode::SUCCESS)
}

/// Hidden `display-probe`: this host's first monitor as atld's `--display` value, on the line remote.rs `Target::display_model` reads. The monitor's own model (DisplayModel::from_monitor: size, rate,
/// density 160 x scale), as atld takes it without `--display` (atld main.rs `host_display`) and as the app's window reports it on entry (atl-composer ui_window.rs `on_monitor`, registry SET_DISPLAY).
/// Never the gates' fixed display ([`run_display`]): the window opens on this monitor, so a fixed value would be the app's first configuration only until the window's report replaced it -- the first
/// layout at the fixed density, then a relaunch at the monitor's (measured on a scale-1.6 laptop: label left=16, then 26). No desktop or no monitor: an error, no default.
pub fn display_probe() -> Result<ExitCode> {
    let m = atl_platform_host::first_monitor()
        .map_err(|e| anyhow::anyhow!("display-probe: {}", e.0))?
        .context("display-probe: the desktop lists no monitor")?;
    let model = DisplayModel::from_monitor(
        m.size.width,
        m.size.height,
        m.refresh_hz,
        m.scale.get(),
        m.rotation.quarter_turns(),
    );
    println!("{}{}", crate::remote::DISPLAY_PROBE_LINE, model.to_arg());
    Ok(ExitCode::SUCCESS)
}

/// `xtask remote test window --stage <s>` (remote.rs `test`): the stage built and recorded here, pushed and checked on the target, its live half run there by the agent ([`agent`]) beside atld, its
/// logs and `live.json` judged here by the same [`judge`] as `test window`. UI stages open the window on the target's session display where the target has no labwc (maintainer decision D1, 2026-11-18:
/// on an empty workspace, remote.rs `workspace_script`) and are judged against that display ([`Rate::Session`]); where it has labwc they are refused until they run on the run's own labwc (decision 2026-11-23).
/// Frames runs headless on the 60 Hz grid as locally.
pub fn remote(root: &Path, a: &crate::remote::TestArgs) -> Result<ExitCode> {
    use crate::remote::{AGENT_LOG, AGENT_RC, Agent, Display, LIVE_JSON, REMOTE_DIR, Target};
    use std::os::unix::process::ExitStatusExt;
    anyhow::ensure!(
        a.target_arch.arch.is_none() && a.fixture_arch.is_none(),
        "--arch and --fixture-arch are `app` options"
    );
    let stage_name = a
        .stage
        .as_deref()
        .context("remote test window needs --stage (frames, widgets or ui-block)")?;
    let arches = crate::arches::load(root)?;
    let (arch, _) = crate::arches::build_machine(root, &arches)?;
    let mut words: Vec<String> = ["--host", arch, "--stage", stage_name]
        .map(str::to_owned)
        .to_vec();
    if let Some(out) = &a.out {
        words.extend(["--out".to_owned(), out.display().to_string()]);
    }
    let args = args_of(&words)?;
    anyhow::ensure!(
        REMOTE_STAGES.iter().any(|s| s.name() == args.stage.name()),
        "remote test window: stage {stage_name} is not remotable (frames, widgets, ui-block)"
    );
    let ui = !matches!(args.stage, Stage::Frames);
    // frames too takes the session's socket when the target has no labwc, without `--ui` (no window): atld's renderer probe mounts the compositor socket even headless, and a local run inherits the desktop's
    let (kind, why) = crate::remote::probe_test_display(&a.host)?;
    let kind = match kind {
        Display::Session => kind,
        Display::NestedX11 | Display::NestedHeadless if ui => anyhow::bail!(
            "remote test window --stage {stage_name}: the target has {why}; the window gate runs only on a session display"
        ),
        Display::Headless if ui => anyhow::bail!(
            "remote test window --stage {stage_name}: no display on the target: {why}"
        ),
        Display::NestedX11 | Display::NestedHeadless | Display::Headless => Display::Headless,
    };
    let key = work_key(&args);
    let rt = Runtime::new(
        root,
        "window",
        &args.host,
        args.out.as_deref(),
        args.java_out.as_deref(),
        &key,
        args.verbose,
    )?
    .with_gralloc(root)?
    .with_gl_driver(root)?
    .with_native_bridge_guest(&args.host)?;
    anyhow::ensure!(
        xtask::native::cross_host_of(root, &rt.out)?.is_none(),
        "remote test window: a cross host's out dir; the agent is the build machine's harness binary (its ISA)"
    );
    let rt = if ui { rt.with_ui("gtk") } else { rt };
    let rt = match &a.ui_break {
        Some(rule) => rt.with_ui_break(root, &format!("atl-composer/break-{rule}"))?,
        None => rt,
    };
    let rt = rt.with_app_env(&[format!("GDK_DEBUG={}", crate::gdk_dmabuf::GDK_DEBUG)]);
    let work = rt.out.join(&key);
    let apk = rt.apk.build(
        &root.join(fixture(args.stage)),
        &work.join("apk"),
        &[],
        None,
        None,
    )?;
    let m = manifest::read(&rt.apk.aapt2, &apk)?;
    let activity = m
        .launch_activity()
        .map(|a| a.name.clone())
        .context("the fixture has no launcher activity")?;
    let app_uid = rt.first_app_uid();
    let run = Run {
        root: work.join("root"),
        scratch: work.join("scratch"),
        launch: Some(m.name.clone()),
        packages: vec![Package::base(apk)],
        libraries: vec![],
        shell: None,
        log_tags: args.log_tags.clone(),
    };
    rt.record_inputs(&run)?;
    let target = Target::of(root, &rt.out, "window", &a.host, a.brk)?;
    if !a.apply {
        println!("{}", target.summary());
        println!(
            "dry run: pushes the bundle to {}, checks {} ELF files there, then runs stage {stage_name} {} with its live half on the target; --apply runs it",
            target.target(),
            target.elf_count(),
            if ui {
                "on the session display (an empty workspace where the compositor has them)"
            } else {
                "headless"
            }
        );
        return Ok(ExitCode::SUCCESS);
    }
    let (pushed, problems) = target.deliver("window")?;
    if !problems.is_empty() {
        println!("{}", crate::verdict::gate("remote test window", &problems));
        return Ok(ExitCode::FAILURE);
    }
    let harness = rt.harness_exe()?;
    let display = if ui {
        let arg = target.display_model(&rt.roots(), &harness)?;
        println!("remote test window: target display {arg}");
        DisplayModel::parse_arg(&arg).map_err(anyhow::Error::msg)?
    } else {
        GATE_OUTPUT.display()
    };
    let dir = rt.out.join(REMOTE_DIR).join(target.log_dir());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let log = dir.join("window.log");
    std::fs::write(&log, "").with_context(|| format!("writing {}", log.display()))?;
    let stage_word =
        |s: Stage| clap::ValueEnum::to_possible_value(&s).map(|v| v.get_name().to_owned());
    let agent = Agent {
        program: harness,
        args: vec![
            "window-live".to_owned(),
            "--work".to_owned(),
            work.display().to_string(),
            "--package".to_owned(),
            m.name.clone(),
            "--activity".to_owned(),
            activity.clone(),
        ],
        window: vec![
            "--host".to_owned(),
            args.host.clone(),
            "--stage".to_owned(),
            stage_word(args.stage).context("stage name")?,
        ],
        proxy: ui,
    };
    let rt = rt.with_display(display.to_arg()).with_site(
        target
            .run_site(&pushed, REMOTE_LIMIT, log.clone(), kind)?
            .with_agent(agent),
    );
    for f in [LIVE_JSON, AGENT_RC, AGENT_LOG] {
        let _ = std::fs::remove_file(run.scratch.join(f));
    }
    let start_ms = now_ms();
    let mut child = rt.start_atld(&run)?;
    // the transfer ends with the run: atld's limit, the agent's margin, the tar
    let deadline = Instant::now() + REMOTE_LIMIT + Duration::from_secs(180);
    let end = loop {
        if let Some(end) = child.try_wait()? {
            break end;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(POLL);
    };
    let transcript = std::fs::read_to_string(&log).unwrap_or_default();
    for line in transcript.lines().filter(|l| l.starts_with("workspace: ")) {
        println!("remote test window: {line}");
    }
    println!("remote test window: transcript {}", log.display());
    rt.check_end(end, &run)?;
    let read_trim =
        |name: &str| std::fs::read_to_string(run.scratch.join(name)).map(|t| t.trim().to_owned());
    let agent_rc = read_trim(AGENT_RC).map_err(|_| {
        anyhow::anyhow!(
            "lost: no {AGENT_RC} came back (the transfer ended {end}); rerun, see {}",
            log.display()
        )
    })?;
    if agent_rc != "0" {
        println!(
            "---- agent log\n{}",
            tail(&read_trim(AGENT_LOG).unwrap_or_default(), TAIL)
        );
        anyhow::bail!("the window agent on the target exited {agent_rc}");
    }
    let record: LiveRecord = serde_json::from_str(&read_trim(LIVE_JSON)?)
        .with_context(|| format!("reading {}", run.scratch.join(LIVE_JSON).display()))?;
    // atld's own status: the run script's record (the agent is not its parent)
    let rc: i32 = read_trim("atld.rc")?.parse().context("atld.rc")?;
    let status = std::process::ExitStatus::from_raw((rc & 0xff) << 8);
    let out = LiveOut {
        liveness: record.liveness,
        captured: Arc::new(Mutex::new(record.captured)),
        app_ids: Arc::default(),
        titles: Arc::default(),
        splash_shot: None,
        ended: record.ended.then_some(status),
        stopped: record.stopped.then_some(status),
        pressure: record.pressure,
        presented: record.presented,
        // remote stages do not throttle (slow-display is local)
        throttle: (0, 0),
        // the target's clock: the commits sub-gate is local only
        commits: None,
        // the tiled stage is local: a target's own compositor sizes its window
        tiled: 0,
    };
    judge(
        &Judged {
            root,
            args: &args,
            run: &run,
            package: &m.name,
            display,
            rate: if ui { Rate::Session } else { Rate::Fixed },
            moved: None,
            app_uid,
            start_ms,
        },
        out,
        None,
    )
}

/// Stage hwui: the run's `LD_DEBUG=bindings` records against launcher-closure.toml (xtask launcher_closure.rs `judge_bindings`): app/AOSP/ours objects bind host only to glibc, the C++ runtime and the declared system bindings; host objects bind only listed AOSP sonames, never a forbidden one.
/// The O4 count per soname is judged against its row's ceiling (5 runs measured identical, spread 0) and printed in full.
fn bindings_check(root: &Path, app: &str, forget: Option<&str>) -> Result<String, String> {
    use xtask::launcher_closure::{judge_bindings, load, parse_bindings};
    let mut table = load(root).map_err(|e| format!("{e:#}"))?;
    if let Some(s) = forget {
        table.reverse.retain(|r| r.to != s);
    }
    // the launcher executable exports only the sigaction interposer (atl-sigaction-hook): its row must be symbol-scoped to those names
    let launcher = xtask::launcher_closure::LAUNCHER;
    if let Some(r) = table.reverse.iter().find(|r| r.to == launcher) {
        let ok = r.symbols.as_ref().is_some_and(|s| {
            s.iter()
                .all(|x| atl_sigaction_hook::INTERPOSED.contains(&x.as_str()))
        });
        if !ok {
            return Err(format!(
                "reverse row {launcher}: `symbols` must be a subset of atl-sigaction-hook INTERPOSED {:?}",
                atl_sigaction_hook::INTERPOSED
            ));
        }
    }
    let records = parse_bindings(app);
    // an unresponsive probe (LD_DEBUG not reaching the app) would otherwise pass with nothing judged
    if records.is_empty() {
        return Err("no LD_DEBUG=bindings records in the app log".into());
    }
    let j = judge_bindings(&table, &records, crate::ui_module::MODULE_FILE);
    println!(
        "window: info reverse bindings (O4) {} records, {} unique, by soname {:?}",
        j.reverse_records, j.reverse_unique, j.reverse_by_to
    );
    if j.failures.is_empty() {
        Ok(format!(
            "{} records; {} declared host bindings; {} reverse sonames, all listed",
            records.len(),
            j.declared,
            j.reverse_by_to.len()
        ))
    } else {
        Err(format!(
            "{} problems: {}",
            j.failures.len(),
            j.failures.join("; ")
        ))
    }
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

    /// Lines as the app log has them (step1-webview run, 2026-11-22): the module's attach record, then ART's first libbase record.
    const ATTACH: &str =
        "1795287835.463  INFO atl-platform-ui: module attached: GTK 4.18.6 on thread atl-display";
    const ART: &str = "11-22 03:03:55.849     2     2 I atl-app-launcher: parsed_options.cc:697 setting boot class path to /system/framework/core-oj.jar";

    #[test]
    fn startup_order_passes_only_with_the_attach_on_the_display_thread_before_art() {
        assert!(startup_order_check(&format!("{ATTACH}\n{ART}\n")).is_ok());
        assert!(startup_order_check(&format!("{ART}\n{ATTACH}\n")).is_err());
        let on_looper = ATTACH.replace("on thread atl-display", "on the process's Looper");
        assert!(startup_order_check(&format!("{on_looper}\n{ART}\n")).is_err());
        assert!(startup_order_check(&format!("{ATTACH}\n")).is_err());
        // a record with a `.cc` word but no line number is not ART's
        assert!(!is_art_line("diag: first prop:miss log.tag.foo.cc: 1"));
    }

    /// A counters line with the release fields `release_check` reads.
    fn release_log(released: u64, held: u64, fenced: u64, unmatched: u64) -> String {
        format!(
            "1.0 INFO composer: counters ticks=5 latches=5 backpressure_same_thread=1 buffers_released={released} releases_fenced={fenced} \
             releases_fenced_unmatched={unmatched} releases_carried=0 releases_held={held} scenes_dropped=1\n"
        )
    }

    /// The failure seen in check runs (2026-11-19/20; out/surface-view-release-flake alone-2): the only release was the buffer of a scene a same-thread latch replaced untaken
    /// (`releases_held=0`, `scenes_dropped=1`), released at once with its acquire fence. Nothing was read, so nothing is wrong -- and nothing was judged: not judged, never a pass.
    #[test]
    fn release_of_a_never_shown_buffer_is_not_judged() {
        assert_eq!(release_check(&release_log(1, 0, 0, 0)), Ok(None));
    }

    /// A shown buffer retired for the paint whose release carried no read fence, and a paint's fence with no release to ride (`--break-ignore-hold`), each fail; a counters line without the
    /// release fields is an error, not zeros.
    #[test]
    fn release_of_a_shown_buffer_needs_its_read_fence() {
        assert!(matches!(
            release_check(&release_log(1, 1, 1, 0)),
            Ok(Some(_))
        ));
        assert!(release_check(&release_log(1, 1, 0, 0)).is_err());
        assert!(release_check(&release_log(1, 0, 0, 1)).is_err());
        assert!(release_check("composer: counters ticks=1\n").is_err());
        assert!(release_check("no counters\n").is_err());
    }

    /// A stage that replaces a shown buffer fails when nothing was judged; a static-frame stage names it apart (`None` = not judged).
    #[test]
    fn not_judged_fails_only_stages_that_replace_a_shown_buffer() {
        let idle = release_log(1, 0, 0, 0);
        assert!(matches!(
            release_sub(&idle, Stage::SurfaceView),
            Some(Err(_))
        ));
        assert!(matches!(release_sub(&idle, Stage::Widgets), Some(Err(_))));
        assert_eq!(release_sub(&idle, Stage::Gtk), None);
        assert!(matches!(
            release_sub(&release_log(1, 1, 1, 0), Stage::Gtk),
            Some(Ok(_))
        ));
    }

    /// A run's vsync lines: the platform's clock line, the tier lines (`start` at the clock's start, `switch` at a change), the last counters.
    fn vsync_log(
        clock: &str,
        switch: &[&str],
        tier: &str,
        vsync: u64,
        other: u64,
        changes: u64,
    ) -> String {
        let mut log = format!(
            "atl-platform-ui: wp_presentation clock_id={clock}\n\
             atl-composer: vsync source free-running (no vsync presentation sample yet) period_ns=8333333\n"
        );
        for t in switch {
            log += &format!("atl-composer: vsync source {t} (a reason) refresh_ns=8333333\n");
        }
        log += &format!(
            "composer: counters ticks=9 vsync_source={tier} vsync_samples={vsync} vsync_samples_other={other} vsync_source_changes={changes} ignored=\n"
        );
        log
    }

    /// Both tiers green from their own lines; the reds: flip-flop, vsync samples without the switch, a switch without samples, mixed kinds, counters and tier line disagreeing, vsyncs on another clock.
    /// Sample counts: 6 = AOSP kMinSamplesForPrediction; 14850 / 0 / 1 = the laptop's measured hardware run (state vsync-source-tier-laptop); 1519 other = the WSLg run (clock_id=4, no vsync flag).
    #[test]
    fn vsync_tier_follows_the_hosts_feedback() {
        // an honest composer: the proxy saw the same samples it counted
        let check = |log: &str| {
            let n = |k: &str| {
                crate::composer::last_counters(log)
                    .and_then(|l| crate::composer::field(l, k))
                    .unwrap_or(0)
            };
            vsync_tier_check(log, Some((n("vsync_samples"), n("vsync_samples_other"))))
        };
        let hw = vsync_log("1", &["hardware-vsync"], "hardware-vsync", 14850, 0, 1);
        assert!(check(&hw).is_ok_and(|s| s.starts_with("hardware-vsync (")));
        let free = vsync_log("4", &[], "free-running", 0, 1519, 0);
        assert!(check(&free).is_ok_and(|s| s.starts_with("free-running (")));
        // the threshold itself: 6 vsyncs switch, 5 do not
        assert!(
            check(&vsync_log(
                "1",
                &["hardware-vsync"],
                "hardware-vsync",
                6,
                0,
                1
            ))
            .is_ok()
        );
        assert!(check(&vsync_log("1", &[], "free-running", 5, 0, 0)).is_ok());
        // review-2 G5: the judge is the proxy's count. A composer that counts nothing (0/0) or reads every flag as not vsync (`--break-ignore-vsync-flag`) stays free-running: red against
        // the laptop's 14850 vsync-flagged feedbacks; green only where the host flagged none (no proxy = no feedback)
        let silent = vsync_log("1", &[], "free-running", 0, 0, 0);
        assert!(vsync_tier_check(&silent, Some((14850, 0))).is_err());
        let ignoring = vsync_log("1", &[], "free-running", 0, 14850, 0);
        assert!(vsync_tier_check(&ignoring, Some((14850, 0))).is_err());
        assert!(vsync_tier_check(&silent, Some((0, 1519))).is_ok());
        assert!(vsync_tier_check(&silent, None).is_ok());
        let reds = [
            vsync_log(
                "1",
                &["hardware-vsync", "free-running", "hardware-vsync"],
                "hardware-vsync",
                14850,
                0,
                3,
            ),
            vsync_log("1", &[], "free-running", 14850, 0, 0),
            vsync_log("4", &["hardware-vsync"], "hardware-vsync", 0, 1519, 1),
            vsync_log("1", &["hardware-vsync"], "hardware-vsync", 14850, 7, 1),
            vsync_log("1", &[], "hardware-vsync", 14850, 0, 1),
            vsync_log("4", &["hardware-vsync"], "hardware-vsync", 14850, 0, 1),
        ];
        for log in &reds {
            assert!(check(log).is_err(), "{log}");
        }
        assert!(vsync_tier_check("composer: counters ticks=1\n", None).is_err());
    }

    /// The app's sandbox and launcher match by an argument's file name; the services' sandbox, binding the runtime's lib dir under `crates/atl-app-launcher`, does not.
    #[test]
    fn launcher_by_argument_file_name() {
        assert!(runs_launcher(
            b"/o/cargo-converter/release/atl-app-launcher\0--app\0"
        ));
        assert!(runs_launcher(
            b"bwrap\0--ro-bind\0/a\0/a\0/o/release/atl-app-launcher\0-cp\0"
        ));
        assert!(!runs_launcher(
            b"bwrap\0--ro-bind\0/o/crates/atl-app-launcher/lib\0/l\0/o/atl-system-services\0"
        ));
    }

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

    /// An unserved relayoutAsync fails its sub-gate: the resize's frames never reach the client. Served (no counter line) passes.
    #[test]
    fn relayout_async_fails_on_an_unserved_call() {
        assert!(relayout_async_check(RESIZE_SERVICES).is_ok());
        let unserved = format!(
            "{RESIZE_SERVICES}diag: first unimplemented IWindowSession.relayoutAsync 1 at=1794093338.966\n"
        );
        assert!(relayout_async_check(&unserved).is_err());
    }

    /// `--output` takes `WxH` of two positive sides.
    #[test]
    fn output_size_is_wxh() {
        assert_eq!(parse_output_size("2560x1440"), Ok((2560, 1440)));
        assert!(parse_output_size("2560").is_err());
        assert!(parse_output_size("0x1440").is_err());
    }

    /// AOSP's portrait size on the gate outputs, by hand: 1920x1200 has aspect 1.6, so the width is 1200 / 1.6 = 750 (+0.5, truncated), and the ideal height is 1200 x 0.75 = 900.
    /// 2560x1440 has aspect 16/9: width 1440 / (16/9) = 810, ideal height 1440 x 0.75 = 1080.
    #[test]
    fn portrait_expected_is_aosp_on_the_gate_outputs() {
        assert_eq!(portrait_expected((1920, 1200)), (750, 900));
        assert_eq!(portrait_expected((2560, 1440)), (810, 1080));
    }

    /// A run that opened landscape and ended at the portrait size, told once, passes; a run left landscape (no reshape) or relaunched fails the sub-gate that names it.
    #[test]
    fn orientation_checks_want_the_reshape_and_the_change() {
        let display = DisplayModel {
            width: 1920,
            height: 1200,
            dpi: 160,
            ..DisplayModel::default()
        };
        // opening 1440x900 (3/4 of 1920x1200); portrait 750x900 (portrait_expected); 750 px at 160 dpi = 750 dp
        let layout = |w: i32, h: i32, o: i32| {
            format!(
                "AtlWindow: size={w}x{h} screenWidthDp={w} window={w}x{h} display=1920x1200 orientation={o}\n"
            )
        };
        let requested = "AtlWindow: requested orientation set=1 get=1\n";
        let created = "AtlWindow: onCreate reached\n";
        let told = "AtlWindow: configuration changed screenWidthDp=750 orientation=1\n";
        // the process follows the activity: 1440x900 dp landscape at onCreate, 750x900 dp portrait after the reshape (160 dpi: dp = px)
        let process = |when: &str, w: i32, h: i32, o: i32| {
            format!(
                "AtlWindow: process configuration {when} app={w}x{h} appOrientation={o} system={w}x{h} systemOrientation={o}\n"
            )
        };
        let (proc_created, proc_changed) = (
            process("created", 1440, 900, 2),
            process("changed", 750, 900, 1),
        );
        // the display's 160 dpi; an unanswered transaction reads 0 in the AIDL proxy
        let initial = "AtlWindow: initial display density=160\n";
        let good = format!(
            "{created}{proc_created}{initial}{requested}{}{told}{proc_changed}{}",
            layout(1440, 900, 2),
            layout(750, 900, 1)
        );
        let failed = |app: &str| -> Vec<&'static str> {
            orientation_checks(app, display)
                .into_iter()
                .filter(|(_, r)| r.is_err())
                .map(|(n, _)| n)
                .collect()
        };
        assert!(
            failed(&good).is_empty(),
            "{:?}",
            orientation_checks(&good, display)
        );
        let kept = format!(
            "{created}{proc_created}{initial}{requested}{}",
            layout(1440, 900, 2)
        );
        assert_eq!(failed(&kept), vec!["reshaped", "told", "process-reshaped"]);
        let relaunched = format!("{good}{created}");
        assert_eq!(failed(&relaunched), vec!["told"]);
        let ignored = good.replace("get=1", "get=-1");
        assert_eq!(failed(&ignored), vec!["requested"]);
        // the process kept the display's configuration (1920x1200 dp): both process sub-gates fail, the activity's pass
        let display_process = good
            .replace(&proc_created, &process("created", 1920, 1200, 2))
            .replace(&proc_changed, &process("changed", 1920, 1200, 2));
        assert_eq!(
            failed(&display_process),
            vec!["process-opened", "process-reshaped"]
        );
        // Resources.getSystem() left behind while the Application followed: still a failure
        let system_stale = good.replace(
            "system=750x900 systemOrientation=1",
            "system=1920x1200 systemOrientation=2",
        );
        assert_eq!(failed(&system_stale), vec!["process-reshaped"]);
        let unanswered = good.replace("density=160", "density=0");
        assert_eq!(failed(&unanswered), vec!["initial-density"]);
        let never_asked = good.replace(initial, "");
        assert_eq!(failed(&never_asked), vec!["initial-density"]);
    }

    /// Every layout must be the portrait size as content and as window frame, on the gate's display; one layout at another size fails the sub-gate.
    #[test]
    fn layout_check_wants_the_portrait_frame_at_every_layout() {
        let layout = |size: &str| {
            format!("AtlWindow: size={size} screenWidthDp=750 window={size} display=1920x1200")
        };
        let good = format!("{}\n{}\n", layout("750x900"), layout("750x900"));
        assert!(layout_check(&good, (750, 900), (1920, 1200)).is_ok());
        let relaid = format!("{good}{}\n", layout("1920x1200"));
        assert!(layout_check(&relaid, (750, 900), (1920, 1200)).is_err());
        assert!(layout_check("no layout line\n", (750, 900), (1920, 1200)).is_err());
    }

    /// The app id is the last `set_app_id` sent before the first commit: GTK's default before it does not count, an id sent after the first commit is too late (the old path: set after `present`, one commit late), a missing prefix fails, and no commit, no id or a dead probe fails.
    /// The label, then the task label, last: pass. The LayoutParams title (the old path), no task label, a task label not last, or the task label alone fails.
    #[test]
    fn title_check_wants_the_label_then_the_task_label() {
        let ok = Ok(Liveness::default());
        let t = |titles: &[&str]| titles.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let check = |titles: &[&str]| title_check(&t(titles), FIXTURE_LABEL, TASK_LABEL, &ok);
        assert!(check(&["com.example.atlwindow", FIXTURE_LABEL, TASK_LABEL]).is_ok());
        assert!(check(&[WINDOW_TITLE]).is_err());
        assert!(check(&["com.example.atlwindow", FIXTURE_LABEL]).is_err());
        assert!(check(&[FIXTURE_LABEL, TASK_LABEL, WINDOW_TITLE]).is_err());
        assert!(check(&[TASK_LABEL]).is_err());
        assert!(
            title_check(
                &t(&[FIXTURE_LABEL, TASK_LABEL]),
                FIXTURE_LABEL,
                TASK_LABEL,
                &Err("dead".into())
            )
            .is_err()
        );
    }

    #[test]
    fn app_id_check_reads_the_last_set_app_id_before_the_first_commit() {
        let ok: Result<Liveness, String> = Ok(Liveness::default());
        let want = "atl.com.example.atlwindow";
        let seen = |before: &[&str], after: &[&str]| AppIdsSeen {
            before_first_commit: before.iter().map(|id| id.to_string()).collect(),
            after_first_commit: after.iter().map(|id| id.to_string()).collect(),
            first_commit: true,
        };
        assert!(app_id_check(&seen(&["GTK Application", want], &[]), want, &ok).is_ok());
        assert!(app_id_check(&seen(&[want, "GTK Application"], &[]), want, &ok).is_err());
        assert!(app_id_check(&seen(&["GTK Application"], &[want]), want, &ok).is_err());
        assert!(app_id_check(&seen(&["com.example.atlwindow"], &[]), want, &ok).is_err());
        assert!(app_id_check(&seen(&[], &[]), want, &ok).is_err());
        let no_commit = AppIdsSeen {
            first_commit: false,
            ..seen(&[want], &[])
        };
        assert!(app_id_check(&no_commit, want, &ok).is_err());
        assert!(
            app_id_check(
                &seen(&[want], &[]),
                want,
                &Err("the fixture never drew".into())
            )
            .is_err()
        );
    }

    /// The counters before the marker are totals; the calls from it on keep their offsets.
    #[test]
    fn calls_split_at_the_marker() {
        let counter = |count| Counter {
            outcome: "unimplemented".into(),
            api: "IA.m".into(),
            count,
            exact: false,
        };
        let ev = |at_ms, count| Event {
            at_ms,
            counter: counter(count),
            cause: None,
        };
        let all = [ev(900, 1), ev(1_000, 2), ev(1_004, 4)];
        assert_eq!(before(&all, 1_000), [counter(1)]);
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

    /// The picture the probe records at `w`x`h` when the fixture has drawn the quadrants there (the fixture's draw, quadrant_pixels).
    fn quad(w: u32, h: u32) -> Result<capture::Shot, String> {
        let rgba = quadrant_pixels(w, h);
        Ok(capture::Shot {
            size: (w, h),
            digest: fnv1a64(&rgba),
            rgba,
        })
    }

    /// The services' log of a Task opened at 4x2 resized to 6x4 (grow) and 3x1 (shrink). A Task opens at its opening size, so only the changes are logged.
    const RESIZE_SERVICES: &str = "services: task 1 of uid 10123 bounds 6x4\nservices: resized 1 windows of task 1\nservices: task 1 of uid 10123 bounds 3x1\nservices: resized 1 windows of task 1";

    /// The named sub-gate is present and failed.
    fn failed(sub: &[Sub], name: &str) -> bool {
        sub.iter().any(|(n, r)| *n == name && r.is_err())
    }

    fn resize_run(
        app: &[String],
        services: &str,
        configured: Vec<bool>,
        pictures: Vec<Result<capture::Shot, String>>,
    ) -> Vec<Sub> {
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
            idle: None,
            minimized: false,
        });
        let seen = Mutex::new(capture::Seen {
            pictures,
            ..Default::default()
        });
        resize_checks(&app.join("\n"), services, display, &liveness, &seen)
    }

    /// The consistent run passes every sub-gate; each defect fails the sub-gate that names it. Sizes and pattern come from the spec, not from the code under test.
    #[test]
    fn resize_checks_pass_a_consistent_run_and_fail_each_defect() {
        let good = [layout(4, 2, 4), layout(6, 4, 6), layout(3, 1, 3)];
        let pictures = || vec![quad(6, 4), quad(3, 1)];
        let ok = resize_run(&good, RESIZE_SERVICES, vec![true, true], pictures());
        assert!(ok.iter().all(|(_, r)| r.is_ok()), "{ok:?}");

        let acks = resize_run(&good, RESIZE_SERVICES, vec![true, false], pictures());
        assert!(
            failed(&acks, "configure"),
            "a configure not acked: {acks:?}"
        );

        let services = RESIZE_SERVICES.replace("bounds 6x4", "bounds 4x4");
        let grow = resize_run(&good, &services, vec![true, true], pictures());
        assert!(
            failed(&grow, "bounds"),
            "grow not over the display: {grow:?}"
        );

        let no_grow = [layout(4, 2, 4), layout(3, 1, 3)];
        let relayout = resize_run(&no_grow, RESIZE_SERVICES, vec![true, true], pictures());
        assert!(
            failed(&relayout, "relayout"),
            "no layout at grow: {relayout:?}"
        );

        let stale = vec![
            quad(6, 4),
            Ok(capture::Shot {
                size: (3, 1),
                digest: 0,
                rgba: vec![0; 12],
            }),
        ];
        let frame = resize_run(&good, RESIZE_SERVICES, vec![true, true], stale);
        assert!(
            failed(&frame, "frame"),
            "shrink picture has other pixels: {frame:?}"
        );

        let unsettled = vec![quad(6, 4), Err("no two captures agreed".to_owned())];
        let frame = resize_run(&good, RESIZE_SERVICES, vec![true, true], unsettled);
        assert!(
            failed(&frame, "frame"),
            "shrink picture never settled: {frame:?}"
        );

        let moved = [
            layout(4, 2, 4),
            layout(6, 4, 6),
            layout(3, 1, 3).replace("display=6x3", "display=8x4"),
        ];
        let display = resize_run(&moved, RESIZE_SERVICES, vec![true, true], pictures());
        assert!(
            failed(&display, "display"),
            "the display moved with the window: {display:?}"
        );

        let same_dp = [layout(4, 2, 4), layout(6, 4, 4), layout(3, 1, 3)];
        let dp = resize_run(&same_dp, RESIZE_SERVICES, vec![true, true], pictures());
        assert!(
            failed(&dp, "display"),
            "screenWidthDp did not change with the grow: {dp:?}"
        );
    }

    /// Each Lifecycle against a log of what it wants, and the one defect each must catch. Sizes are arbitrary: grow (6, 4), shrink (3, 1).
    #[test]
    fn lifecycle_check_tells_the_three_lives_apart() {
        let steps: Result<Steps, String> = Ok(((6, 4), (3, 1)));
        let count = |name: &str, n: u32| format!("diag: count answered I.{name} {n} at=1.000\n");
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
            "{}diag: first answered {RELAUNCHED_COUNTER}1 at=1.000\n",
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

    /// Only a first draw in touch mode passes; out of touch mode (the defect: addWindow without ADD_FLAG_IN_TOUCH_MODE) and a missing line fail.
    #[test]
    fn touch_mode_check_needs_touch_mode_at_first_draw() {
        let line = |v: &str| format!("I AtlWindow: touch-mode first-draw={v}\n");
        assert!(touch_mode_check(&line("true")).is_ok());
        assert!(touch_mode_check(&line("false")).is_err());
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

    /// Callbacks one display period apart pass. A mean off by a fifth, too few callbacks, one missed vsync (one interval of two periods, the mean still within tolerance) and one interval closer than SurfaceFlinger dispatches fail.
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
            cadence_check(&refs, hz, Rate::Fixed, &Samples::default())
        };
        let session = |times: &[u64], hz: f32| {
            let owned = lines(times);
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            cadence_check(&refs, hz, Rate::Session, &Samples::default())
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
        // the grid moved 6 ms back once: one interval of 10.67 ms (< 16.67 - 3), the mean still within tolerance
        let rephased: Vec<u64> = (0..31)
            .map(|i| 5_000_000_000 + i * STEP - if i >= 15 { 6_000_000 } else { 0 })
            .collect();
        assert!(check(&rephased, 60.0).is_err(), "one interval of 10.67 ms");
        // a session display: a missed vsync and a slow mean are host-stated (VRR stretches intervals); too few callbacks and an interval under P - 3 ms still fail
        let stated = session(&missed, 60.0).expect("host-stated on a session display");
        assert!(stated.contains("host-stated mean="), "{stated}");
        assert!(
            session(&fast, 60.0).is_ok(),
            "20 ms intervals, slower than a 60 Hz max"
        );
        assert!(session(&short, 60.0).is_err());
        assert!(session(&rephased, 60.0).is_err());
        assert!(frame_time("1000.000 1 2 I AtlWindow: onDraw reached").is_none());
    }

    /// The starvation rule: a client that skips every other vsync (the stage's deliberate-skip case) FAILs on a quiet host and is undecidable on a starved one; steady callbacks
    /// PASS on both (a pass under load is a pass). Pressure samples bracket the window at the threshold's half and double.
    #[test]
    fn cadence_undecidable_only_when_starved() {
        use atl_host_load::{STARVED_STALL_SHARE, Sample};
        const STEP: u64 = 16_666_667;
        const T0: u64 = 5_000_000_000;
        let pressure = |share: f64| {
            let wall_ns = 2_000_000_000_i64;
            Samples(vec![
                Sample {
                    at_ns: T0 as i64 - 1,
                    stall_us: 0,
                    ..Default::default()
                },
                Sample {
                    at_ns: T0 as i64 - 1 + wall_ns,
                    stall_us: (share * wall_ns as f64 / 1e3) as u64,
                    ..Default::default()
                },
            ])
        };
        let run = |times: &[u64], p: &Samples| {
            let owned: Vec<String> = times
                .iter()
                .map(|t| format!("1000.000 1 2 I AtlWindow: frame 1 time={t}"))
                .collect();
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            cadence_check(&refs, 60.0, Rate::Fixed, p)
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

    /// A bounded stage's watch ends WINDOW_MS after its probe: a crash 1.9 s after the probe's end is still in the watched log and fails, one at 2.1 s comes after the stop and is not seen.
    /// The log is cut where the loop stops the apps (stop_apps), which is what the judge reads. The 1.9 / 2.1 s offsets straddle the 2 s grace (WINDOW_MS's own derivation) by 0.1 s.
    #[test]
    fn a_bounded_watch_sees_a_crash_inside_the_grace_only() {
        const T: u64 = 1_000_000;
        // the probe is done 6 s after the marker: 6 + 2 s ends before surface-view's 10 s watch
        let done = T + 6_000;
        let (end, why) = watch_end(T, GTK_WINDOW_MS, Some(done), false, true)
            .expect("an end once the probe is done");
        assert_eq!((end, why), (done + 2_000, WatchEnd::Probe));
        let mark = app_line(T, "liblog/AtlWindow", "onResume reached");
        let watched = |crash: u64| {
            let log =
                mark.clone() + &app_line(crash, "liblog/AndroidRuntime", "FATAL EXCEPTION: main");
            let cut: String = log
                .lines()
                .filter(|l| line_stamp(l).is_some_and(|t| t <= end))
                .map(|l| format!("{l}\n"))
                .collect();
            verdict(&cut, marker(Stage::Resume), false, GTK_WINDOW_MS)
                .fault
                .is_some()
        };
        assert!(watched(done + 1_900), "a crash inside the grace passed");
        assert!(!watched(done + 2_100), "a crash after the stop was judged");
    }

    /// The other ends: an unbounded stage keeps its window; a bounded one whose probe has not finished times out at the window; a probe-ended stage waits for its probe and may run past the window;
    /// a probe done right after the marker still leaves the resume path's WINDOW_MS.
    #[test]
    fn watch_end_cases() {
        const T: u64 = 1_000_000;
        let w = GTK_WINDOW_MS;
        assert_eq!(
            watch_end(T, w, Some(T + 1_000), false, false),
            Some((T + w, WatchEnd::Window))
        );
        assert_eq!(
            watch_end(T, w, None, false, true),
            Some((T + w, WatchEnd::Window))
        );
        // the probe ends 9 s in: 9 + 2 s is past the 10 s window, so the window ends it
        assert_eq!(
            watch_end(T, w, Some(T + 9_000), false, true),
            Some((T + w, WatchEnd::Window))
        );
        assert_eq!(watch_end(T, w, None, true, true), None);
        assert_eq!(watch_end(T, w, None, true, false), None);
        // ime-like: a relaunch that ends 15 s in runs to 17 s, past its window
        assert_eq!(
            watch_end(T, w, Some(T + 15_000), true, true),
            Some((T + 17_000, WatchEnd::Probe))
        );
        // unbounded probe-ended stage (aux-process): its probe's end, or the window, whichever is later
        assert_eq!(
            watch_end(T, w, Some(T + 15_000), true, false),
            Some((T + 15_000, WatchEnd::Probe))
        );
        assert_eq!(
            watch_end(T, w, Some(T + 3_000), true, false),
            Some((T + w, WatchEnd::Window))
        );
        // a probe done before the marker: the resume path's own WINDOW_MS after the marker
        assert_eq!(
            watch_end(T, w, Some(T - 500), false, true),
            Some((T + WINDOW_MS, WatchEnd::Probe))
        );
    }
}
